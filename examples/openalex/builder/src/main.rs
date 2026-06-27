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
//! For a corpus whose in-RAM index exceeds memory, `-chunks K` (K>1) runs the
//! phased, resumable build (see [`phased`]): the doc-ID space is partitioned into K
//! contiguous ranges built one at a time, each writing four per-chunk artifacts
//! (text partial, record temp, facet partial, DOI temp) atomically to a stable work
//! directory, and the finalizers then merge those into the `.rrs`, record store,
//! `.rrf`, and `.rril`. Each phase — and each chunk — skips itself when its output
//! already exists, so an interrupted build is resumed by re-running with the same
//! arguments; the DOI and facet accumulators live on disk rather than in RAM, and
//! the doc-ID map is dropped before the finalizers, so peak memory is one chunk's
//! working set rather than the whole corpus. `-work <dir>` overrides the work
//! directory; `-keep-work` retains it. `-chunks 1` (the default) is the single-pass
//! path and stays byte-for-byte unchanged; the phased outputs are byte-identical to
//! it on the same inputs.
//!
//! Additive outputs: every record optionally carries a stored `ab` abstract field
//! (`-abstract-cap`, default 2000 bytes; 0 omits it), a DOI exact-lookup sidecar
//! (`-lookup`, `RRIL`) maps bare DOIs to doc IDs, and the record store can be
//! zstd-compressed against a trained shared dictionary (`-records-zstd`, with
//! `-dict`/`-dict-size`/`-zstd-level`) — all off-by-default for compression so a
//! plain run is byte-for-byte unchanged save for the new `ab` field and lookup.
//! Record-store zstd composes with `-chunks > 1`: the shared dictionary is trained
//! from a sample re-derived from the record temps (exactly the single-pass
//! selection), so a chunked compressed build is byte-identical to the single-pass
//! one.
//!
//! `-split-set` mode builds an `RRSS` split set instead of the monolith — byte-capped tiered
//! `RRS` splits (`-split-cap`, default 512 MiB; the resident per-split Bloom makes manifest/boot
//! cost grow with split count, so the default favors a handful of large splits) with term Bloom
//! filters (`-bloom-bits`) + per-split `RRSF` facet sidecars + a matching record store —
//! streaming each sealed split to `-out` so the split output never all lives in RAM. See
//! [`build_split_set`].

use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::{
    serialize_posting, split_posting, train_record_dict, write_facets, write_index, write_lookup,
    write_records, write_records_zstd, FacetCategory, FacetField, RecordWriter,
    DEFAULT_HEAD_BOUNDARY, DEFAULT_STRIDE,
};
use roaringrange::ngram_keys;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{error, info, info_span, warn};

/// Initializes structured logging: timestamped, leveled, key=value events with the
/// active phase span as context. Level defaults to INFO; override with `RUST_LOG`
/// (e.g. `RUST_LOG=debug`). Writes to stderr so existing `> build.log` capture and
/// `tail -f` keep working.
fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        // Color on an interactive terminal; plain text when redirected to build.log.
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

mod phased;
mod secondary;

/// Trigram size (matches the index/reader contract).
const GRAM: usize = 3;
/// Byte cap on the reconstructed abstract, bounding indexed text per work.
const ABSTRACT_CHAR_CAP: usize = 2000;
/// Max records sampled to train the shared zstd dictionary in the single-pass
/// path: enough variety for a good dictionary without holding extra copies.
const ZSTD_DICT_SAMPLE_CAP: usize = 100_000;
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
    #[serde(default)]
    doi: Option<String>,
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

/// One source's pass-2 output: its `(docID, record bytes)` pairs, its
/// `(bare DOI, docID)` lookup pairs (only for works that carry a DOI), and its
/// per-field facet postings (one `value -> docs` map per [`FACET_FIELDS`] entry).
/// Facets are accumulated source-locally (no shared lock) and unioned by the
/// caller — the union is order-independent, so the result is identical to a
/// shared-map build but without the per-work facet-mutex contention.
struct SourceOut {
    recs: Vec<(u32, Vec<u8>)>,
    dois: Vec<(String, u32)>,
    facets: Vec<HashMap<String, RoaringBitmap>>,
}

