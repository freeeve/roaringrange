//! Builds a **trigram split-set** (`RRSS`, `bodyKind = 0`) over an `RRSR` record store — the
//! trigram sibling of `build_term_splitset`. Each sealed split is a byte-capped `RRS` trigram
//! index over a band of top-ranked documents; the `.rrss` manifest names them all, and a
//! `.rrhc` boot bundle inlines every split's boot region for `RrssIndex.openBundle`.
//!
//!   cargo run --release --features splits --example build_trigram_splitset -- \
//!       records.idx records.bin records.dict <N> <out_dir> \
//!       [byte_cap_mb=512] [prefix=openalex] [bloom_bits=10]
//!
//! Records are JSON in descending rank, so doc id == rank and `0..n` is the top-`n` corpus.
//! For each doc the indexed text mirrors the monolith's `build_text` —
//! `"<title> <abstract> <authors> <venue>"` — and the `year` facet is recovered from the
//! record's `"y"` field (the only facet field the record store retains; type/oa/language/topic
//! are not stored, so they are left to a later `openalex-full.rrf` slice). Records are **not**
//! re-emitted: the split set is built over the record store's existing rank order, so the same
//! record store (`records-full`) serves both the monolith and this split set.
//!
//! The JSON parse is fanned out across a `rayon` pool per chunk; the builder's seal/insert stays
//! sequential (rank order is load-bearing). Splits stream to disk via `drain_sealed` after every
//! chunk, so peak memory is one open split rather than the whole split set.

use rayon::prelude::*;
use roaringrange::records::RecordStore;
use roaringrange::{write_splitset_bundle, FileFetch, Policy, SplitBuildConfig, SplitSetBuilder};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

/// Parses one record's JSON into the indexed text and its facets. Text is
/// `"<title> <abstract> <authors> <venue>"` (the monolith's `build_text`); facets carry the
/// `year` field (`"y"`) when present. A record that fails to parse contributes empty text and no
/// facets (it still consumes a doc id, keeping the doc-id space dense and aligned with records).
fn record_fields(bytes: &[u8]) -> (String, Vec<(String, String)>) {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return (String::new(), Vec::new()),
    };
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let (title, abstract_, authors, venue) = (g("t"), g("ab"), g("a"), g("v"));
    let mut text =
        String::with_capacity(title.len() + abstract_.len() + authors.len() + venue.len() + 3);
    text.push_str(title);
    for f in [abstract_, authors, venue] {
        if !f.is_empty() {
            text.push(' ');
            text.push_str(f);
        }
    }
    let mut facets = Vec::new();
    if let Some(y) = v.get("y").and_then(|x| x.as_i64()) {
        if y != 0 {
            facets.push(("year".to_string(), y.to_string()));
        }
    }
    (text, facets)
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 || args.len() > 10 {
        eprintln!(
            "usage: build_trigram_splitset <records.idx> <records.bin> <records.dict> <N> <out_dir> \
             [byte_cap_mb=512] [prefix=openalex] [bloom_bits=10] [cap_max_mb=0 (geometric: caps double per tier up to this)]"
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
        .unwrap_or(512);
    let prefix = args
        .get(7)
        .cloned()
        .unwrap_or_else(|| "openalex".to_string());
    let bloom_bits: u32 = args
        .get(8)
        .map(|s| s.parse().expect("bloom_bits"))
        .unwrap_or(10);
    let cap_max_mb: u64 = args
        .get(9)
        .map(|s| s.parse().expect("cap_max_mb"))
        .unwrap_or(0);

    std::fs::create_dir_all(out_dir).expect("create out dir");

    let dict = std::fs::read(dict_path).expect("read dict");
    let idx = FileFetch::open(idx_path).expect("open idx");
    let bin = FileFetch::open(bin_path).expect("open bin");
    let store = futures::executor::block_on(RecordStore::open_with_dict(idx, bin, dict))
        .expect("open record store");

    let n = want_n.min(store.len());
    let byte_cap = byte_cap_mb * 1024 * 1024;

    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap,
        byte_cap_max: cap_max_mb * 1024 * 1024,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: prefix.clone(),
        sortcol: None,
        bloom_bits_per_key: bloom_bits,
    });

    // Chunk size trades parallelism against the rayon fan-out's transient memory (one batch of
    // parsed texts resident at a time); 50k docs is a few MB of text, well under the byte cap.
    const CHUNK: u32 = 50_000;
    let started = Instant::now();
    let mut files_written = 0usize;
    let mut bytes_written = 0u64;
    let mut splits_total = 0usize;

    let mut start = 0u32;
    while start < n {
        let end = (start + CHUNK).min(n);
        // Parse the chunk's records in parallel: each rayon worker pread's its own record and
        // JSON-parses it into (text, facets). Sorting back into rank order keeps the sequential
        // builder feed in strict rank order regardless of completion order.
        let mut batch: Vec<(u32, (String, Vec<(String, String)>))> = (start..end)
            .into_par_iter()
            .map(|id| {
                let bytes = futures::executor::block_on(store.get(id))
                    .expect("get record")
                    .expect("record present");
                (id, record_fields(&bytes))
            })
            .collect();
        batch.sort_unstable_by_key(|(id, _)| *id);

        for (id, (text, facets)) in &batch {
            let assigned = b.add_faceted(text, facets).expect("add doc");
            debug_assert_eq!(assigned, *id);
        }
        drop(batch);

        // Stream every split sealed during this chunk to disk and free its bytes.
        let (splits, facets) = b.drain_sealed();
        let (sc, sb) = flush_blobs(out_dir, splits);
        let (fc, fb) = flush_blobs(out_dir, facets);
        files_written += sc + fc;
        splits_total += sc;
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

    // Seal the final open split, then write the bundle, manifest, splits, and facet sidecars.
    let built = b.finish().expect("finish");
    splits_total += built.splits.len();

    // RRHC boot bundle: inline every split's boot region so the demo boots the whole set with the
    // per-split header GETs collapsed into one `.rrhc` (`RrssIndex.openBundle`). Trigram splits
    // have a from-boot path (term splits do not), so the bundle is written here unconditionally.
    let mut rrhc = Vec::new();
    write_splitset_bundle(&mut rrhc, &built, 0, 1 << 20).expect("write rrhc bundle");
    std::fs::write(out_dir.join(format!("{prefix}.rrhc")), &rrhc).expect("write rrhc");

    std::fs::write(out_dir.join(format!("{prefix}.rrss")), &built.manifest)
        .expect("write manifest");
    let (sc, sb) = flush_blobs(out_dir, built.splits);
    let (fc, fb) = flush_blobs(out_dir, built.facets);
    files_written += sc + fc;
    bytes_written += sb + fb;

    eprintln!(
        "done: {} docs -> {} splits ({} split/facet files + {}.rrss + {}.rrhc, {:.1} MB) in {:.1}s into {}",
        n,
        splits_total,
        files_written,
        prefix,
        prefix,
        bytes_written as f64 / (1024.0 * 1024.0),
        started.elapsed().as_secs_f64(),
        out_dir.display(),
    );
}
