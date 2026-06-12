//! Slices a monolithic `.rrt` into a TERM split set by doc-ID range — the fast
//! path to a geometric split set when the monolith already exists. The shared
//! rank-ordered doc-ID space means a split's posting for a term is exactly the
//! monolith posting restricted to `[lo, hi)`, rebased to local IDs; and the
//! monolith's dictionary + postings regions are laid out in term order, so the
//! whole slice is ONE sequential read of the `.rrt` fanned into N streaming
//! split writers. ~Disk speed, versus days re-tokenizing 484M records (the
//! `TermSplitSetBuilder`'s open-split map degrades badly at multi-GiB caps).
//!
//! Ranges are geometric in doc count: `base_docs`, doubling per tier, capped at
//! `max_docs` — the doc-count mirror of the builders' `byte_cap_max` doubling
//! (bytes per doc is roughly uniform, so split sizes follow). The last split
//! takes the remainder.
//!
//!   cargo run --release --features "terms splits" --example slice_term_monolith -- \
//!     MONO.rrt OUT_DIR PREFIX [base_docs=2000000] [max_docs=67108864]
//!   cargo run --release --features "terms splits" --example slice_term_monolith -- --selftest
//!
//! `--selftest` builds a synthetic corpus through `TermSplitSetBuilder` AND
//! through monolith-build → slice (using the builder's own doc ranges), and
//! asserts the two split sets are byte-identical — manifest and every split.

use futures::executor::block_on;
use roaring::RoaringBitmap;
use roaringrange::build::split_posting;
use roaringrange::terms::parse_dict_block;
use roaringrange::{
    write_splitset, FileFetch, Policy, SplitSetConfig, SplitSpec, TermIndex, TermIndexStreamWriter,
    BODY_KIND_TERM,
};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--selftest") {
        selftest();
        return;
    }
    if args.len() < 3 {
        eprintln!(
            "usage: slice_term_monolith MONO.rrt OUT_DIR PREFIX [base_docs=2000000] [max_docs=67108864]\n       slice_term_monolith --selftest"
        );
        std::process::exit(2);
    }
    let mono = &args[0];
    let out_dir = PathBuf::from(&args[1]);
    let prefix = args[2].clone();
    let base_docs: u32 = args
        .get(3)
        .map(|s| s.parse().expect("base_docs"))
        .unwrap_or(2_000_000);
    let max_docs: u32 = args
        .get(4)
        .map(|s| s.parse().expect("max_docs"))
        .unwrap_or(67_108_864);
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    // Doc count: the monolith records none, so take it from the deepest posting —
    // cheaper and simpler to pass corpus knowledge: max doc id + 1 is discovered
    // during the stream, but ranges must be FIXED up front. Use the record-store
    // convention: the caller knows the corpus; hard default to the OpenAlex full
    // corpus when slicing its monolith.
    let n_docs: u32 = args
        .get(5)
        .map(|s| s.parse().expect("n_docs"))
        .unwrap_or(484_369_476);

    let ranges = geometric_ranges(n_docs, base_docs, max_docs);
    eprintln!(
        "slicing {mono} into {} splits: {:?}",
        ranges.len(),
        ranges.iter().map(|&(lo, hi)| hi - lo).collect::<Vec<_>>()
    );
    let t0 = Instant::now();
    slice(Path::new(mono), &out_dir, &prefix, &ranges, 0).expect("slice");
    eprintln!("done in {:.0}s", t0.elapsed().as_secs_f64());
}

/// Doubling doc-count ranges: `base`, `2·base`, …, capped at `max`, last takes
/// the remainder.
fn geometric_ranges(n_docs: u32, base: u32, max: u32) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut lo: u32 = 0;
    let mut size: u64 = base as u64;
    while lo < n_docs {
        let hi = (lo as u64 + size).min(n_docs as u64) as u32;
        ranges.push((lo, hi));
        lo = hi;
        size = (size * 2).min(max as u64);
    }
    ranges
}

/// One split under construction: its streaming writer over a region temp file.
struct SplitSink {
    writer: TermIndexStreamWriter<BufWriter<File>>,
    region_path: PathBuf,
}