fn main() {
    init_logging();
    let args: Vec<String> = std::env::args().collect();

    // The v2-era head-boundary tools (-analyze-head, -gen-workload, -transcode)
    // were removed with the RRS v3 head/tail collapse: there is no head boundary
    // left to tune or re-split, and their v2-layout parsing misread v3 files.
    // Fail loudly in case a stale runbook still passes them.
    for gone in ["-analyze-head", "-gen-workload", "-transcode"] {
        if flag(&args, gone) {
            eprintln!(
                "{gone} was removed with RRS v3 (no head boundary exists to tune or re-split)"
            );
            std::process::exit(2);
        }
    }

    // Secondary-index mode: remap the *finished* primary outputs into an alternate
    // sort order (newest-first by `-sort-field`, default "year"), writing a second
    // `.rrs` + `.rrf` + an `RRSC` perm column. Independent of the corpus (`-in`) —
    // it reads the primary `.rrs`/`.rrf` and the record `.idx` (for the doc count).
    if flag(&args, "-secondary") {
        let rrs_in = arg(&args, "-in-rrs", "/tmp/openalex.rrs");
        let rrf_in = arg(&args, "-in-rrf", "/tmp/openalex.rrf");
        let idx_in = arg(&args, "-in-idx", "/tmp/openalex-records.idx");
        let sort_field = arg(&args, "-sort-field", "year");
        let out_rrs = arg(&args, "-out-rrs", "/tmp/openalex-newest.rrs");
        let out_rrf = arg(&args, "-out-rrf", "/tmp/openalex-newest.rrf");
        let out_perm = arg(&args, "-out-perm", "/tmp/openalex-newest.perm.rrsc");
        let n = secondary::read_record_count(&idx_in).expect("read record count") as usize;
        let _span = info_span!("secondary").entered();
        info!(docs = n, sort_field = %sort_field, "secondary build start (in: {rrs_in}, {rrf_in})");
        secondary::build_secondary(
            &rrs_in,
            &rrf_in,
            n,
            &sort_field,
            &out_rrs,
            &out_rrf,
            &out_perm,
        )
        .expect("build secondary");
        return;
    }

    let in_arg = arg(&args, "-in", "/tmp/openalex/works/*/*.gz");
    let rrs_path = arg(&args, "-rrs", "/tmp/openalex.rrs");
    let facets_path = arg(&args, "-facets", "/tmp/openalex.rrf");
    let bin_path = arg(&args, "-bin", "/tmp/openalex-records.bin");
    let idx_path = arg(&args, "-idx", "/tmp/openalex-records.idx");
    let lookup_path = arg(&args, "-lookup", "/tmp/openalex.rril");
    let limit: usize = arg(&args, "-limit", "0").parse().unwrap_or(0);
    let maxfiles: usize = arg(&args, "-maxfiles", "0").parse().unwrap_or(0);
    let chunks: usize = arg(&args, "-chunks", "1").parse().unwrap_or(1).max(1);
    // Stored-abstract config: -abstract-cap caps the `ab` record field (0 = omit
    // it entirely, preserving the lean record). This is independent of the index
    // text cap (ABSTRACT_CHAR_CAP in build_text), which stays unchanged.
    let abstract_cap: usize = arg(&args, "-abstract-cap", "2000").parse().unwrap_or(2000);
    // Record-store zstd config (all no-ops unless -records-zstd is set).
    let records_zstd = flag(&args, "-records-zstd");
    let dict_path = arg(&args, "-dict", "/tmp/openalex.dict");
    let dict_size: usize = arg(&args, "-dict-size", "114688").parse().unwrap_or(114688);
    let zstd_level: i32 = arg(&args, "-zstd-level", "19").parse().unwrap_or(19);
    // Phased-build knobs (chunked path only): work directory for resumable
    // per-chunk artifacts (defaults to `<rrs>.rrwork`), and whether to keep it.
    let work_arg = args
        .iter()
        .position(|a| a == "-work")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let keep_work = flag(&args, "-keep-work");

    let mut sources = resolve_sources(&in_arg);
    if sources.is_empty() {
        warn!("no input shards matched {in_arg}");
        std::process::exit(1);
    }
    if maxfiles > 0 && sources.len() > maxfiles {
        sources.truncate(maxfiles);
    }
    info!(shards = sources.len(), "resolved input sources");
    let t0 = Instant::now();

    // Split-set mode: build an `RRSS` split set (many byte-capped immutable splits + per-split
    // `RRSF` facet sidecars + a matching record store) instead of the monolith, so it can be
    // compared side by side. `-split-set` builds trigram (`RRS`) bodies; `-term-splits` builds
    // term/FST (`RRTI`) bodies for a head-to-head comparison. Both stream sealed splits to disk;
    // see `build_split_set`. Use distinct `-out`/`-split-prefix` so the two don't collide.
    if flag(&args, "-split-set") {
        build_split_set(&args, &sources, limit, abstract_cap, t0, SplitBody::Trigram);
        return;
    }
    if flag(&args, "-term-splits") {
        build_split_set(&args, &sources, limit, abstract_cap, t0, SplitBody::Term);
        return;
    }

    // Chunked path: the phased, resumable, bounded-memory build for indexes larger
    // than RAM. It ranks internally (persisting the ranking so a resume skips it)
    // and writes per-chunk artifacts it can pick up after an interruption.
    if chunks > 1 {
        phased::build(
            &sources,
            phased::Opts {
                rrs_path: &rrs_path,
                facets_path: &facets_path,
                bin_path: &bin_path,
                idx_path: &idx_path,
                lookup_path: &lookup_path,
                limit,
                chunks,
                abstract_cap,
                zstd: records_zstd.then_some((dict_path.as_str(), dict_size, zstd_level)),
                work_dir: work_arg.as_deref(),
                keep_work,
            },
            t0,
        );
        return;
    }

    // Single-pass: rank by citations to assign doc IDs, then index in one pass.
    let rows = rank_rows(&sources, limit, t0);
    let n = rows.len();
    if n == 0 {
        warn!("no works ranked");
        std::process::exit(1);
    }
    info!(works = n, top_cited = rows[0].1, "ranked");
    let id_to_doc: HashMap<u64, u32> = rows
        .iter()
        .enumerate()
        .map(|(i, (wid, _))| (*wid, i as u32))
        .collect();
    drop(rows);

    // Pass 2: tokenize + index + facets + records, fanned out across shards.
    let t1 = Instant::now();
    let shards: Vec<Mutex<HashMap<u64, RoaringBitmap>>> = (0..KEY_SHARDS)
        .map(|_| Mutex::new(HashMap::new()))
        .collect();

    let per_file: Vec<SourceOut> = sources
        .par_iter()
        .map(|s| build_source(s, &id_to_doc, &shards, abstract_cap))
        .collect();
    check_stream_truncations("pass 2 (indexing)");
    let indexed: usize = per_file.iter().map(|v| v.recs.len()).sum();
    info!(
        works = indexed,
        elapsed_s = t1.elapsed().as_secs_f64(),
        "pass2 indexed"
    );

    // Place records into doc-ID order; gather DOIs and union per-source facet
    // postings across all sources (union is order-independent).
    let t2 = Instant::now();
    let mut records: Vec<Vec<u8>> = vec![Vec::new(); n];
    let mut dois: Vec<(String, u32)> = Vec::new();
    let mut facet_acc: Vec<HashMap<String, RoaringBitmap>> =
        (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
    for fr in per_file {
        for (d, rec) in fr.recs {
            records[d as usize] = rec;
        }
        dois.extend(fr.dois);
        for (fi, m) in fr.facets.into_iter().enumerate() {
            for (val, bm) in m {
                match facet_acc[fi].get_mut(&val) {
                    Some(acc) => *acc |= bm,
                    None => {
                        facet_acc[fi].insert(val, bm);
                    }
                }
            }
        }
    }

    // Write the record store: optionally zstd-compressed against a shared trained
    // dictionary, else the original raw (version-1) store byte-for-byte.
    if records_zstd {
        let dict = train_dict_from_records(&records, dict_size);
        std::fs::write(&dict_path, &dict).expect("write dict");
        info!(
            bytes = dict.len(),
            sampled = sample_count(records.len()),
            "trained zstd dict {dict_path}"
        );
        let bin = BufWriter::with_capacity(1 << 20, File::create(&bin_path).expect("create bin"));
        let idx = BufWriter::with_capacity(1 << 20, File::create(&idx_path).expect("create idx"));
        write_records_zstd(bin, idx, &records, &dict, zstd_level).expect("write records (zstd)");
    } else {
        let bin = BufWriter::with_capacity(1 << 20, File::create(&bin_path).expect("create bin"));
        let idx = BufWriter::with_capacity(1 << 20, File::create(&idx_path).expect("create idx"));
        write_records(bin, idx, &records).expect("write records");
    }
    info!(
        elapsed_s = t2.elapsed().as_secs_f64(),
        "wrote record store {bin_path} (+{idx_path})"
    );
    drop(records);

    // DOI exact-lookup sidecar.
    let tl = Instant::now();
    write_doi_lookup(&dois, &lookup_path);
    info!(
        entries = dois.len(),
        bytes = file_len(&lookup_path),
        elapsed_s = tl.elapsed().as_secs_f64(),
        "wrote DOI lookup {lookup_path}"
    );
    drop(dois);

    // Split each posting head/tail (parallel across shards) and write the RRS.
    let t3 = Instant::now();
    let entries: Vec<(u64, Vec<u8>)> = shards
        .into_par_iter()
        .flat_map_iter(|m| {
            let map = m.into_inner().unwrap();
            map.into_iter()
                .map(|(k, bm)| (k, serialize_posting(&bm)))
                .collect::<Vec<_>>()
        })
        .collect();
    let ngrams = entries.len();
    {
        let out = BufWriter::with_capacity(1 << 20, File::create(&rrs_path).expect("create rrs"));
        write_index(out, GRAM as u16, DEFAULT_STRIDE, entries).expect("write index");
    }
    info!(
        ngrams,
        bytes = file_len(&rrs_path),
        elapsed_s = t3.elapsed().as_secs_f64(),
        "wrote RRS {rrs_path}"
    );

    // Facets, from the unioned per-source accumulators.
    let t4 = Instant::now();
    let fields_out: Vec<FacetField> = facet_acc
        .into_iter()
        .enumerate()
        .map(|(fi, map)| {
            let mut cats: Vec<FacetCategory> = map
                .into_iter()
                .map(|(val, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm, DEFAULT_HEAD_BOUNDARY);
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
    info!(
        bytes = file_len(&facets_path),
        elapsed_s = t4.elapsed().as_secs_f64(),
        "wrote facets {facets_path}"
    );

    info!(
        docs = n,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "build complete (single-pass)"
    );
}

/// Resolves `-in` to input shards: an `s3://…/` prefix is enumerated from the
/// bucket manifest (streamed over HTTPS); anything else is a local glob.
fn resolve_sources(in_arg: &str) -> Vec<Source> {
    if let Some(_rest) = in_arg.strip_prefix("s3://") {
        info!("enumerating S3 manifest under {in_arg}");
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
/// Local files are read directly; S3 objects stream over HTTPS. OpenAlex ships
/// gzipped JSON-Lines shards, so the input is gzip-decoded.
fn open_source(src: &Source) -> std::io::Result<Box<dyn BufRead>> {
    let raw: Box<dyn Read> = match src {
        Source::Local(p) => Box::new(File::open(p)?),
        Source::Url(u) => Box::new(http_get(u)?),
    };
    let gz = MultiGzDecoder::new(BufReader::with_capacity(1 << 20, raw));
    Ok(Box::new(BufReader::with_capacity(1 << 20, gz)))
}

/// Source streams that ended in a mid-body read error this pass. A transport or
/// gzip failure mid-stream silently truncates the source's line iterator —
/// every work after the failure is lost — so each one is counted and the pass
/// must fail loudly via [`check_stream_truncations`] instead of building an
/// index with silently missing docs.
static STREAM_TRUNCATIONS: AtomicUsize = AtomicUsize::new(0);

/// Records a mid-stream read failure for `label`: the rest of that source is
/// unreadable and its remaining works are lost to this pass.
fn note_stream_truncation(label: &str, err: &std::io::Error) {
    STREAM_TRUNCATIONS.fetch_add(1, Ordering::Relaxed);
    warn!(source = %label, error = %err, "mid-stream read error — source truncated");
}

/// Fails the build if any source stream was truncated since the last check.
/// The phased build resumes cheaply from its chunk artifacts, so rerunning is
/// far cheaper than shipping an index built over silently missing documents.
pub(crate) fn check_stream_truncations(pass: &str) {
    let n = STREAM_TRUNCATIONS.swap(0, Ordering::Relaxed);
    if n > 0 {
        error!(
            sources = n,
            pass, "source streams truncated by mid-stream read errors; rerun the build"
        );
        std::process::exit(1);
    }
}

/// Streams one source for pass 1, returning `(wid, cited_by_count)` per indexable
/// work (titled, with a parseable id).
fn rank_source(src: &Source) -> Vec<(u64, i64)> {
    let mut out = Vec::new();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            warn!(source = %src.label(), error = %e, "skipping unreadable source");
            return out;
        }
    };
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                note_stream_truncation(&src.label(), &e);
                break;
            }
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

/// Pass 1: streams every source for `(wid, cited_by_count)`, ranks by citations
/// descending (doc 0 = most cited; ties broken by ascending id), truncates to
/// `limit` (0 = no limit), and returns the ranked rows. A row's position is its
/// doc ID. Shared by the single-pass and phased builds.
fn rank_rows(sources: &[Source], limit: usize, t0: Instant) -> Vec<(u64, i64)> {
    let mut rows: Vec<(u64, i64)> = sources.par_iter().flat_map_iter(rank_source).collect();
    check_stream_truncations("pass 1 (ranking)");
    info!(
        rows = rows.len(),
        elapsed_s = t0.elapsed().as_secs_f64(),
        "pass-1 scan complete"
    );
    rows.par_sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    rows
}

/// Which per-split body encoding a split-set build uses: trigram `RRS` (`-split-set`) or
/// term/FST `RRTI` (`-term-splits`).
#[derive(Clone, Copy, PartialEq)]
enum SplitBody {
    Trigram,
    Term,
}

/// A split-set builder over either body kind. Both inner builders expose the same body-agnostic
/// build calls (`add_faceted`, `drain_sealed`, `finish`), so the chunked re-stream in
/// [`build_split_set`] is shared verbatim between the trigram and term variants.
enum SplitBuilder {
    Trigram(roaringrange::SplitSetBuilder),
    Term(roaringrange::TermSplitSetBuilder),
}

impl SplitBuilder {
    fn add_faceted(&mut self, text: &str, facets: &[(String, String)]) -> std::io::Result<u32> {
        match self {
            SplitBuilder::Trigram(b) => b.add_faceted(text, facets),
            SplitBuilder::Term(b) => b.add_faceted(text, facets),
        }
    }

    fn drain_sealed(&mut self) -> (roaringrange::NamedFiles, roaringrange::NamedFiles) {
        match self {
            SplitBuilder::Trigram(b) => b.drain_sealed(),
            SplitBuilder::Term(b) => b.drain_sealed(),
        }
    }

    fn finish(self) -> std::io::Result<roaringrange::BuiltSplitSet> {
        match self {
            SplitBuilder::Trigram(b) => b.finish(),
            SplitBuilder::Term(b) => b.finish(),
        }
    }
}

/// Builds an `RRSS` split set (the `-split-set` / `-term-splits` modes): rank the works by
/// citations (doc 0 = most cited), then build the split set in `-chunks` contiguous doc-ID ranges.
/// Each chunk re-streams the sources (in parallel, keeping only that chunk's works), buffers its
/// text/facets/records in doc-ID order, and feeds them to a single streaming [`SplitBuilder`]
/// (trigram or term per `body`) whose open split carries
/// across chunk boundaries; sealed splits are drained to `-out` as they seal and records stream
/// to an `RRSR` store in global doc-ID order. So peak RAM is **one open split (≤ cap) + one
/// chunk's buffered text**, never the whole corpus — `-chunks K` builds the full 484 M corpus on
/// a bounded box (more chunks = less RAM, at K re-streams of the input). `-chunks 1` (default) is
/// the single-pass path, fine for subsets / large-RAM boxes.
///
/// Trigram (`-split-set`) writes `‹prefix›.rrss`, the split `‹prefix›-s*.rrs`/`.rrf`,
/// `‹prefix›-records.{idx,bin}`, and `‹prefix›.rrhc` (the boot bundle inlining every split's boot
/// region, for `RrssIndex.openBundle`); term (`-term-splits`) writes the same with `-s*.rrt`
/// bodies but no `.rrhc` (term split bodies have no from-boot path yet).
/// Flags: `-out <dir>`, `-split-prefix <p>` (default `openalex`), `-split-cap <bytes>`
/// (default 512 MiB). Trigram-only: `-bloom-bits <n>` (default 10; 0 disables). Term-only:
/// `-stem` (Snowball English) and `-stopwords` (drop stop words) — `-bloom-bits`/`-gram` are
/// ignored in term mode (no term Bloom yet).
///
/// On the cap (trigram): the per-split term Bloom is held **resident in the manifest**, and a
/// rank-tiered split spans ~the full trigram vocabulary, so the manifest's bloom bytes ≈
/// `distinct_trigrams_per_split × bits`, *repeated per split* — i.e. manifest/boot cost grows
/// with the split *count*. A small cap (many splits) bloats the boot; the default targets a
/// handful of large splits for a lean manifest. To trade pruning for an even leaner manifest,
/// raise `-split-cap` or lower `-bloom-bits` (or `0` to drop blooms entirely — a rank-tiered
/// split set already prunes the cold tiers by rank).
fn build_split_set(
    args: &[String],
    sources: &[Source],
    limit: usize,
    abstract_cap: usize,
    t0: Instant,
    body: SplitBody,
) {
    use roaringrange::{
        Language, Policy, SplitBuildConfig, SplitSetBuilder, TermSplitBuildConfig,
        TermSplitSetBuilder,
    };

    let out_dir = PathBuf::from(arg(args, "-out", "/tmp/openalex-split"));
    let prefix = arg(args, "-split-prefix", "openalex");
    let cap: u64 = arg(args, "-split-cap", "536870912")
        .parse()
        .unwrap_or(512 << 20);
    let bloom_bits: u32 = arg(args, "-bloom-bits", "10").parse().unwrap_or(10);
    let stem = flag(args, "-stem");
    let stopwords = flag(args, "-stopwords");
    let chunks: usize = arg(args, "-chunks", "1").parse().unwrap_or(1).max(1);
    std::fs::create_dir_all(&out_dir).expect("create -out dir");

    // Pass 1: rank → doc IDs (doc 0 = most cited).
    let rows = rank_rows(sources, limit, t0);
    let n = rows.len();
    if n == 0 {
        warn!("no works ranked");
        std::process::exit(1);
    }
    let id_to_doc: HashMap<u64, u32> = rows
        .iter()
        .enumerate()
        .map(|(i, (wid, _))| (*wid, i as u32))
        .collect();
    drop(rows);
    let chunk_size = n.div_ceil(chunks);
    info!(
        works = n,
        chunks, chunk_size, "ranked; building split set (re-streaming per chunk)"
    );

    // One streaming builder + record store, both fed in global doc-ID order across chunks, so
    // peak RAM is one open split (≤ cap) plus one chunk's buffered text. The builder's body kind
    // (trigram RRS vs term RRTI) is the only thing that differs between the two modes.
    let mut b = match body {
        SplitBody::Trigram => SplitBuilder::Trigram(SplitSetBuilder::new(SplitBuildConfig {
            policy: Policy::Tiered,
            byte_cap: cap,
            byte_cap_max: 0,
            gram_size: GRAM as u16,
            head_boundary: 0,
            stride: 0,
            name_prefix: prefix.clone(),
            sortcol: None,
            bloom_bits_per_key: bloom_bits,
            case_sensitive: false,
        })),
        SplitBody::Term => SplitBuilder::Term(TermSplitSetBuilder::new(TermSplitBuildConfig {
            policy: Policy::Tiered,
            byte_cap: cap,
            byte_cap_max: 0,
            head_boundary: 0,
            name_prefix: prefix.clone(),
            sortcol: None,
            language: stem.then_some(Language::English),
            stopwords,
            case_sensitive: false,
        })),
    };
    let mut rec_w = RecordWriter::new(
        BufWriter::with_capacity(
            1 << 20,
            File::create(out_dir.join(format!("{prefix}-records.bin"))).unwrap(),
        ),
        BufWriter::with_capacity(
            1 << 20,
            File::create(out_dir.join(format!("{prefix}-records.idx"))).unwrap(),
        ),
        n as u32,
    )
    .expect("create record store");
    let write_files = |files: Vec<(String, Vec<u8>)>| {
        for (name, bytes) in files {
            std::fs::write(out_dir.join(&name), &bytes).expect("write split file");
        }
    };

    // Pass 2: per doc-ID-range chunk, re-stream + parse the sources in parallel (keeping only
    // this chunk's works), then feed the chunk's docs to the builder/record store in order.
    type ChunkDoc = (usize, String, Vec<(String, String)>, Vec<u8>);
    for c in 0..chunks {
        let lo = c * chunk_size;
        if lo >= n {
            break;
        }
        let hi = (lo + chunk_size).min(n);
        let span = hi - lo;

        let per_src: Vec<Vec<ChunkDoc>> = sources
            .par_iter()
            .map(|src| {
                let mut out: Vec<ChunkDoc> = Vec::new();
                let reader = match open_source(src) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(source = %src.label(), error = %e, "skipping unreadable source");
                        return out;
                    }
                };
                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(e) => {
                            note_stream_truncation(&src.label(), &e);
                            break;
                        }
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
                        Some(d) => *d as usize,
                        None => continue,
                    };
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
                    let facets = (0..FACET_FIELDS.len())
                        .filter_map(|fi| {
                            let v = facet_value(&w, fi, &topic);
                            (!v.is_empty()).then(|| (FACET_FIELDS[fi].to_string(), v))
                        })
                        .collect();
                    let rec = build_record(
                        trim_openalex_id(&w.id),
                        title,
                        &authors,
                        w.publication_year.unwrap_or(0),
                        venue,
                        w.cited_by_count.unwrap_or(0),
                        &abstract_,
                        abstract_cap,
                    );
                    out.push((docid - lo, text, facets, rec));
                }
                out
            })
            .collect();
        check_stream_truncations("split-set chunk scan");

        // Place into chunk-local doc order (gaps stay empty: a keyword-less, recordless doc).
        let mut text = vec![String::new(); span];
        let mut facets: Vec<Vec<(String, String)>> = vec![Vec::new(); span];
        let mut recs: Vec<Vec<u8>> = vec![Vec::new(); span];
        for v in per_src {
            for (i, t, f, r) in v {
                text[i] = t;
                facets[i] = f;
                recs[i] = r;
            }
        }

        // Feed in global doc-ID order; the open split carries across chunk boundaries.
        for i in 0..span {
            b.add_faceted(&text[i], &facets[i])
                .expect("add work to split set");
            rec_w.write(&recs[i]).expect("write record");
        }
        let (s, f) = b.drain_sealed();
        write_files(s);
        write_files(f);
        info!(
            chunk = c,
            lo,
            hi,
            elapsed_s = t0.elapsed().as_secs_f64(),
            "chunk indexed"
        );
    }

    rec_w.flush().expect("flush records");
    let built = b.finish().expect("finish split set");
    let splits = built.splits.len();
    // RRHC boot bundle (trigram only): inline every split's boot region so the demo boots the
    // whole set with the per-split header GETs collapsed into one `.rrhc` (RrssIndex.openBundle).
    // Term (`.rrt`) splits have no from_boot path yet, so they get no bundle. Emit before
    // `write_files` moves the splits Vec (the writer needs `&built`).
    if body == SplitBody::Trigram {
        let mut rrhc = Vec::new();
        roaringrange::write_splitset_bundle(&mut rrhc, &built, 0, 1 << 20)
            .expect("write rrhc bundle");
        std::fs::write(out_dir.join(format!("{prefix}.rrhc")), &rrhc).expect("write rrhc");
    }
    std::fs::write(out_dir.join(format!("{prefix}.rrss")), &built.manifest)
        .expect("write manifest");
    write_files(built.splits);
    write_files(built.facets);

    info!(
        docs = n,
        splits,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "split-set build done -> {}",
        out_dir.display()
    );
}

