//! Parallel Rust builder for the OpenAlex demo index.
//!
//! Produces the same `.rrs` (text index), `.rrf` (facet sidecar), and record
//! store as the Go loader in `examples/openalex`, but builds them directly with
//! the reader crate's `build` module — no roaringsearch, no FTSR, no transcode,
//! and no GC. It streams the works twice so the source text never lives in RAM:
//!
//!   pass 1: read only (id, cited_by_count) for every work, rank by citations
//!           descending to assign doc IDs (doc 0 = most cited);
//!   pass 2: re-stream, tokenize each work's text into trigram keys, insert its
//!           doc ID into key-sharded roaring bitmaps + facet bitmaps, and emit
//!           its record. Parsing/tokenizing fan out across shards with rayon.
//!
//! Input shards are local gzipped files (`-in <glob>`) or, with `-in s3://…/`,
//! the public OpenAlex bucket streamed directly over HTTPS (no download, no
//! credentials) — its `manifest` lists the objects. Pass through `-maxfiles N`
//! to cap the shard count for a quick run.
//!
//! For a corpus whose in-RAM index exceeds memory, `-chunks K` (K>1) partitions
//! the doc-ID space into K contiguous ranges and builds them one at a time, so
//! peak memory is one chunk's index plus the merge's per-key working set rather
//! than the whole index. Each chunk re-streams every source (skipping works
//! outside its doc-ID range), writes a key-sorted partial + a records temp file,
//! and ORs its facet postings into running accumulators; after the last chunk the
//! partials are merged into one standard `.rrs`, the record temps are concatenated
//! in doc-ID order, and the accumulated facets are written. This re-streams the
//! sources K times — acceptable for an offline build. `-chunks 1` (the default) is
//! the original single-pass path and stays byte-for-byte unchanged.

use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::chunk::{merge_partials_to_rrs, write_partial};
use roaringrange::build::{
    split_posting, write_facets, write_index, write_records, FacetCategory, FacetField,
    RecordWriter, DEFAULT_STRIDE,
};
use roaringrange::ngram_keys;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Trigram size (matches the index/reader contract).
const GRAM: usize = 3;
/// Byte cap on the reconstructed abstract, bounding indexed text per work.
const ABSTRACT_CHAR_CAP: usize = 2000;
/// Number of key shards; each is an independently-locked bitmap map, so parse
/// threads insert with low contention.
const KEY_SHARDS: usize = 256;
/// Facet fields, emitted in this order.
const FACET_FIELDS: [&str; 5] = ["year", "type", "oa", "language", "topic"];

/// One input shard: a local gzipped file or a public S3 (HTTPS) object.
enum Source {
    Local(PathBuf),
    Url(String),
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::Local(p) => p.display().to_string(),
            Source::Url(u) => u.clone(),
        }
    }
}

/// Pass-1 view: just enough to rank a work.
#[derive(Deserialize)]
struct RankRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    cited_by_count: Option<i64>,
}

/// Pass-2 view: the fields the index, facets, and record need. Unknown fields
/// are ignored; nullable scalars are `Option` so a `null` doesn't fail the line.
#[derive(Deserialize)]
struct Work {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    abstract_inverted_index: Option<HashMap<String, Vec<i64>>>,
    #[serde(default)]
    authorships: Vec<Authorship>,
    #[serde(default)]
    publication_year: Option<i64>,
    #[serde(default, rename = "type")]
    work_type: Option<String>,
    #[serde(default)]
    open_access: Option<OpenAccess>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    primary_topic: Option<Named>,
    #[serde(default)]
    concepts: Vec<Named>,
    #[serde(default)]
    cited_by_count: Option<i64>,
    #[serde(default)]
    primary_location: Option<PrimaryLocation>,
}

