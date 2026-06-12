//! Full-corpus `RRSB` (`.rrb`) BM25 impact-sidecar builder over an `RRSR` record
//! store and its finished `.rrt` — out-of-core, resumable, built for the 484M-doc
//! OpenAlex corpus (~187M terms, ~40B term-doc pairs; the in-RAM
//! `ImpactsAccumulator` cannot hold that).
//!
//! Two phases:
//!  1. **Spill** — stream records in doc-ID chunks; per chunk, tokenize with the
//!     `.rrt`'s OWN tokenizer spec (read from its header, so the vocabularies
//!     cannot diverge), accumulate `(term → [(doc, tf)])`, and spill one
//!     term-sorted run file plus a doc-length file. Chunks resume by skipping
//!     existing run files. Text is `"<t> <ab>"` — exactly what
//!     `build_term_index_stream.py` fed the `.rrt` (title + abstract only).
//!  2. **Merge** — k-way merge the runs in term order, in LOCKSTEP with a
//!     streaming scan of the `.rrt` dictionary (`dict_block_locs` +
//!     `parse_dict_block`): every dictionary term must match the merge head
//!     exactly, or the build fails loudly. Impacts are quantized with the same
//!     `bm25::quantize_impact` the in-RAM writer uses and streamed to disk; the
//!     final `.rrb` is assembled header + sparse + entries + impacts.
//!
//! `--selftest` builds a synthetic corpus through BOTH paths (in-RAM
//! `write_impacts` vs this pipeline's run/merge/assembly with tiny chunks) and
//! asserts the outputs are byte-identical — the guard against serialization drift.
//!
//!   cargo run --release --features "terms zstd" --example build_impacts -- --selftest
//!   cargo run --release --features "terms zstd" --example build_impacts -- \
//!     records-full.idx records-full.bin openalex-full.dict \
//!     openalex-484m-stem.rrt out.rrb --tmp /tmp/rrb-work [--chunk-docs 4000000] [--limit N]

use futures::executor::block_on;
use rayon::prelude::*;
use roaringrange::bm25::{
    quantize_impact, DEFAULT_B, DEFAULT_K1, ENTRY_SIZE, HEADER_SIZE, MAGIC, SPARSE_STRIDE, VERSION,
};
use roaringrange::records::RecordStore;
use roaringrange::terms::parse_dict_block;
use roaringrange::{FileFetch, RangeFetch, TermIndex, Tokenizer};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Docs fetched per `get_many` wave (coalesced into a few big reads).
const FETCH_BATCH: usize = 8192;
/// Fetch waves handed to one rayon tokenize pass before merging into the chunk map.
const GROUP_BATCHES: usize = 64;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--selftest") {
        selftest();
        return;
    }
    if args.len() < 5 {
        eprintln!(
            "usage: build_impacts IDX BIN DICT RRT OUT [--tmp DIR] [--chunk-docs N] [--limit N]\n       build_impacts --selftest"
        );
        std::process::exit(2);
    }
    let (idx, bin, dict, rrt, out) = (&args[0], &args[1], &args[2], &args[3], &args[4]);
    let mut tmp = PathBuf::from("/tmp/rrb-work");
    let mut chunk_docs: u32 = 4_000_000;
    let mut limit: Option<u32> = None;
    let mut i = 5;
    while i < args.len() {
        match args[i].as_str() {
            "--tmp" => {
                tmp = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--chunk-docs" => {
                chunk_docs = args[i + 1].parse().expect("--chunk-docs");
                i += 2;
            }
            "--limit" => {
                limit = Some(args[i + 1].parse().expect("--limit"));
                i += 2;
            }
            a => panic!("unknown arg {a:?}"),
        }
    }
    std::fs::create_dir_all(&tmp).expect("create tmp dir");

    let t0 = Instant::now();
    let log = |msg: String| eprintln!("[{:7.0}s] {msg}", t0.elapsed().as_secs_f64());

    let store = block_on(RecordStore::open_with_dict(
        FileFetch::open(idx).expect("open idx"),
        FileFetch::open(bin).expect("open bin"),
        std::fs::read(dict).expect("read dict"),
    ))
    .expect("open record store");
    let rrt_idx = block_on(TermIndex::open(FileFetch::open(rrt).expect("open rrt"))).expect("rrt");
    let n_docs = limit.unwrap_or(store.len()).min(store.len());
    let spec = rrt_idx.tokenizer().spec();
    log(format!(
        "{} docs, {} dictionary terms, tokenizer {:?}; chunk={chunk_docs}, tmp={}",
        n_docs,
        rrt_idx.len(),
        spec,
        tmp.display()
    ));

    // Phase 1: spill term-sorted runs per doc chunk (resumable).
    let chunks = (n_docs as u64).div_ceil(chunk_docs as u64) as u32;
    for c in 0..chunks {
        let run = tmp.join(format!("run-{c:05}.spill"));
        let lens = tmp.join(format!("lens-{c:05}.spill"));
        if run.exists() && lens.exists() {
            continue;
        }
        let lo = c * chunk_docs;
        let hi = ((c as u64 + 1) * chunk_docs as u64).min(n_docs as u64) as u32;
        let (map, chunk_lens) = spill_chunk(&store, spec, lo, hi);
        write_run(&run, &map).expect("write run");
        write_lens(&lens, &chunk_lens).expect("write lens");
        log(format!(
            "chunk {}/{chunks}: {} docs, {} distinct terms spilled",
            c + 1,
            hi - lo,
            map.len()
        ));
    }
    log("phase 1 complete; merging against the dictionary scan".to_string());

    // Phase 2: streaming merge against the dictionary scan.
    merge(&rrt_idx, rrt, &tmp, chunks, n_docs, out, &log).expect("merge");
    log(format!("done: wrote {out}"));
}