/// Streams one source for pass 2: tokenizes each work and inserts its doc ID into
/// the shared sharded text bitmaps (one lock per touched shard). Facet postings are
/// accumulated in source-local maps (no lock) and returned for the caller to union.
fn build_source(
    src: &Source,
    id_to_doc: &HashMap<u64, u32>,
    shards: &[Mutex<HashMap<u64, RoaringBitmap>>],
    abstract_cap: usize,
) -> SourceOut {
    let mut recs = Vec::new();
    let mut dois = Vec::new();
    let mut facets: Vec<HashMap<String, RoaringBitmap>> =
        (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            warn!(source = %src.label(), error = %e, "skipping unreadable source");
            return SourceOut { recs, dois, facets };
        }
    };
    // Per-shard key buckets, reused across this source's works to batch locks.
    let mut buckets: Vec<Vec<u64>> = (0..KEY_SHARDS).map(|_| Vec::new()).collect();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                note_stream_truncation(&src.label(), &e);
                break;
            }
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

        // Facets: one bitmap per (field, value), accumulated source-locally.
        for (fi, map) in facets.iter_mut().enumerate() {
            let v = facet_value(&w, fi, &topic);
            if v.is_empty() {
                continue;
            }
            map.entry(v).or_default().insert(docid);
        }

        if let Some(doi) = normalize_doi(w.doi.as_deref()) {
            dois.push((doi, docid));
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
                &abstract_,
                abstract_cap,
            ),
        ));
    }
    SourceOut { recs, dois, facets }
}

