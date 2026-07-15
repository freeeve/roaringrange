//! Builds a **term split-set** (`RRSS`, `bodyKind = 1`) over an `RRSR` record store —
//! fully native Rust, parallel, and bounded-RAM. Each sealed split is an `RRTI` term index
//! over a byte-capped band of top-ranked documents; the `.rrss` manifest names them all.
//!
//!   cargo run --release --features "splits terms" --example build_term_splitset -- \
//!       records.idx records.bin records.dict <N> <out_dir> \
//!       [byte_cap_mb=256] [language=english] [cap_max_mb=0] [workers=1]
//!
//! Records are JSON in descending rank, so doc id == rank and `0..n` is the top-`n` corpus.
//! For each doc the title (`"t"`) and abstract (`"ab"`) are concatenated into one text field,
//! tokenized by the term builder, and fed in rank order.
//!
//! `workers = 1` is the serial reference build: the JSON parse is fanned out across a `rayon`
//! pool per chunk and one sequential builder consumes the texts in rank order. `workers = K`
//! partitions `[0, N)` into K contiguous rank bands, each built by an independent thread with
//! its **own `RecordStore`** (own zstd decode context) and its **own `TermSplitSetBuilder`**
//! (its own tokenize + stem + insert), so the serial hot path runs once per core instead of
//! once total. The bands' splits are then merged into one rank-ordered manifest via
//! `merge_term_split_bands`. Either way, splits stream to disk via `drain_sealed` after every
//! chunk, so peak memory is one open split per worker (keep `K × byte_cap` well under RAM).

use rayon::prelude::*;
use roaringrange::records::RecordStore;
use roaringrange::{
    merge_term_split_bands, FileFetch, Language, Policy, TermSplitBuildConfig, TermSplitParts,
    TermSplitSetBuilder,
};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

/// The split-set name prefix: the manifest is `‹PREFIX›.rrss`, splits `‹PREFIX›-s00000.rrt`, ….
const PREFIX: &str = "openalex-484m-terms";
/// Docs fed between `drain_sealed` flushes. Trades parallelism against the serial path's
/// per-chunk transient memory (one batch of parsed texts resident at a time); 50k docs is a
/// few MB of text, well under the byte cap.
const CHUNK: u32 = 50_000;

/// Parses one record's JSON bytes into the indexed text: `"<title> <abstract>"`, trimmed. A
/// missing field is treated as empty; a record that fails to parse contributes empty text (it
/// still consumes a doc id, keeping the doc-id space dense and aligned with records/facets).
fn record_text(bytes: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
    let ab = v.get("ab").and_then(|x| x.as_str()).unwrap_or("");
    format!("{t} {ab}").trim().to_string()
}

/// Writes each `(filename, bytes)` blob into `dir`, dropping the bytes as it goes, and returns
/// the number of files and total bytes written. Freeing the drained blobs here keeps peak RAM at
/// one open split.
fn flush_blobs(dir: &Path, blobs: Vec<(String, Vec<u8>)>) -> (usize, u64) {
    let mut count = 0;
    let mut total = 0u64;
    for (name, bytes) in blobs {
        total += bytes.len() as u64;
        let mut f = File::create(dir.join(&name)).expect("create split file");
        f.write_all(&bytes).expect("write split file");
        count += 1;
    }
    (count, total)
}

/// The record-store files each band worker opens privately (its own fds and decode context).
struct StoreInputs<'a> {
    idx_path: &'a str,
    bin_path: &'a str,
    dict: &'a [u8],
}