/// The indexed text for one record: `"<t> <ab>"`, mirroring
/// `build_term_index_stream.py` (title + abstract only — NOT the trigram
/// builder's title+abstract+authors+venue; token sequences must match the `.rrt`).
fn record_text(bytes: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    format!("{} {}", g("t"), g("ab"))
}

type ChunkMap = BTreeMap<Vec<u8>, Vec<(u32, u8)>>;

/// Tokenizes one doc-ID chunk of records into `(term → [(doc, tf)])` plus per-doc
/// token counts. Records are fetched on the calling thread in coalesced waves
/// (the standing builder rule: never mix `block_on` fetches with CPU work inside
/// one rayon task); tokenization fans out over rayon with one tokenizer per
/// worker and merges in batch order, so per-term doc lists ascend by construction.
fn spill_chunk(
    store: &RecordStore<FileFetch>,
    spec: (Option<roaringrange::Language>, bool),
    lo: u32,
    hi: u32,
) -> (ChunkMap, Vec<u32>) {
    let mut map: ChunkMap = BTreeMap::new();
    let mut lens = vec![0u32; (hi - lo) as usize];
    let mut doc = lo;
    while doc < hi {
        let mut batches: Vec<(u32, Vec<Option<Vec<u8>>>)> = Vec::new();
        for _ in 0..GROUP_BATCHES {
            if doc >= hi {
                break;
            }
            let end = (doc + FETCH_BATCH as u32).min(hi);
            let ids: Vec<u32> = (doc..end).collect();
            let recs = block_on(store.get_many(&ids)).expect("get_many");
            batches.push((doc, recs));
            doc = end;
        }
        type DocTf = (u32, BTreeMap<Vec<u8>, u8>);
        let tokenized: Vec<(u32, Vec<DocTf>)> = batches
            .into_par_iter()
            .map_init(
                || Tokenizer::new(spec.0, spec.1),
                |tok, (base, recs)| {
                    let per_doc = recs
                        .into_iter()
                        .map(|rec| {
                            let text = rec.as_deref().map(record_text).unwrap_or_default();
                            let toks = tok.tokenize(&text);
                            let n = toks.len() as u32;
                            let mut tf: BTreeMap<Vec<u8>, u8> = BTreeMap::new();
                            for t in toks {
                                let e = tf.entry(t.into_bytes()).or_default();
                                *e = e.saturating_add(1);
                            }
                            (n, tf)
                        })
                        .collect();
                    (base, per_doc)
                },
            )
            .collect();
        for (base, per_doc) in tokenized {
            for (j, (n, tf)) in per_doc.into_iter().enumerate() {
                let doc_id = base + j as u32;
                lens[(doc_id - lo) as usize] = n;
                for (term, c) in tf {
                    map.entry(term).or_default().push((doc_id, c));
                }
            }
        }
    }
    (map, lens)
}