/// Slices the monolith at `mono` into one split per `ranges` entry plus the
/// manifest, writing `‹prefix›-sNNNNN.rrt` files and `‹prefix›.rrss` into
/// `out_dir`. `byte_cap` is recorded in the manifest (informational).
fn slice(
    mono: &Path,
    out_dir: &Path,
    prefix: &str,
    ranges: &[(u32, u32)],
    byte_cap: u64,
) -> io::Result<()> {
    // Reader-facing metadata (head boundary, tokenizer spec) + the dict block map.
    let idx = block_on(TermIndex::open(
        FileFetch::open(mono).expect("open mono .rrt"),
    ))
    .map_err(|e| io::Error::other(format!("open mono: {e:?}")))?;
    let head_boundary = idx.head_boundary();
    let (language, stopwords) = idx.tokenizer().spec();
    let block_locs = idx.dict_block_locs();
    let term_count = idx.len() as u64;
    drop(idx);

    // Raw header for region offsets: routerLen @16, dictLen @24 (40 B header).
    let mut f = File::open(mono)?;
    let mut header = [0u8; 40];
    f.read_exact(&mut header)?;
    let router_len = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let dict_len = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let dict_start = 40 + router_len;
    let postings_offset = dict_start + dict_len;

    // Two sequential readers in lockstep: dictionary blocks and posting blocks.
    let mut dict_r = BufReader::with_capacity(4 << 20, File::open(mono)?);
    dict_r.seek(SeekFrom::Start(dict_start))?;
    let mut post_r = BufReader::with_capacity(32 << 20, File::open(mono)?);
    post_r.seek(SeekFrom::Start(postings_offset))?;
    let mut post_pos: u64 = 0; // region-relative; must track each term's head_off

    let mut sinks: Vec<SplitSink> = ranges
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let region_path = out_dir.join(format!("{prefix}-s{i:05}.region.tmp"));
            let w = BufWriter::with_capacity(8 << 20, File::create(&region_path)?);
            Ok(SplitSink {
                writer: TermIndexStreamWriter::new(w, head_boundary, language, stopwords, 0),
                region_path,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    let t0 = Instant::now();
    let mut terms_done: u64 = 0;
    // Block offsets are contiguous in file order, so the sequential dict reader is
    // positioned at each block by construction; only the lengths matter here.
    for &(_abs_off, len) in &block_locs {
        let mut block = vec![0u8; len];
        dict_r.read_exact(&mut block)?;
        for (term, head_off, head_size) in parse_dict_block(&block) {
            if head_off != post_pos {
                return Err(io::Error::other(format!(
                    "postings lockstep diverged at term {:?}: dict says {head_off}, stream is at {post_pos}",
                    String::from_utf8_lossy(&term)
                )));
            }
            let mut tail_len = [0u8; 4];
            post_r.read_exact(&mut tail_len)?;
            let tail_size = u32::from_le_bytes(tail_len) as usize;
            let mut head = vec![0u8; head_size];
            post_r.read_exact(&mut head)?;
            let mut tail = vec![0u8; tail_size];
            post_r.read_exact(&mut tail)?;
            post_pos += 4 + head_size as u64 + tail_size as u64;

            let mut full = RoaringBitmap::deserialize_from(&head[..])
                .map_err(|e| io::Error::other(format!("head posting: {e}")))?;
            if tail_size > 0 {
                full |= RoaringBitmap::deserialize_from(&tail[..])
                    .map_err(|e| io::Error::other(format!("tail posting: {e}")))?;
            }
            let term = String::from_utf8(term)
                .map_err(|_| io::Error::other("non-UTF-8 dictionary term"))?;
            for (si, &(lo, hi)) in ranges.iter().enumerate() {
                // Cheap intersect check before materializing the slice.
                if full.range_cardinality(lo..hi) == 0 {
                    continue;
                }
                let local = RoaringBitmap::from_sorted_iter(full.range(lo..hi).map(|d| d - lo))
                    .expect("sorted local ids");
                let (h, t) = split_posting(&local, head_boundary);
                sinks[si].writer.push(&term, &h, &t)?;
            }
            terms_done += 1;
            if terms_done.is_multiple_of(20_000_000) {
                eprintln!(
                    "  {terms_done}/{term_count} terms in {:.0}s",
                    t0.elapsed().as_secs_f64()
                );
            }
        }
    }
    if terms_done != term_count {
        return Err(io::Error::other(format!(
            "dictionary stream yielded {terms_done} terms, header says {term_count}"
        )));
    }

    // Assemble each split (header + router + dict blocks + streamed region) and
    // collect the manifest entries.
    let mut specs = Vec::with_capacity(ranges.len());
    for (i, (sink, &(lo, hi))) in sinks.into_iter().zip(ranges).enumerate() {
        let region_len = sink.writer.region_len();
        let (header, router, blocks) = sink.writer.finish_meta()?;
        let actual = std::fs::metadata(&sink.region_path)?.len();
        if actual != region_len {
            return Err(io::Error::other(format!(
                "split {i}: region temp file is {actual} B, writer streamed {region_len} B"
            )));
        }
        let name = format!("{prefix}-s{i:05}.rrt");
        let mut out = BufWriter::new(File::create(out_dir.join(&name))?);
        out.write_all(&header)?;
        out.write_all(&router)?;
        for b in &blocks {
            out.write_all(b)?;
        }
        io::copy(&mut File::open(&sink.region_path)?, &mut out)?;
        out.flush()?;
        std::fs::remove_file(&sink.region_path)?;
        let byte_size = std::fs::metadata(out_dir.join(&name))?.len();
        eprintln!(
            "  split {i}: docs [{lo}, {hi}) -> {name} ({:.1} MB)",
            byte_size as f64 / (1024.0 * 1024.0)
        );
        specs.push(SplitSpec {
            data_file: name,
            tier: i.min(u16::MAX as usize) as u16,
            doc_count: hi - lo,
            doc_id_lo: lo,
            doc_id_hi: hi - 1,
            epoch: 0,
            byte_size,
            flags: 0,
            summary: Vec::new(),
        });
    }

    let config = SplitSetConfig {
        policy: Policy::Tiered,
        tier_count: specs.len().min(u16::MAX as usize) as u16,
        base_count: specs.len() as u32,
        byte_cap,
        gram_size: 0,
        body_kind: BODY_KIND_TERM,
        sortcol: None,
        flags: 0,
    };
    let mut manifest = BufWriter::new(File::create(out_dir.join(format!("{prefix}.rrss")))?);
    write_splitset(&mut manifest, &specs, &config)?;
    manifest.flush()
}

/// Builds a synthetic corpus through `TermSplitSetBuilder` AND through
/// monolith-build → slice with the builder's own doc ranges, asserting the two
/// split sets are byte-identical (manifest + every split).
fn selftest() {
    use roaringrange::{
        SplitSet, TermIndexBuilder, TermIndexConfig, TermSplitBuildConfig, TermSplitSetBuilder,
    };
    let dir = std::env::temp_dir().join(format!("slice-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("selftest dir");

    let docs: Vec<String> = (0..5_000)
        .map(|i| {
            let mut s = String::new();
            for j in 0..(i % 7 + 1) {
                s.push_str(&format!("word{} ", (i * 3 + j) % 211));
            }
            s.push_str("common shared");
            s
        })
        .collect();

    // Path A: the greedy builder (unfaceted, flat cap → several splits).
    let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap: 20_000,
        byte_cap_max: 0,
        head_boundary: 0,
        name_prefix: "sl".to_string(),
        sortcol: None,
        language: None,
        stopwords: false,
    });
    for d in &docs {
        b.add_text(d).unwrap();
    }
    let built = b.finish().unwrap();

    // The builder's ranges, recovered from its own manifest.
    let ss = SplitSet::from_bytes(&built.manifest).expect("parse manifest");
    let ranges: Vec<(u32, u32)> = ss
        .splits()
        .iter()
        .map(|s| (s.doc_id_lo, s.doc_id_hi + 1))
        .collect();

    // Path B: monolith, then slice with those exact ranges.
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
    let mono_path = dir.join("mono.rrt");
    tb.finish(BufWriter::new(File::create(&mono_path).unwrap()))
        .unwrap();
    slice(&mono_path, &dir, "sl", &ranges, 20_000).expect("slice");

    let manifest_b = std::fs::read(dir.join("sl.rrss")).unwrap();
    assert_eq!(
        built.manifest, manifest_b,
        "sliced manifest differs from the builder's"
    );
    for (name, bytes) in &built.splits {
        let sliced = std::fs::read(dir.join(name)).unwrap();
        assert_eq!(bytes, &sliced, "{name} differs between builder and slicer");
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "selftest OK: {} splits + manifest byte-identical across builder and slicer",
        built.splits.len()
    );
}