/// Pass-2 worker for a chunk: like [`build_source`] but indexes only works whose
/// doc ID falls in `[lo, hi)`, returning that chunk's records and DOI lookup
/// pairs.
fn build_source_range(
    src: &Source,
    id_to_doc: &HashMap<u64, u32>,
    lo: u32,
    hi: u32,
    shards: &[Mutex<HashMap<u64, RoaringBitmap>>],
    abstract_cap: usize,
) -> SourceOut {
    let mut recs = Vec::new();
    let mut dois = Vec::new();
    let mut facets: Vec<HashMap<String, RoaringBitmap>> =
        (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
    let reader = match open_source(src) {
        Ok(r) => r,
        Err(e) => {
            warn!(source = %src.label(), error = %e, "skipping unreadable source");
            return SourceOut { recs, dois, facets };
        }
    };
    let mut buckets: Vec<Vec<u64>> = (0..KEY_SHARDS).map(|_| Vec::new()).collect();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                note_stream_truncation(&src.label(), &e);
                break;
            }
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

        for (fi, map) in facets.iter_mut().enumerate() {
            let v = facet_value(&w, fi, &topic);
            if v.is_empty() {
                continue;
            }
            map.entry(v).or_default().insert(docid);
        }

        if let Some(doi) = normalize_doi(w.doi.as_deref()) {
            dois.push((doi, docid));
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
                &abstract_,
                abstract_cap,
            ),
        ));
    }
    SourceOut { recs, dois, facets }
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