/// Spills one chunk's term-sorted run: per term `[len u32][term][count u32]`
/// then `count × ([doc u32][tf u8])`. Written to `.tmp` and renamed, so a file
/// that exists is complete (the resume contract).
fn write_run(path: &Path, map: &ChunkMap) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::new(File::create(&tmp)?);
    for (term, pairs) in map {
        w.write_all(&(term.len() as u32).to_le_bytes())?;
        w.write_all(term)?;
        w.write_all(&(pairs.len() as u32).to_le_bytes())?;
        for &(doc, tf) in pairs {
            w.write_all(&doc.to_le_bytes())?;
            w.write_all(&[tf])?;
        }
    }
    w.flush()?;
    std::fs::rename(&tmp, path)
}

/// Spills one chunk's per-doc token counts (u32 LE each).
fn write_lens(path: &Path, lens: &[u32]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut w = BufWriter::new(File::create(&tmp)?);
    for &n in lens {
        w.write_all(&n.to_le_bytes())?;
    }
    w.flush()?;
    std::fs::rename(&tmp, path)
}

/// A positioned reader over one run file: holds the current group's header; pairs
/// stream on demand.
struct RunReader {
    r: BufReader<File>,
    term: Vec<u8>,
    remaining: u32,
}

impl RunReader {
    fn open(path: &Path) -> io::Result<Option<Self>> {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
        match Self::read_header(&mut r)? {
            Some((term, count)) => Ok(Some(RunReader {
                r,
                term,
                remaining: count,
            })),
            None => Ok(None),
        }
    }

    fn read_header(r: &mut BufReader<File>) -> io::Result<Option<(Vec<u8>, u32)>> {
        let mut len4 = [0u8; 4];
        match r.read_exact(&mut len4) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut term = vec![0u8; u32::from_le_bytes(len4) as usize];
        r.read_exact(&mut term)?;
        let mut c4 = [0u8; 4];
        r.read_exact(&mut c4)?;
        Ok(Some((term, u32::from_le_bytes(c4))))
    }

    /// Streams the current group's `(doc, tf)` pairs through `f`.
    fn stream_pairs(&mut self, mut f: impl FnMut(u32, u8) -> io::Result<()>) -> io::Result<()> {
        let mut buf = [0u8; 5];
        for _ in 0..self.remaining {
            self.r.read_exact(&mut buf)?;
            f(u32::from_le_bytes(buf[0..4].try_into().unwrap()), buf[4])?;
        }
        self.remaining = 0;
        Ok(())
    }