#[derive(Deserialize)]
struct Authorship {
    #[serde(default)]
    author: Option<Named>,
}
#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    display_name: Option<String>,
}
#[derive(Deserialize)]
struct OpenAccess {
    #[serde(default)]
    oa_status: Option<String>,
}
#[derive(Deserialize)]
struct PrimaryLocation {
    #[serde(default)]
    source: Option<Named>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let in_arg = arg(&args, "-in", "/tmp/openalex/works/*/*.gz");
    let rrs_path = arg(&args, "-rrs", "/tmp/openalex.rrs");
    let facets_path = arg(&args, "-facets", "/tmp/openalex.rrf");
    let bin_path = arg(&args, "-bin", "/tmp/openalex-records.bin");
    let idx_path = arg(&args, "-idx", "/tmp/openalex-records.idx");
    let limit: usize = arg(&args, "-limit", "0").parse().unwrap_or(0);
    let maxfiles: usize = arg(&args, "-maxfiles", "0").parse().unwrap_or(0);
    let chunks: usize = arg(&args, "-chunks", "1").parse().unwrap_or(1).max(1);

    let mut sources = resolve_sources(&in_arg);
    if sources.is_empty() {
        eprintln!("no input shards matched {in_arg}");
        std::process::exit(1);
    }
    if maxfiles > 0 && sources.len() > maxfiles {
        sources.truncate(maxfiles);
    }
    eprintln!("{} input shards", sources.len());
    let t0 = Instant::now();

    // Pass 1: rank by citations to assign doc IDs.
    let mut rows: Vec<(u64, i64)> = sources.par_iter().flat_map_iter(rank_source).collect();
    eprintln!(
        "pass1: {} ranked rows in {:.1}s",
        rows.len(),
        t0.elapsed().as_secs_f64()
    );
    rows.par_sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    let n = rows.len();
    if n == 0 {
        eprintln!("no works ranked");
        std::process::exit(1);
    }
    eprintln!("ranked {} works; top cited_by_count={}", n, rows[0].1);
    let id_to_doc: HashMap<u64, u32> = rows
        .iter()
        .enumerate()
        .map(|(i, (wid, _))| (*wid, i as u32))
        .collect();
    drop(rows);

    // Chunked path: bounded-memory build for indexes larger than RAM.
    if chunks > 1 {
        build_chunked(
            &sources,
            &id_to_doc,
            n,
            chunks,
            &rrs_path,
            &facets_path,
            &bin_path,
            &idx_path,
            t0,
        );
        return;
    }

    // Pass 2: tokenize + index + facets + records, fanned out across shards.
    let t1 = Instant::now();
    let shards: Vec<Mutex<HashMap<u64, RoaringBitmap>>> = (0..KEY_SHARDS)
        .map(|_| Mutex::new(HashMap::new()))
        .collect();
    let facets: Vec<Mutex<HashMap<String, RoaringBitmap>>> = (0..FACET_FIELDS.len())
        .map(|_| Mutex::new(HashMap::new()))
        .collect();

    let per_file: Vec<Vec<(u32, Vec<u8>)>> = sources
        .par_iter()
        .map(|s| build_source(s, &id_to_doc, &shards, &facets))
        .collect();
    let indexed: usize = per_file.iter().map(|v| v.len()).sum();
    eprintln!(
        "pass2: indexed {} works in {:.1}s",
        indexed,
        t1.elapsed().as_secs_f64()
    );

    // Place records into doc-ID order, then write the record store.
    let t2 = Instant::now();
    let mut records: Vec<Vec<u8>> = vec![Vec::new(); n];
    for fr in per_file {
        for (d, rec) in fr {
            records[d as usize] = rec;
        }
    }
    {
        let bin = BufWriter::with_capacity(1 << 20, File::create(&bin_path).expect("create bin"));
        let idx = BufWriter::with_capacity(1 << 20, File::create(&idx_path).expect("create idx"));
        write_records(bin, idx, &records).expect("write records");
    }
    eprintln!(
        "wrote record store {} (+{}) in {:.1}s",
        bin_path,
        idx_path,
        t2.elapsed().as_secs_f64()
    );
    drop(records);