/// Number of records compressed per parallel batch in [`concat_chunk_records`].
/// Each batch builds its own dictionary-backed compressor once, so the batch must
/// be large enough to amortize that (a few hundred thousand records makes the
/// dictionary load negligible) yet small enough that a wave of `cores`× batches
/// fits comfortably in memory.
const RECORD_BATCH: usize = 200_000;

/// Concatenates the per-chunk record temps (written by [`write_chunk_records`] in
/// ascending chunk order, each chunk in doc-ID order) into the final record store.
/// The chunk temps in order reconstruct the global doc-ID sequence (chunks are
/// contiguous disjoint ranges).
///
/// With `zstd = None` the records are copied straight through a streaming
/// [`RecordWriter`] (no compression), byte-identical to [`write_records`]. With
/// `zstd = Some((dict, level))` compression — the build's heaviest single-threaded
/// step — is fanned across rayon: records are read in bounded waves, split into
/// batches, and each batch is framed by its own in-memory [`RecordWriter`] in
/// parallel; the batch blobs are then concatenated in order and the offset index
/// rebased to global positions. Because the framing and the `RRSR` header both come
/// from the crate's [`RecordWriter`] (the header is taken from a zero-record writer,
/// each frame from a per-batch writer), the output is byte-identical to the
/// single-threaded [`write_records_zstd`] store — same records, order, dictionary,
/// and per-record raw/zstd framing — just produced ~cores× faster.
fn concat_chunk_records(
    paths: &[PathBuf],
    n: usize,
    bin_path: &str,
    idx_path: &str,
    zstd: Option<(&[u8], i32)>,
) -> std::io::Result<()> {
    let (dict, level) = match zstd {
        Some(z) => z,
        None => {
            // Uncompressed: the streaming writer is already cheap (no compression).
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
            if writer.written() != n as u32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "record store holds {} records but the ranking expects {n} — \
                         chunk temps are stale or incomplete",
                        writer.written()
                    ),
                ));
            }
            return writer.flush();
        }
    };

    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(8);
    concat_zstd_parallel(
        paths,
        n,
        bin_path,
        idx_path,
        dict,
        level,
        RECORD_BATCH,
        RECORD_BATCH * cores * 2,
    )
}