    /// Advances to the next group header; `false` at end of run.
    fn advance(&mut self) -> io::Result<bool> {
        assert_eq!(self.remaining, 0, "advance before pairs were consumed");
        match Self::read_header(&mut self.r)? {
            Some((term, count)) => {
                self.term = term;
                self.remaining = count;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// K-way merges the runs in lockstep with the `.rrt` dictionary scan and writes
/// the final `.rrb` (header + sparse + entries + impacts), byte-compatible with
/// `bm25::write_impacts`.
#[allow(clippy::too_many_arguments)]
fn merge(
    rrt_idx: &TermIndex<FileFetch>,
    rrt_path: &str,
    tmp: &Path,
    chunks: u32,
    n_docs: u32,
    out: &str,
    log: &dyn Fn(String),
) -> io::Result<()> {
    // Doc lengths + avgdl, identical math to the in-RAM writer.
    let mut lens: Vec<u32> = Vec::with_capacity(n_docs as usize);
    for c in 0..chunks {
        let mut r =
            BufReader::with_capacity(1 << 20, File::open(tmp.join(format!("lens-{c:05}.spill")))?);
        let mut b = [0u8; 4];
        loop {
            match r.read_exact(&mut b) {
                Ok(()) => lens.push(u32::from_le_bytes(b)),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }
    }
    if lens.len() != n_docs as usize {
        return Err(io::Error::other(format!(
            "lens files cover {} docs, expected {n_docs}",
            lens.len()
        )));
    }
    let avgdl = lens.iter().map(|&n| n as u64).sum::<u64>() as f32 / n_docs as f32;
    let (k1, b) = (DEFAULT_K1, DEFAULT_B);
    let scale = k1 + 1.0;

    // Open every run; seed the heap with each run's first term.
    let mut runs: Vec<RunReader> = Vec::new();
    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::new();
    for c in 0..chunks {
        if let Some(rr) = RunReader::open(&tmp.join(format!("run-{c:05}.spill")))? {
            heap.push(Reverse((rr.term.clone(), runs.len())));
            runs.push(rr);
        }
    }

    // Dictionary scan via a dedicated fetch (block reads are sequential, off rayon).
    let locs = rrt_idx.dict_block_locs();
    let dfetch = FileFetch::open(rrt_path)?;

    let mut entries_w = BufWriter::new(File::create(tmp.join("entries.tmp"))?);
    let mut impacts_w = BufWriter::new(File::create(tmp.join("impacts.tmp"))?);
    let mut sparse: Vec<u64> = Vec::new();
    let mut term_i: u64 = 0;
    let mut impacts_len: u64 = 0;

    for &(off, len) in &locs {
        let block = block_on(RangeFetch::read(&dfetch, off, len))
            .map_err(|e| io::Error::other(format!("dict block read: {e:?}")))?;
        for (dterm, head_off) in parse_dict_block(&block) {
            let head = match heap.peek() {
                Some(Reverse((t, _))) => t,
                None => {
                    return Err(io::Error::other(format!(
                        "dictionary term {:?} has no run data — tokenizer/text mismatch?",
                        String::from_utf8_lossy(&dterm)
                    )))
                }
            };
            if *head != dterm {
                return Err(io::Error::other(format!(
                    "dictionary/run order diverged: dict {:?} vs runs {:?}",
                    String::from_utf8_lossy(&dterm),
                    String::from_utf8_lossy(head)
                )));
            }
            let mut group: Vec<usize> = Vec::new();
            while heap
                .peek()
                .map(|Reverse((t, _))| *t == dterm)
                .unwrap_or(false)
            {
                let Reverse((_, ri)) = heap.pop().unwrap();
                group.push(ri);
            }
            group.sort_unstable(); // run order == ascending doc ranges

            if term_i % SPARSE_STRIDE as u64 == 0 {
                sparse.push(head_off);
            }
            let rel = impacts_len;
            let mut card: u64 = 0;
            for ri in group {
                let rr = &mut runs[ri];
                rr.stream_pairs(|doc, tf| {
                    let dl = lens[doc as usize] as f32;
                    impacts_w.write_all(&[quantize_impact(tf as u32, dl, avgdl, k1, b)])?;
                    card += 1;
                    Ok(())
                })?;
                if rr.advance()? {
                    heap.push(Reverse((rr.term.clone(), ri)));
                }
            }
            entries_w.write_all(&head_off.to_le_bytes())?;
            entries_w.write_all(&rel.to_le_bytes())?;
            entries_w.write_all(&(card as u32).to_le_bytes())?;
            impacts_len += card;
            term_i += 1;
            if term_i % 10_000_000 == 0 {
                log(format!(
                    "merge checkpoint: {term_i} terms, {impacts_len} impact bytes"
                ));
            }
        }
    }
    if let Some(Reverse((t, _))) = heap.peek() {
        return Err(io::Error::other(format!(
            "runs contain term {:?} missing from the dictionary — tokenizer mismatch?",
            String::from_utf8_lossy(t)
        )));
    }
    let term_count: u32 = term_i
        .try_into()
        .map_err(|_| io::Error::other("term count exceeds u32"))?;
    if term_count as usize != rrt_idx.len() {
        return Err(io::Error::other(format!(
            "merged {term_count} terms but the dictionary has {}",
            rrt_idx.len()
        )));
    }
    entries_w.flush()?;
    impacts_w.flush()?;
    drop((entries_w, impacts_w));

    // Assemble: header + sparse + entries + impacts.
    let sparse_count = (term_count as usize).div_ceil(SPARSE_STRIDE as usize);
    assert_eq!(sparse.len(), sparse_count);
    let entries_off = (HEADER_SIZE + sparse_count * 8) as u64;
    let impacts_off = entries_off + term_count as u64 * ENTRY_SIZE as u64;
    let mut w = BufWriter::new(File::create(out)?);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?;
    w.write_all(&scale.to_le_bytes())?;
    w.write_all(&k1.to_le_bytes())?;
    w.write_all(&b.to_le_bytes())?;
    w.write_all(&avgdl.to_le_bytes())?;
    w.write_all(&term_count.to_le_bytes())?;
    w.write_all(&SPARSE_STRIDE.to_le_bytes())?;
    w.write_all(&entries_off.to_le_bytes())?;
    w.write_all(&impacts_off.to_le_bytes())?;
    w.write_all(&(n_docs as u64).to_le_bytes())?;
    w.write_all(&[0u8; 8])?;
    for &s in &sparse {
        w.write_all(&s.to_le_bytes())?;
    }
    io::copy(&mut File::open(tmp.join("entries.tmp"))?, &mut w)?;
    io::copy(&mut File::open(tmp.join("impacts.tmp"))?, &mut w)?;
    w.flush()?;
    let _ = std::fs::remove_file(tmp.join("entries.tmp"));
    let _ = std::fs::remove_file(tmp.join("impacts.tmp"));
    Ok(())
}

/// Builds a synthetic corpus through the in-RAM writer AND this pipeline's
/// run/merge/assembly, asserting byte-identical output.
fn selftest() {
    use roaringrange::{write_impacts, ImpactsAccumulator, TermIndexBuilder, TermIndexConfig};
    let dir = std::env::temp_dir().join(format!("rrb-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("selftest dir");

    let docs: Vec<String> = (0..30_000)
        .map(|i| {
            let mut s = String::new();
            for _ in 0..(i % 9) {
                s.push_str("alpha ");
            }
            for _ in 0..(i % 4) {
                s.push_str("beta ");
            }
            for j in 0..(i % 13) {
                s.push_str(&format!("word{} ", (i + j) % 257));
            }
            s.push_str("common");
            s
        })
        .collect();

    // The .rrt (plain tokenizer), written to disk so both paths read the same file.
    let cfg = TermIndexConfig {
        head_boundary: 65536,
        language: None,
        stopwords: false,
        block_cap: 0,
    };
    let mut tb = TermIndexBuilder::new(&cfg);
    for (i, d) in docs.iter().enumerate() {
        tb.add(i as u32, d);
    }
    let rrt_path = dir.join("self.rrt");
    tb.finish(BufWriter::new(File::create(&rrt_path).unwrap()))
        .unwrap();
    let rrt_idx = block_on(TermIndex::open(FileFetch::open(&rrt_path).unwrap())).unwrap();

    // Path A: in-RAM accumulator + write_impacts.
    let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
    for d in &docs {
        acc.add_doc(d);
    }
    let dict = block_on(rrt_idx.dict_terms()).unwrap();
    let mut a = Vec::new();
    write_impacts(&mut a, &dict, &acc, DEFAULT_K1, DEFAULT_B).unwrap();

    // Path B: spill tiny chunks via the same accumulation logic, then merge.
    let chunk_docs = 701u32; // deliberately not a divisor of the doc count
    let n = docs.len() as u32;
    let chunks = n.div_ceil(chunk_docs);
    let tok = Tokenizer::plain();
    for c in 0..chunks {
        let lo = c * chunk_docs;
        let hi = ((c + 1) * chunk_docs).min(n);
        let mut map: ChunkMap = BTreeMap::new();
        let mut lens = Vec::new();
        for doc in lo..hi {
            let toks = tok.tokenize(&docs[doc as usize]);
            lens.push(toks.len() as u32);
            let mut tf: BTreeMap<Vec<u8>, u8> = BTreeMap::new();
            for t in toks {
                let e = tf.entry(t.into_bytes()).or_default();
                *e = e.saturating_add(1);
            }
            for (term, cnt) in tf {
                map.entry(term).or_default().push((doc, cnt));
            }
        }
        write_run(&dir.join(format!("run-{c:05}.spill")), &map).unwrap();
        write_lens(&dir.join(format!("lens-{c:05}.spill")), &lens).unwrap();
    }
    let out_b = dir.join("self.rrb");
    merge(
        &rrt_idx,
        rrt_path.to_str().unwrap(),
        &dir,
        chunks,
        n,
        out_b.to_str().unwrap(),
        &|m| eprintln!("  {m}"),
    )
    .unwrap();
    let b = std::fs::read(&out_b).unwrap();

    assert_eq!(a.len(), b.len(), "selftest length mismatch");
    assert_eq!(a, b, "selftest byte mismatch");
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "selftest OK: both paths produced {} identical bytes",
        a.len()
    );
}
