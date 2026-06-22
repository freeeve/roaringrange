//! Builds the **v3 trigram monolith** (`openalex-full.rrs`) over an `RRSR` record store — the
//! single-index sibling of `build_trigram_splitset`. Where the split-set builder seals many
//! byte-capped `RRS` splits, this folds the whole corpus into one ordinary `RRS` v3 index via the
//! chunked partial→merge path (`build::chunk`), so peak memory is one doc-ID chunk rather than the
//! whole 100+ GB index.
//!
//!   cargo run --release --features zstd --example build_trigram_monolith -- \
//!       records.idx records.bin records.dict <N> <out.rrs> \
//!       [chunk_docs=8000000] [work_dir=<out>.rrwork]
//!
//! Records are JSON in descending rank, so doc id == rank and `0..n` is the top-`n` corpus. The
//! indexed text mirrors `build_trigram_splitset`'s `record_text` exactly —
//! `"<title> <abstract> <authors> <venue>"` — so the monolith and the split set index byte-identical
//! trigrams over the same doc-ID space. Only the `.rrs` (trigram postings) is produced: the v3
//! head/tail collapse changed `.rrs` alone, so the existing `openalex-full.rrf`/`.rril`/records on
//! the same rank order stay valid and are reused (no facet sidecar is written here).
//!
//! Build shape (mirrors the OpenAlex builder's `phased` path): the doc-ID space is cut into
//! `chunk_docs`-sized chunks; each chunk's trigram postings are accumulated into 256 key-sharded
//! maps (fanned across `rayon`, one sub-batch of records per task), serialized to a key-sorted
//! partial on disk, then `merge_partials_to_rrs` streams the disjoint partials into one v3 `.rrs`.
//! A chunk whose partial already exists is skipped, so an interrupted build resumes where it left off.

use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::chunk::{merge_partials_to_rrs, write_partial};
use roaringrange::build::DEFAULT_STRIDE;
use roaringrange::ngram_keys;
use roaringrange::records::RecordStore;
use roaringrange::FileFetch;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Trigram size (must match the reader/query path and the split set).
const GRAM: usize = 3;

/// A fast hasher for the `u64` trigram keys: Fibonacci (multiplicative) mixing instead of std's
/// SipHash. The per-chunk fold does billions of `entry()` lookups, where SipHash dominates; the
/// keys are already well-distributed packed trigrams, so a single multiply suffices.
#[derive(Default)]
struct U64Hasher(u64);

impl Hasher for U64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    /// Byte fallback (FNV-1a); not on the hot path since `u64` keys hash via `write_u64`.
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
        self.0 = h;
    }
}

/// `HashMap` keyed by trigram with the fast [`U64Hasher`].
type PostingMap = HashMap<u64, RoaringBitmap, BuildHasherDefault<U64Hasher>>;
/// Records fetched+tokenized per parallel sub-batch. Only the I/O-bound get+decode+tokenize is
/// fanned across `rayon` (the proven `build_trigram_splitset` pattern); the bitmap build that
/// consumes each sub-batch is single-threaded, so all `RoaringBitmap` mutation is race-free.
const SUBBATCH: u32 = 50_000;
/// Default chunk width; ~8M docs is roughly a 2 GB partial and keeps peak chunk RAM well within box
/// limits, while leaving the partial count far under the open-fd ceiling the merge needs.
const DEFAULT_CHUNK_DOCS: u32 = 8_000_000;

/// Builds one record's indexed text — `"<title> <abstract> <authors> <venue>"`, skipping empty
/// fields — byte-identical to `build_trigram_splitset`'s `record_fields` text so the monolith and
/// the split set index the same trigrams. A record that fails to parse contributes empty text (it
/// still consumes a doc id, keeping the doc-id space dense and aligned with records/`.rrf`).
fn record_text(bytes: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
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
    text
}