/// Parallel zstd record concat (the compression path of [`concat_chunk_records`]).
/// Split out with explicit `batch_size`/`wave_records` so the multi-batch global
/// offset rebasing can be exercised with tiny values in tests.
#[allow(clippy::too_many_arguments)]
fn concat_zstd_parallel(
    paths: &[PathBuf],
    n: usize,
    bin_path: &str,
    idx_path: &str,
    dict: &[u8],
    level: i32,
    batch_size: usize,
    wave_records: usize,
) -> std::io::Result<()> {
    let mut bin_w = BufWriter::with_capacity(1 << 20, File::create(bin_path)?);
    let mut idx_w = BufWriter::with_capacity(1 << 20, File::create(idx_path)?);

    // The 16-byte RRSR header + leading off[0]=0, taken verbatim from the crate so
    // the layout can never drift from RecordWriter.
    {
        let mut hdr_bin = Vec::new();
        let mut hdr_idx = Vec::new();
        RecordWriter::new_zstd(&mut hdr_bin, &mut hdr_idx, n as u32, dict, level)?;
        idx_w.write_all(&hdr_idx)?;
    }

    let mut reader = RecordTempReader::new(paths);
    let mut global_base: u64 = 0;
    let mut total_records: usize = 0;

    loop {
        // Read a wave of raw records (bounded), then split into batches.
        let mut wave: Vec<Vec<u8>> = Vec::new();
        for _ in 0..wave_records {
            match reader.next_record()? {
                Some(rec) => wave.push(rec),
                None => break,
            }
        }
        if wave.is_empty() {
            break;
        }
        total_records += wave.len();
        let batches: Vec<&[Vec<u8>]> = wave.chunks(batch_size).collect();

        // Frame each batch in parallel via its own in-memory RecordWriter.
        let framed: Vec<(Vec<u8>, Vec<u8>)> = batches
            .par_iter()
            .map(|batch| {
                let mut bin = Vec::new();
                let mut idx = Vec::new();
                let mut w =
                    RecordWriter::new_zstd(&mut bin, &mut idx, batch.len() as u32, dict, level)
                        .expect("batch record writer");
                for rec in batch.iter() {
                    w.write(rec).expect("frame record");
                }
                w.flush().expect("flush batch");
                (bin, idx)
            })
            .collect();

        // Stitch in order: append each batch's bin, rebase its offsets to global.
        for (bin, idx) in framed {
            // idx = [16B header][off[0]=0][off[1..=len]]; record offsets start at byte 24.
            for off in idx[24..].chunks_exact(8) {
                let rel = u64::from_le_bytes(off.try_into().unwrap());
                idx_w.write_all(&(global_base + rel).to_le_bytes())?;
            }
            global_base += bin.len() as u64;
            bin_w.write_all(&bin)?;
        }
    }

    if total_records != n {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "record store holds {total_records} records but the ranking expects {n} — \
                 chunk temps are stale or incomplete"
            ),
        ));
    }
    bin_w.flush()?;
    idx_w.flush()
}