/// Builds rank band `[lo, hi)` on one worker thread: its own `RecordStore` (own decode
/// context), its own builder rooted at global base `lo` under a per-band name prefix, docs fed
/// sequentially in rank order, sealed splits streamed to disk each chunk. Returns the band's
/// parts (blobs already flushed, so only specs + the facet flag remain) and the files/bytes
/// written.
fn build_band(
    inputs: &StoreInputs,
    lo: u32,
    hi: u32,
    out_dir: &Path,
    config: TermSplitBuildConfig,
    started: Instant,
) -> (TermSplitParts, usize, u64) {
    let idx = FileFetch::open(inputs.idx_path).expect("open idx");
    let bin = FileFetch::open(inputs.bin_path).expect("open bin");
    let store =
        futures::executor::block_on(RecordStore::open_with_dict(idx, bin, inputs.dict.to_vec()))
            .expect("open record store");
    let band_tag = config.name_prefix.clone();
    let mut b = TermSplitSetBuilder::new(config).with_global_base(lo);

    let mut files_written = 0usize;
    let mut bytes_written = 0u64;
    let mut start = lo;
    while start < hi {
        let end = (start + CHUNK).min(hi);
        for id in start..end {
            let bytes = futures::executor::block_on(store.get(id))
                .expect("get record")
                .expect("record present");
            let assigned = b.add_text(&record_text(&bytes)).expect("add doc");
            debug_assert_eq!(assigned, id);
        }
        let (splits, facets) = b.drain_sealed();
        let (sc, sb) = flush_blobs(out_dir, splits);
        let (fc, fb) = flush_blobs(out_dir, facets);
        files_written += sc + fc;
        bytes_written += sb + fb;
        eprintln!(
            "  [{band_tag}] {} / {} docs | {} files ({:.1} MB) | {:.1}s",
            end - lo,
            hi - lo,
            files_written,
            bytes_written as f64 / (1024.0 * 1024.0),
            started.elapsed().as_secs_f64(),
        );
        start = end;
    }

    let mut parts = b.finish_parts().expect("finish band");
    let (sc, sb) = flush_blobs(out_dir, std::mem::take(&mut parts.splits));
    let (fc, fb) = flush_blobs(out_dir, std::mem::take(&mut parts.facets));
    (parts, files_written + sc + fc, bytes_written + sb + fb)
}

/// The `workers > 1` path: K contiguous rank bands built by K independent threads, merged into
/// one manifest, split files renamed on disk into the global `‹PREFIX›-s#####` sequence (a
/// facet sidecar follows its split's stem). Returns the files/bytes written.
fn build_parallel(
    inputs: &StoreInputs,
    n: u32,
    workers: u32,
    out_dir: &Path,
    config: &TermSplitBuildConfig,
    started: Instant,
) -> (usize, u64) {
    let banded: Vec<(TermSplitParts, usize, u64)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                let lo = (n as u64 * w as u64 / workers as u64) as u32;
                let hi = (n as u64 * (w as u64 + 1) / workers as u64) as u32;
                let mut band_config = config.clone();
                band_config.name_prefix = format!("{PREFIX}-w{w:03}");
                s.spawn(move || build_band(inputs, lo, hi, out_dir, band_config, started))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("band worker panicked"))
            .collect()
    });

    let mut files_written = 0usize;
    let mut bytes_written = 0u64;
    let mut parts = Vec::with_capacity(banded.len());
    for (p, fc, fb) in banded {
        parts.push(p);
        files_written += fc;
        bytes_written += fb;
    }

    let (built, renames) = merge_term_split_bands(parts, config).expect("merge bands");
    for (old, new) in renames {
        std::fs::rename(out_dir.join(&old), out_dir.join(&new)).expect("rename split file");
        let old_rrf = old.replace(".rrt", ".rrf");
        if out_dir.join(&old_rrf).exists() {
            let new_rrf = new.replace(".rrt", ".rrf");
            std::fs::rename(out_dir.join(&old_rrf), out_dir.join(&new_rrf))
                .expect("rename facet sidecar");
        }
    }
    let manifest_name = format!("{PREFIX}.rrss");
    std::fs::write(out_dir.join(&manifest_name), &built.manifest).expect("write manifest");
    (files_written, bytes_written)
}