    // Split each posting head/tail (parallel across shards) and write the RRS.
    let t3 = Instant::now();
    let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = shards
        .into_par_iter()
        .flat_map_iter(|m| {
            let map = m.into_inner().unwrap();
            map.into_iter()
                .map(|(k, bm)| {
                    let (h, t) = split_posting(&bm);
                    (k, h, t)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let ngrams = entries.len();
    {
        let out = BufWriter::with_capacity(1 << 20, File::create(&rrs_path).expect("create rrs"));
        write_index(out, GRAM as u16, DEFAULT_STRIDE, entries).expect("write index");
    }
    eprintln!(
        "wrote RRS {} ({} ngrams, {} bytes) in {:.1}s",
        rrs_path,
        ngrams,
        file_len(&rrs_path),
        t3.elapsed().as_secs_f64()
    );

    // Facets.
    let t4 = Instant::now();
    let fields_out: Vec<FacetField> = facets
        .into_iter()
        .enumerate()
        .map(|(fi, m)| {
            let map = m.into_inner().unwrap();
            let mut cats: Vec<FacetCategory> = map
                .into_iter()
                .map(|(val, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm);
                    FacetCategory {
                        name: val,
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            // Sort by name so the string-blob byte layout is reproducible across
            // runs (HashMap iteration order is otherwise nondeterministic; the
            // reader resolves by name and the category table re-sorts by key).
            cats.sort_by(|a, b| a.name.cmp(&b.name));
            FacetField {
                name: FACET_FIELDS[fi].to_string(),
                cats,
            }
        })
        .collect();
    {
        let out =
            BufWriter::with_capacity(1 << 20, File::create(&facets_path).expect("create facets"));
        write_facets(out, fields_out).expect("write facets");
    }
    eprintln!(
        "wrote facets {} ({} bytes) in {:.1}s",
        facets_path,
        file_len(&facets_path),
        t4.elapsed().as_secs_f64()
    );

    eprintln!(
        "DONE: {} docs in {:.1}s total",
        n,
        t0.elapsed().as_secs_f64()
    );
}

/// Resolves `-in` to input shards: an `s3://…/` prefix is enumerated from the
/// bucket manifest (streamed over HTTPS); anything else is a local glob.
fn resolve_sources(in_arg: &str) -> Vec<Source> {
    if let Some(_rest) = in_arg.strip_prefix("s3://") {
        eprintln!("enumerating S3 manifest under {in_arg} …");
        s3_sources(in_arg)
    } else {
        let mut v: Vec<PathBuf> = glob::glob(in_arg)
            .expect("invalid -in glob")
            .filter_map(Result::ok)
            .collect();
        v.sort();
        v.into_iter().map(Source::Local).collect()
    }
}

/// Enumerates object URLs for an `s3://…/` works prefix via its `manifest`.
fn s3_sources(prefix: &str) -> Vec<Source> {
    let base = prefix.trim_end_matches('/');
    let manifest_url = s3_to_https(&format!("{base}/manifest"));
    let mut body = String::new();
    http_get(&manifest_url)
        .and_then(|mut r| r.read_to_string(&mut body))
        .unwrap_or_else(|e| panic!("fetch manifest {manifest_url}: {e}"));
    let v: serde_json::Value = serde_json::from_str(&body).expect("parse manifest JSON");
    v["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e["url"].as_str())
                .map(|u| Source::Url(s3_to_https(u)))
                .collect()
        })
        .unwrap_or_default()
}

/// Converts `s3://bucket/key` to the public `https://bucket.s3.amazonaws.com/key`.
fn s3_to_https(s3url: &str) -> String {
    match s3url.strip_prefix("s3://") {
        Some(rest) => {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            format!("https://{bucket}.s3.amazonaws.com/{key}")
        }
        None => s3url.to_string(),
    }
}

/// GETs `url` (public, no credentials), retrying transient failures, returning
/// the streaming response body.
fn http_get(url: &str) -> std::io::Result<Box<dyn Read + Send + Sync>> {
    let mut last = String::new();
    for attempt in 0..5u64 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(250 * attempt));
        }
        match ureq::get(url).call() {
            Ok(resp) => return Ok(resp.into_reader()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(std::io::Error::other(format!("GET {url}: {last}")))
}

/// Opens a source as a buffered line reader over its decompressed JSON Lines.
/// Local files are read directly; S3 objects stream over HTTPS.
fn open_source(src: &Source) -> std::io::Result<Box<dyn BufRead>> {
    let raw: Box<dyn Read> = match src {
        Source::Local(p) => Box::new(File::open(p)?),
        Source::Url(u) => Box::new(http_get(u)?),
    };
    let gz = MultiGzDecoder::new(BufReader::with_capacity(1 << 20, raw));
    Ok(Box::new(BufReader::with_capacity(1 << 20, gz)))
}

/// Streams one source for pass 1, returning `(wid, cited_by_count)` per indexable
/// work (titled, with a parseable id).
fn rank_source(src: &Source) -> Vec<(u64, i64)> {
    let mut out = Vec::new();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip {}: {e}", src.label());
            return out;
        }
    };
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<RankRow>(&line) {
            if r.display_name.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            if let Some(wid) = parse_wid(&r.id) {
                out.push((wid, r.cited_by_count.unwrap_or(0)));
            }
        }
    }
    out
}

/// Streams one source for pass 2: tokenizes each work and inserts its doc ID into
/// the shared sharded bitmaps + facet bitmaps (one lock per touched shard/field),
/// returning the source's `(docID, record bytes)` pairs.
fn build_source(
    src: &Source,
    id_to_doc: &HashMap<u64, u32>,
    shards: &[Mutex<HashMap<u64, RoaringBitmap>>],
    facets: &[Mutex<HashMap<String, RoaringBitmap>>],
) -> Vec<(u32, Vec<u8>)> {
    let mut recs = Vec::new();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip {}: {e}", src.label());
            return recs;
        }
    };
    // Per-shard key buckets, reused across this source's works to batch locks.
    let mut buckets: Vec<Vec<u64>> = (0..KEY_SHARDS).map(|_| Vec::new()).collect();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let w: Work = match serde_json::from_str(&line) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let title = w.display_name.as_deref().unwrap_or("");
        if title.is_empty() {
            continue;
        }
        let docid = match parse_wid(&w.id).and_then(|wid| id_to_doc.get(&wid)) {
            Some(d) => *d,
            None => continue,
        };