/// Streaming reader over the per-chunk record temps: yields one record's bytes per
/// call (`[len u32][bytes]` frames), advancing across files in order.
struct RecordTempReader {
    paths: Vec<PathBuf>,
    next: usize,
    cur: Option<BufReader<File>>,
}

impl RecordTempReader {
    fn new(paths: &[PathBuf]) -> Self {
        Self {
            paths: paths.to_vec(),
            next: 0,
            cur: None,
        }
    }

    fn next_record(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if self.cur.is_none() {
                if self.next >= self.paths.len() {
                    return Ok(None);
                }
                let p = &self.paths[self.next];
                self.next += 1;
                self.cur = Some(BufReader::with_capacity(1 << 20, File::open(p)?));
            }
            let r = self.cur.as_mut().unwrap();
            let mut lb = [0u8; 4];
            match r.read_exact(&mut lb) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.cur = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
            let mut bytes = vec![0u8; u32::from_le_bytes(lb) as usize];
            r.read_exact(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }
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

/// Marshals the stored record JSON (compact keys: id, t, a, y, v, c, ab) with the
/// same omit-empty rules as the Go loader. The `ab` (abstract) field is appended
/// only when `abstract_cap > 0` and the work has a non-empty reconstructed
/// abstract, truncated to `abstract_cap` bytes on a char boundary, so
/// `-abstract-cap 0` preserves the original lean record byte-for-byte.
#[allow(clippy::too_many_arguments)]
fn build_record(
    id: &str,
    title: &str,
    authors: &str,
    year: i64,
    venue: &str,
    cited: i64,
    abstract_: &str,
    abstract_cap: usize,
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
    if abstract_cap > 0 && !abstract_.is_empty() {
        let ab = cap_str(abstract_, abstract_cap);
        if !ab.is_empty() {
            s.push_str(",\"ab\":");
            s.push_str(&json_str(ab));
        }
    }
    s.push('}');
    s.into_bytes()
}

/// Truncates `s` to at most `cap` bytes on a UTF-8 char boundary.
fn cap_str(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// JSON-encodes a string (quoted + escaped) via serde_json.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Normalizes an OpenAlex DOI for the lookup index: strips a leading
/// `https://doi.org/` or `http://doi.org/` URL prefix (case-insensitively),
/// yielding the bare DOI like `10.1234/abcd`. Returns `None` when the input is
/// absent or empty after stripping. Casing/punctuation beyond the prefix are left
/// to the reader's `normalize_id` (which uppercases letters and drops
/// non-alphanumerics); the writer applies it identically, so no extra lowercasing
/// is needed here.
fn normalize_doi(doi: Option<&str>) -> Option<String> {
    let raw = doi?.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let bare = if let Some(rest) = lower.strip_prefix("https://doi.org/") {
        &raw[raw.len() - rest.len()..]
    } else if let Some(rest) = lower.strip_prefix("http://doi.org/") {
        &raw[raw.len() - rest.len()..]
    } else {
        raw
    };
    if bare.is_empty() {
        None
    } else {
        Some(bare.to_string())
    }
}

/// Writes the `(DOI, docID)` pairs to the `RRIL` exact-lookup sidecar at `path`.
fn write_doi_lookup(dois: &[(String, u32)], path: &str) {
    let out = BufWriter::with_capacity(1 << 20, File::create(path).expect("create lookup"));
    write_lookup(out, dois).expect("write lookup");
}

/// Number of records sampled to train the dictionary (capped at
/// [`ZSTD_DICT_SAMPLE_CAP`]).
fn sample_count(total: usize) -> usize {
    total.min(ZSTD_DICT_SAMPLE_CAP)
}

/// Trains a shared zstd dictionary from a sample of the record bytes. Samples
/// every Nth non-empty record (stride chosen so at most [`ZSTD_DICT_SAMPLE_CAP`]
/// records are used) to bound memory and training time while still spanning the
/// corpus, then trains a dictionary capped at `max_dict_bytes`.
fn train_dict_from_records(records: &[Vec<u8>], max_dict_bytes: usize) -> Vec<u8> {
    let total = records.len();
    let stride = total.div_ceil(ZSTD_DICT_SAMPLE_CAP).max(1);
    let samples: Vec<&[u8]> = records
        .iter()
        .step_by(stride)
        .filter(|r| !r.is_empty())
        .map(|r| r.as_slice())
        .collect();
    train_record_dict(&samples, max_dict_bytes).expect("train record dict")
}

/// Returns the value following `flag` in `args`, or `default`.
fn arg(args: &[String], flag: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// Returns whether the boolean `flag` is present in `args`.
fn flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// File size in bytes, or 0 if it can't be stat'd.
fn file_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use roaringrange::{MemoryFetch, RecordStore};

    /// A chunked compressed store (raw temps concatenated through the zstd writer) is
    /// byte-identical to the single-pass [`write_records_zstd`] store on the same
    /// records and dictionary, and round-trips through the reader — including the
    /// zero-length record.
    #[test]
    fn chunked_zstd_store_matches_single_pass_and_round_trips() {
        // Enough realistic, repetitive records for the dictionary trainer; one empty
        // sprinkled in periodically.
        let recs: Vec<Vec<u8>> = (0..3000)
            .map(|i| {
                if i % 137 == 0 {
                    Vec::new()
                } else {
                    format!(
                        "{{\"id\":\"W{i}\",\"t\":\"a study of widget number {i} in context\",\"a\":\"Smith, J; Doe, A\",\"y\":{},\"v\":\"Journal of Widgets\",\"c\":{}}}",
                        1990 + (i % 35),
                        i % 500
                    )
                    .into_bytes()
                }
            })
            .collect();
        let n = recs.len();
        let level = 19;

        // The dictionary sample is the single-pass selection (every stride-th
        // non-empty record). The phased builder re-derives this same set from the
        // record temps (see phased::tests::sample_from_temps_matches_single_pass_selection).
        let stride = n.div_ceil(ZSTD_DICT_SAMPLE_CAP).max(1);
        let mid = n / 2;
        let sample_refs: Vec<&[u8]> = recs
            .iter()
            .step_by(stride)
            .filter(|r| !r.is_empty())
            .map(|r| r.as_slice())
            .collect();
        let dict = train_record_dict(&sample_refs, 8192).expect("train dict");

        // Single-pass compressed store.
        let mut bin_s = Vec::new();
        let mut idx_s = Vec::new();
        write_records_zstd(&mut bin_s, &mut idx_s, &recs, &dict, level).unwrap();

        // Chunked: two raw temps, then concat through the zstd writer.
        let dir = std::env::temp_dir();
        let p0 = dir.join("rr_czstd_p0.recs");
        let p1 = dir.join("rr_czstd_p1.recs");
        write_chunk_records(&p0, &recs[0..mid]).unwrap();
        write_chunk_records(&p1, &recs[mid..n]).unwrap();
        let bin_p = dir.join("rr_czstd.bin");
        let idx_p = dir.join("rr_czstd.idx");
        concat_chunk_records(
            &[p0.clone(), p1.clone()],
            n,
            bin_p.to_str().unwrap(),
            idx_p.to_str().unwrap(),
            Some((&dict, level)),
        )
        .unwrap();
        let bin_c = std::fs::read(&bin_p).unwrap();
        let idx_c = std::fs::read(&idx_p).unwrap();

        assert_eq!(idx_c, idx_s, "chunked idx differs from single-pass");
        assert_eq!(bin_c, bin_s, "chunked bin differs from single-pass");

        // Round-trips through the reader with the shared dictionary.
        let store = block_on(RecordStore::open_with_dict(
            MemoryFetch::new(idx_c),
            MemoryFetch::new(bin_c),
            dict,
        ))
        .unwrap();
        assert_eq!(store.len() as usize, n);
        for (d, rec) in recs.iter().enumerate() {
            let got = block_on(store.get(d as u32)).unwrap().unwrap_or_default();
            assert_eq!(&got, rec, "record {d} round-trip mismatch");
        }

        for p in [p0, p1, bin_p, idx_p] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The parallel zstd concat with a tiny batch/wave (so records span multiple
    /// batches AND multiple waves) is byte-identical to the single-threaded
    /// [`write_records_zstd`]. This exercises the global-offset rebasing that the
    /// single-batch test above does not.
    #[test]
    fn parallel_concat_multibatch_matches_single_pass() {
        // 45 records spanning several batches (size 8) and waves (16 records each);
        // periodic empties to cover the zero-length frame across a batch boundary.
        let recs: Vec<Vec<u8>> = (0..45)
            .map(|i| {
                if i % 11 == 0 {
                    Vec::new()
                } else {
                    format!("{{\"id\":\"W{i}\",\"t\":\"record number {i} of the widget corpus\",\"c\":{}}}", i * 7)
                        .into_bytes()
                }
            })
            .collect();
        let n = recs.len();
        let level = 19;
        let sample: Vec<&[u8]> = recs
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| r.as_slice())
            .collect();
        let dict = train_record_dict(&sample, 4096).expect("train dict");

        // Single-threaded reference store.
        let mut bin_s = Vec::new();
        let mut idx_s = Vec::new();
        write_records_zstd(&mut bin_s, &mut idx_s, &recs, &dict, level).unwrap();

        // Parallel concat from two temps, forced to tiny batch (8) + wave (16).
        let dir = std::env::temp_dir();
        let p0 = dir.join("rr_pmb_p0.recs");
        let p1 = dir.join("rr_pmb_p1.recs");
        let mid = 20;
        write_chunk_records(&p0, &recs[0..mid]).unwrap();
        write_chunk_records(&p1, &recs[mid..n]).unwrap();
        let bin_p = dir.join("rr_pmb.bin");
        let idx_p = dir.join("rr_pmb.idx");
        concat_zstd_parallel(
            &[p0.clone(), p1.clone()],
            n,
            bin_p.to_str().unwrap(),
            idx_p.to_str().unwrap(),
            &dict,
            level,
            8,  // batch_size -> ~6 batches
            16, // wave_records -> ~3 waves
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&idx_p).unwrap(),
            idx_s,
            "parallel idx differs from single-pass"
        );
        assert_eq!(
            std::fs::read(&bin_p).unwrap(),
            bin_s,
            "parallel bin differs from single-pass"
        );

        for p in [p0, p1, bin_p, idx_p] {
            let _ = std::fs::remove_file(p);
        }
    }
}