/// The `workers = 1` reference path: rayon-parallel JSON parse per chunk feeding one
/// sequential builder in strict rank order. Returns the files/bytes written.
fn build_serial(
    store: &RecordStore<FileFetch>,
    n: u32,
    out_dir: &Path,
    config: TermSplitBuildConfig,
    started: Instant,
) -> (usize, u64) {
    let mut b = TermSplitSetBuilder::new(config);
    let mut files_written = 0usize;
    let mut bytes_written = 0u64;

    let mut start = 0u32;
    while start < n {
        let end = (start + CHUNK).min(n);
        // Parse the chunk's records in parallel: each rayon worker pread's its own record and
        // JSON-parses it, returning (global_id, text). Sorting back into rank order keeps the
        // sequential builder feed in strict rank order regardless of completion order.
        let mut batch: Vec<(u32, String)> = (start..end)
            .into_par_iter()
            .map(|id| {
                let bytes = futures::executor::block_on(store.get(id))
                    .expect("get record")
                    .expect("record present");
                (id, record_text(&bytes))
            })
            .collect();
        batch.sort_unstable_by_key(|(id, _)| *id);

        for (id, text) in &batch {
            let assigned = b.add_text(text).expect("add doc");
            debug_assert_eq!(assigned, *id);
        }
        drop(batch);

        // Stream every split sealed during this chunk to disk and free its bytes.
        let (splits, facets) = b.drain_sealed();
        let (sc, sb) = flush_blobs(out_dir, splits);
        let (fc, fb) = flush_blobs(out_dir, facets);
        files_written += sc + fc;
        bytes_written += sb + fb;

        eprintln!(
            "  {} / {} docs | {} files written ({:.1} MB) | {:.1}s",
            end,
            n,
            files_written,
            bytes_written as f64 / (1024.0 * 1024.0),
            started.elapsed().as_secs_f64(),
        );
        start = end;
    }

    // Seal the final open split, write the remaining blobs, and write the manifest last.
    let built = b.finish().expect("finish");
    let (sc, sb) = flush_blobs(out_dir, built.splits);
    let (fc, fb) = flush_blobs(out_dir, built.facets);
    files_written += sc + fc;
    bytes_written += sb + fb;

    std::fs::write(out_dir.join(format!("{PREFIX}.rrss")), &built.manifest)
        .expect("write manifest");
    (files_written, bytes_written)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 || args.len() > 10 {
        eprintln!(
            "usage: build_term_splitset <records.idx> <records.bin> <records.dict> <N> <out_dir> \
             [byte_cap_mb=256] [language=english] [cap_max_mb=0 (geometric: caps double per tier \
             up to this)] [workers=1 (contiguous rank bands built in parallel)]"
        );
        std::process::exit(2);
    }
    let idx_path = &args[1];
    let bin_path = &args[2];
    let dict_path = &args[3];
    let want_n: u32 = args[4].parse().expect("N");
    let out_dir = Path::new(&args[5]);
    let byte_cap_mb: u64 = args
        .get(6)
        .map(|s| s.parse().expect("byte_cap_mb"))
        .unwrap_or(256);
    let language_arg = args.get(7).map(String::as_str).unwrap_or("english");
    let language = match language_arg {
        "none" | "" => None,
        other => match Language::from_code(other) {
            Some(l) => Some(l),
            None => {
                eprintln!("unknown language {other:?}; use e.g. `english`, `spanish`, or `none`");
                std::process::exit(2);
            }
        },
    };
    let cap_max_mb: u64 = args
        .get(8)
        .map(|s| s.parse().expect("cap_max_mb"))
        .unwrap_or(0);
    let workers: u32 = args
        .get(9)
        .map(|s| s.parse().expect("workers"))
        .unwrap_or(1)
        .max(1);

    std::fs::create_dir_all(out_dir).expect("create out dir");

    let dict = std::fs::read(dict_path).expect("read dict");
    let idx = FileFetch::open(idx_path).expect("open idx");
    let bin = FileFetch::open(bin_path).expect("open bin");
    let store = futures::executor::block_on(RecordStore::open_with_dict(idx, bin, dict.clone()))
        .expect("open record store");

    let n = want_n.min(store.len());
    let config = TermSplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap: byte_cap_mb * 1024 * 1024,
        byte_cap_max: cap_max_mb * 1024 * 1024,
        head_boundary: 0,
        name_prefix: PREFIX.to_string(),
        sortcol: None,
        language,
        stem: language.is_some(),
        stopwords: false,
        case_sensitive: false,
    };

    let started = Instant::now();
    let (files_written, bytes_written) = if workers > 1 {
        drop(store);
        let inputs = StoreInputs {
            idx_path,
            bin_path,
            dict: &dict,
        };
        build_parallel(&inputs, n, workers, out_dir, &config, started)
    } else {
        build_serial(&store, n, out_dir, config, started)
    };

    eprintln!(
        "done: {} docs -> {} split/facet files + {PREFIX}.rrss ({:.1} MB total) in {:.1}s \
         into {} ({} worker{})",
        n,
        files_written,
        bytes_written as f64 / (1024.0 * 1024.0),
        started.elapsed().as_secs_f64(),
        out_dir.display(),
        workers,
        if workers == 1 { "" } else { "s" },
    );
}