        let abstract_ = reconstruct_abstract(&w.abstract_inverted_index);
        let authors = author_names(&w);
        let venue = w
            .primary_location
            .as_ref()
            .and_then(|p| p.source.as_ref())
            .and_then(|s| s.display_name.as_deref())
            .unwrap_or("");
        let topic = topic_name(&w);
        let text = build_text(title, &abstract_, &authors, venue);

        // Tokenize, bucket keys by shard, then insert each shard's keys under one lock.
        let keys = ngram_keys(&text, GRAM);
        for b in buckets.iter_mut() {
            b.clear();
        }
        for &k in &keys {
            buckets[(k as usize) % KEY_SHARDS].push(k);
        }
        for (si, ks) in buckets.iter().enumerate() {
            if ks.is_empty() {
                continue;
            }
            let mut m = shards[si].lock().unwrap();
            for &k in ks {
                m.entry(k).or_default().insert(docid);
            }
        }

        // Facets: one bitmap per (field, value).
        for (fi, map) in facets.iter().enumerate() {
            let v = facet_value(&w, fi, &topic);
            if v.is_empty() {
                continue;
            }
            map.lock().unwrap().entry(v).or_default().insert(docid);
        }

        let id_trim = trim_openalex_id(&w.id);
        recs.push((
            docid,
            build_record(
                id_trim,
                title,
                &authors,
                w.publication_year.unwrap_or(0),
                venue,
                w.cited_by_count.unwrap_or(0),
            ),
        ));
    }
    recs
}