/// Accumulates the trigram postings for the doc-ID range `[lo, hi)` into a key-sorted partial at
/// `partial_path`, returning the number of distinct trigram keys written.
///
/// Only the I/O-bound part — record `get` (pread + libzstd decode) and `ngram_keys` tokenization —
/// is fanned across `rayon`, one [`SUBBATCH`] of doc IDs at a time. The resulting `(doc, keys)` are
/// folded into the posting map **single-threaded**, so every `RoaringBitmap` is built and serialized
/// on one thread (no concurrent bitmap mutation, no parallel collect). This mirrors the proven
/// `build_trigram_splitset` structure; doing the bitmap work in parallel raced and corrupted the
/// heap. Peak memory is the chunk's posting map plus one in-flight sub-batch of key lists.
fn build_chunk_partial(
    store: &RecordStore<FileFetch>,
    lo: u32,
    hi: u32,
    partial_path: &Path,
) -> usize {
    let mut map: PostingMap = PostingMap::default();

    let mut start = lo;
    while start < hi {
        let end = (start + SUBBATCH).min(hi);
        // Parallel get+decode+tokenize for this sub-batch; bitmap insertion is deferred to the
        // single-threaded fold below (trigram sets are order-independent, so no sort is needed).
        let parsed: Vec<(u32, Vec<u64>)> = (start..end)
            .into_par_iter()
            .map(|id| {
                let bytes = futures::executor::block_on(store.get(id))
                    .expect("get record")
                    .expect("record present");
                (id, ngram_keys(&record_text(&bytes), GRAM))
            })
            .collect();
        for (id, keys) in &parsed {
            for &k in keys {
                map.entry(k).or_default().insert(*id);
            }
        }
        start = end;
    }

    // Serialize each key's whole (unsplit) bitmap single-threaded, then write the partial atomically
    // (`.tmp` then rename) so a resumed build trusts only whole files. `write_partial` key-sorts.
    let mut entries: Vec<(u64, Vec<u8>)> = Vec::with_capacity(map.len());
    for (k, bm) in map {
        let mut b = Vec::with_capacity(bm.serialized_size());
        bm.serialize_into(&mut b).expect("serialize posting");
        entries.push((k, b));
    }
    let ngrams = entries.len();

    let tmp = partial_path.with_extension("partial.tmp");
    let mut w = BufWriter::new(File::create(&tmp).expect("create partial tmp"));
    write_partial(&mut w, entries).expect("write partial");
    w.flush().expect("flush partial");
    drop(w);
    std::fs::rename(&tmp, partial_path).expect("rename partial");
    ngrams
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 || args.len() > 8 {
        eprintln!(
            "usage: build_trigram_monolith <records.idx> <records.bin> <records.dict> <N> <out.rrs> \
             [chunk_docs=8000000] [work_dir=<out>.rrwork]"
        );
        std::process::exit(2);
    }
    let idx_path = &args[1];
    let bin_path = &args[2];
    let dict_path = &args[3];
    let want_n: u32 = args[4].parse().expect("N");
    let out_rrs = PathBuf::from(&args[5]);
    let chunk_docs: u32 = args
        .get(6)
        .map(|s| s.parse().expect("chunk_docs"))
        .unwrap_or(DEFAULT_CHUNK_DOCS)
        .max(1);
    let work_dir = args
        .get(7)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{}.rrwork", out_rrs.display())));

    std::fs::create_dir_all(&work_dir).expect("create work dir");

    let dict = std::fs::read(dict_path).expect("read dict");
    let idx = FileFetch::open(idx_path).expect("open idx");
    let bin = FileFetch::open(bin_path).expect("open bin");
    let store = futures::executor::block_on(RecordStore::open_with_dict(idx, bin, dict))
        .expect("open record store");

    let n = want_n.min(store.len());
    let chunks = n.div_ceil(chunk_docs);
    eprintln!(
        "monolith build: {n} docs, {chunks} chunks x {chunk_docs} docs, work {}",
        work_dir.display()
    );

    let started = Instant::now();
    let mut partials: Vec<PathBuf> = Vec::with_capacity(chunks as usize);
    for c in 0..chunks {
        let lo = c * chunk_docs;
        let hi = (lo + chunk_docs).min(n);
        let partial_path = work_dir.join(format!("chunk_{c:05}.partial"));
        partials.push(partial_path.clone());

        // Resume: a whole partial from a prior run is trusted as-is.
        if std::fs::metadata(&partial_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            eprintln!("  chunk {c}/{chunks} [{lo}..{hi}) cached, skip");
            continue;
        }

        let tc = Instant::now();
        let ngrams = build_chunk_partial(&store, lo, hi, &partial_path);
        // Same field layout the dash parses from the split logs: "<done> / <total> docs | <files>
        // files written | <elapsed>s" plus a trailing trigram count for visibility.
        eprintln!(
            "  {hi} / {n} docs | {} files written ({ngrams} trigrams) | {:.1}s  (chunk {:.1}s)",
            c + 1,
            started.elapsed().as_secs_f64(),
            tc.elapsed().as_secs_f64(),
        );
    }

    // Merge the disjoint per-chunk partials into one v3 `.rrs`. Streams by key (peak = one key's
    // postings + the key dictionary), so the merge stays bounded regardless of corpus size.
    eprintln!(
        "merging {} partials -> {}",
        partials.len(),
        out_rrs.display()
    );
    let tm = Instant::now();
    let mut out = File::create(&out_rrs).expect("create out rrs");
    merge_partials_to_rrs(&partials, GRAM as u16, DEFAULT_STRIDE, &mut out)
        .expect("merge partials");
    out.flush().expect("flush rrs");
    let bytes = out.metadata().map(|m| m.len()).unwrap_or(0);

    eprintln!(
        "done: {n} docs -> {} ({:.1} GB) | merge {:.1}s | total {:.1}s",
        out_rrs.display(),
        bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        tm.elapsed().as_secs_f64(),
        started.elapsed().as_secs_f64(),
    );
    eprintln!("note: reuse the existing openalex-full.rrf / .rril (unchanged by v3); work dir {} can be removed", work_dir.display());
    eprintln!(
        "\n⚠ GATE before uploading: this build is only correct if `{idx}` is the S3-LIVE \
         records-full. The doc-id == records-position invariant silently breaks if a stale/divergent \
         local records was used. Verify against the live records (re-fetch if unsure):\n  \
         cargo run --release --features zstd --example verify_monolith_aligned -- \\\n    {out} \
         <records.idx> <records.bin> <records.dict>",
        idx = idx_path,
        out = out_rrs.display(),
    );
}