/// Builds the index in `chunks` contiguous doc-ID ranges, one at a time, so peak
/// memory is a single chunk's index plus the merge's per-key working set rather
/// than the whole index. Re-streams every source once per chunk (skipping works
/// outside the chunk's doc-ID range), writing a key-sorted partial and a records
/// temp file per chunk and ORing each chunk's facet postings into running
/// accumulators (chunk ranges are disjoint, so OR = union). After the last chunk,
/// merges the partials into one standard `.rrs`, concatenates the record temps in
/// doc-ID order into the final store, and writes the accumulated facets — yielding
/// the same outputs the single-pass path would, in bounded memory.
#[allow(clippy::too_many_arguments)]
fn build_chunked(
    sources: &[Source],
    id_to_doc: &HashMap<u64, u32>,
    n: usize,
    chunks: usize,
    rrs_path: &str,
    facets_path: &str,
    bin_path: &str,
    idx_path: &str,
    t0: Instant,
) {
    let chunk_size = n.div_ceil(chunks);
    eprintln!(
        "chunked build: {chunks} chunks of ~{chunk_size} docs each (re-streams sources {chunks}×)"
    );

    let tmp_dir = std::env::temp_dir();
    let stamp = std::process::id();
    let mut partial_paths: Vec<PathBuf> = Vec::with_capacity(chunks);
    let mut record_paths: Vec<PathBuf> = Vec::with_capacity(chunks);

    // Running per-(field, value) facet postings, unioned across chunks.
    let mut facet_acc: Vec<HashMap<String, RoaringBitmap>> =
        (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();

    for c in 0..chunks {
        let lo = (c * chunk_size) as u32;
        let hi = (((c + 1) * chunk_size).min(n)) as u32;
        if lo >= hi {
            continue;
        }
        let tc = Instant::now();

        // This chunk's sharded text bitmaps + facet bitmaps, populated in parallel.
        let shards: Vec<Mutex<HashMap<u64, RoaringBitmap>>> = (0..KEY_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        let facets: Vec<Mutex<HashMap<String, RoaringBitmap>>> = (0..FACET_FIELDS.len())
            .map(|_| Mutex::new(HashMap::new()))
            .collect();

        let per_file: Vec<Vec<(u32, Vec<u8>)>> = sources
            .par_iter()
            .map(|s| build_source_range(s, id_to_doc, lo, hi, &shards, &facets))
            .collect();
        let indexed: usize = per_file.iter().map(|v| v.len()).sum();

        // Records for this chunk, placed at their offset within [lo, hi).
        let mut chunk_recs: Vec<Vec<u8>> = vec![Vec::new(); (hi - lo) as usize];
        for fr in per_file {
            for (d, rec) in fr {
                chunk_recs[(d - lo) as usize] = rec;
            }
        }
        let rpath = tmp_dir.join(format!("rr_chunk_{stamp}_{c}.recs"));
        write_chunk_records(&rpath, &chunk_recs).expect("write chunk records");
        record_paths.push(rpath);
        drop(chunk_recs);

        // Serialize this chunk's text postings to a key-sorted partial (full bitmaps).
        let entries: Vec<(u64, Vec<u8>)> = shards
            .into_par_iter()
            .flat_map_iter(|m| {
                m.into_inner()
                    .unwrap()
                    .into_iter()
                    .map(|(k, bm)| {
                        let mut b = Vec::with_capacity(bm.serialized_size());
                        bm.serialize_into(&mut b).expect("serialize posting");
                        (k, b)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let ngrams = entries.len();
        let ppath = tmp_dir.join(format!("rr_chunk_{stamp}_{c}.partial"));
        {
            let out =
                BufWriter::with_capacity(1 << 20, File::create(&ppath).expect("create partial"));
            write_partial(out, entries).expect("write partial");
        }
        partial_paths.push(ppath);

        // Union this chunk's facet postings into the running accumulators.
        for (fi, m) in facets.into_iter().enumerate() {
            for (val, bm) in m.into_inner().unwrap() {
                match facet_acc[fi].get_mut(&val) {
                    Some(acc) => *acc |= bm,
                    None => {
                        facet_acc[fi].insert(val, bm);
                    }
                }
            }
        }

        eprintln!(
            "chunk {c} [{lo},{hi}): indexed {indexed} works, {ngrams} ngrams in {:.1}s",
            tc.elapsed().as_secs_f64()
        );
    }

    // Merge partials into one standard RRS.
    let t3 = Instant::now();
    {
        let mut rrs = File::create(rrs_path).expect("create rrs");
        merge_partials_to_rrs(&partial_paths, GRAM as u16, DEFAULT_STRIDE, &mut rrs)
            .expect("merge partials");
    }
    eprintln!(
        "merged {} partials -> RRS {} ({} bytes) in {:.1}s",
        partial_paths.len(),
        rrs_path,
        file_len(rrs_path),
        t3.elapsed().as_secs_f64()
    );

    // Concatenate per-chunk record temps in doc-ID order into the final store.
    let t2 = Instant::now();
    concat_chunk_records(&record_paths, n, bin_path, idx_path).expect("concat records");
    eprintln!(
        "wrote record store {} (+{}) in {:.1}s",
        bin_path,
        idx_path,
        t2.elapsed().as_secs_f64()
    );

    // Facets from the unioned accumulators.
    let t4 = Instant::now();
    let fields_out: Vec<FacetField> = facet_acc
        .into_iter()
        .enumerate()
        .map(|(fi, map)| {
            let mut cats: Vec<FacetCategory> = map
                .into_iter()
                .map(|(val, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm);
                    FacetCategory {
                        name: val,
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            // Sort by name so the string-blob byte layout is reproducible across
            // runs (HashMap iteration order is otherwise nondeterministic; the
            // reader resolves by name and the category table re-sorts by key).
            cats.sort_by(|a, b| a.name.cmp(&b.name));
            FacetField {
                name: FACET_FIELDS[fi].to_string(),
                cats,
            }
        })
        .collect();
    {
        let out =
            BufWriter::with_capacity(1 << 20, File::create(facets_path).expect("create facets"));
        write_facets(out, fields_out).expect("write facets");
    }
    eprintln!(
        "wrote facets {} ({} bytes) in {:.1}s",
        facets_path,
        file_len(facets_path),
        t4.elapsed().as_secs_f64()
    );

    for p in partial_paths.iter().chain(record_paths.iter()) {
        let _ = std::fs::remove_file(p);
    }
    eprintln!("DONE: {n} docs in {:.1}s total", t0.elapsed().as_secs_f64());
}

/// Pass-2 worker for a chunk: like [`build_source`] but indexes only works whose
/// doc ID falls in `[lo, hi)`, returning that source's `(docID, record bytes)`
/// pairs for the chunk.
fn build_source_range(
    src: &Source,
    id_to_doc: &HashMap<u64, u32>,
    lo: u32,
    hi: u32,
    shards: &[Mutex<HashMap<u64, RoaringBitmap>>],
    facets: &[Mutex<HashMap<String, RoaringBitmap>>],
) -> Vec<(u32, Vec<u8>)> {
    let mut recs = Vec::new();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip {}: {e}", src.label());
            return recs;
        }
    };
    let mut buckets: Vec<Vec<u64>> = (0..KEY_SHARDS).map(|_| Vec::new()).collect();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let w: Work = match serde_json::from_str(&line) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let title = w.display_name.as_deref().unwrap_or("");
        if title.is_empty() {
            continue;
        }
        let docid = match parse_wid(&w.id).and_then(|wid| id_to_doc.get(&wid)) {
            Some(d) => *d,
            None => continue,
        };
        // Skip works outside this chunk's doc-ID range.
        if docid < lo || docid >= hi {
            continue;
        }

        let abstract_ = reconstruct_abstract(&w.abstract_inverted_index);
        let authors = author_names(&w);
        let venue = w
            .primary_location
            .as_ref()
            .and_then(|p| p.source.as_ref())
            .and_then(|s| s.display_name.as_deref())
            .unwrap_or("");
        let topic = topic_name(&w);
        let text = build_text(title, &abstract_, &authors, venue);

        let keys = ngram_keys(&text, GRAM);
        for b in buckets.iter_mut() {
            b.clear();
        }
        for &k in &keys {
            buckets[(k as usize) % KEY_SHARDS].push(k);
        }
        for (si, ks) in buckets.iter().enumerate() {
            if ks.is_empty() {
                continue;
            }
            let mut m = shards[si].lock().unwrap();
            for &k in ks {
                m.entry(k).or_default().insert(docid);
            }
        }

        for (fi, map) in facets.iter().enumerate() {
            let v = facet_value(&w, fi, &topic);
            if v.is_empty() {
                continue;
            }
            map.lock().unwrap().entry(v).or_default().insert(docid);
        }

        let id_trim = trim_openalex_id(&w.id);
        recs.push((
            docid,
            build_record(
                id_trim,
                title,
                &authors,
                w.publication_year.unwrap_or(0),
                venue,
                w.cited_by_count.unwrap_or(0),
            ),
        ));
    }
    recs
}

/// Flushes a chunk's records (in chunk-local doc-ID order) to a temp file as a
/// stream of `[len u32][bytes]` frames, so they can be concatenated later without
/// holding all chunks' records in RAM.
fn write_chunk_records(path: &PathBuf, recs: &[Vec<u8>]) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    for rec in recs {
        w.write_all(&(rec.len() as u32).to_le_bytes())?;
        w.write_all(rec)?;
    }
    w.flush()
}

/// Concatenates the per-chunk record temps (written by [`write_chunk_records`] in
/// ascending chunk order, each chunk in doc-ID order) into the final record store,
/// streaming through a [`RecordWriter`] so no chunk's records stay resident — only
/// one record frame is held at a time. The chunk temps in order reconstruct the
/// global doc-ID sequence (chunks are contiguous disjoint ranges), giving a store
/// byte-identical to the single-pass path's [`write_records`] output.
fn concat_chunk_records(
    paths: &[PathBuf],
    n: usize,
    bin_path: &str,
    idx_path: &str,
) -> std::io::Result<()> {
    let bin = BufWriter::with_capacity(1 << 20, File::create(bin_path)?);
    let idx = BufWriter::with_capacity(1 << 20, File::create(idx_path)?);
    let mut writer = RecordWriter::new(bin, idx, n as u32)?;

    for p in paths {
        let mut r = BufReader::with_capacity(1 << 20, File::open(p)?);
        loop {
            let mut lb = [0u8; 4];
            match r.read_exact(&mut lb) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let mut bytes = vec![0u8; u32::from_le_bytes(lb) as usize];
            r.read_exact(&mut bytes)?;
            writer.write(&bytes)?;
        }
    }
    writer.flush()
}

/// Parses the numeric tail of an OpenAlex work id
/// ("https://openalex.org/W2741809807" -> 2741809807), a compact, stable key
/// across the two passes.
fn parse_wid(id: &str) -> Option<u64> {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let bytes = tail.as_bytes();
    if bytes.len() < 2 || (bytes[0] != b'W' && bytes[0] != b'w') {
        return None;
    }
    tail[1..].parse::<u64>().ok()
}

/// Drops the URL prefix, keeping the "W..." id stored in the record.
fn trim_openalex_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Joins authorship display names with "; ".
fn author_names(w: &Work) -> String {
    let names: Vec<&str> = w
        .authorships
        .iter()
        .filter_map(|a| a.author.as_ref().and_then(|x| x.display_name.as_deref()))
        .filter(|n| !n.is_empty())
        .collect();
    names.join("; ")
}

/// Primary topic display name, falling back to the first concept's.
fn topic_name(w: &Work) -> String {
    if let Some(n) = w
        .primary_topic
        .as_ref()
        .and_then(|t| t.display_name.as_deref())
    {
        if !n.is_empty() {
            return n.to_string();
        }
    }
    w.concepts
        .first()
        .and_then(|c| c.display_name.as_deref())
        .unwrap_or("")
        .to_string()
}

/// Rebuilds abstract text from OpenAlex's inverted index (word -> positions),
/// capped at `ABSTRACT_CHAR_CAP` bytes on a char boundary.
fn reconstruct_abstract(idx: &Option<HashMap<String, Vec<i64>>>) -> String {
    let idx = match idx {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };
    let max_pos = idx
        .values()
        .flat_map(|ps| ps.iter().copied())
        .max()
        .unwrap_or(-1);
    if max_pos < 0 {
        return String::new();
    }
    let mut words: Vec<&str> = vec![""; (max_pos as usize) + 1];
    for (word, ps) in idx {
        for &p in ps {
            if p >= 0 && (p as usize) < words.len() {
                words[p as usize] = word.as_str();
            }
        }
    }
    let mut s = words.join(" ");
    if s.len() > ABSTRACT_CHAR_CAP {
        let mut end = ABSTRACT_CHAR_CAP;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Indexed text: title, then any non-empty abstract/authors/venue (space-joined).
fn build_text(title: &str, abstract_: &str, authors: &str, venue: &str) -> String {
    let mut s =
        String::with_capacity(title.len() + abstract_.len() + authors.len() + venue.len() + 3);
    s.push_str(title);
    for f in [abstract_, authors, venue] {
        if !f.is_empty() {
            s.push(' ');
            s.push_str(f);
        }
    }
    s
}

/// The work's value for facet field `fi` ("" = omit).
fn facet_value(w: &Work, fi: usize, topic: &str) -> String {
    match fi {
        0 => match w.publication_year {
            Some(y) if y != 0 => y.to_string(),
            _ => String::new(),
        },
        1 => w.work_type.clone().unwrap_or_default(),
        2 => w
            .open_access
            .as_ref()
            .and_then(|o| o.oa_status.clone())
            .unwrap_or_default(),
        3 => w.language.clone().unwrap_or_default(),
        _ => topic.to_string(),
    }
}

/// Marshals the stored record JSON (compact keys: id, t, a, y, v, c) with the
/// same omit-empty rules as the Go loader.
fn build_record(
    id: &str,
    title: &str,
    authors: &str,
    year: i64,
    venue: &str,
    cited: i64,
) -> Vec<u8> {
    let mut s = String::with_capacity(160);
    s.push_str("{\"id\":");
    s.push_str(&json_str(id));
    s.push_str(",\"t\":");
    s.push_str(&json_str(title));
    if !authors.is_empty() {
        s.push_str(",\"a\":");
        s.push_str(&json_str(authors));
    }
    if year != 0 {
        s.push_str(",\"y\":");
        s.push_str(&year.to_string());
    }
    if !venue.is_empty() {
        s.push_str(",\"v\":");
        s.push_str(&json_str(venue));
    }
    s.push_str(",\"c\":");
    s.push_str(&cited.to_string());
    s.push('}');
    s.into_bytes()
}

/// JSON-encodes a string (quoted + escaped) via serde_json.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Returns the value following `flag` in `args`, or `default`.
fn arg(args: &[String], flag: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// File size in bytes, or 0 if it can't be stat'd.
fn file_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
