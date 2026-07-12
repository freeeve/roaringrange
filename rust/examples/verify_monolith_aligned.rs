//! Doc-ID alignment guard for a trigram `.rrs` (monolith or split) against a record
//! store. Samples doc IDs across the corpus and, for each, re-derives the doc's
//! trigrams from its record and confirms the `.rrs` actually lists that doc under its
//! own rarest trigrams. A `.rrs` built from a DIFFERENT records ordering than the one
//! given fails here — the live record at id `d` has different text than the index
//! built for `d`, so `d` is absent from its own trigram postings. Exits non-zero on
//! any mismatch.
//!
//!   cargo run --release --features zstd --example verify_monolith_aligned -- \
//!       openalex-full.rrs records.idx records.bin records.dict [samples=300]
//!   cargo run --release --features zstd --example verify_monolith_aligned -- --selftest
//!
//! Run it against the **S3-live** records before uploading a rebuilt monolith: that is
//! the gate that catches a records/monolith divergence. The
//! `doc-id == records position` invariant only holds when the build's records match
//! the records the index is served with.

use futures::executor::block_on;
use roaringrange::fetch::RangeFetch;
use roaringrange::index::IndexError;
use roaringrange::records::RecordStore;
use roaringrange::{ngram_keys, FileFetch, Index};

/// How many of a doc's rarest trigrams to intersect when checking membership. The
/// rarest trigrams have small, distinctive postings; a misaligned index has
/// essentially no chance of listing the exact doc id under all of them by accident.
const RAREST_K: usize = 8;
/// Default number of doc IDs to sample, spread evenly across the corpus (so deep ids —
/// where the original divergence showed — are covered, not just the popular head).
const DEFAULT_SAMPLES: u32 = 300;

/// One record's indexed text — MUST stay byte-identical to
/// `build_trigram_monolith::record_text` (and `build_trigram_splitset`'s): the
/// `"<title> <abstract> <authors> <venue>"` join over the `t`/`ab`/`a`/`v` JSON
/// fields. If this drifts from the builder, a correct index would falsely fail here.
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

/// Outcome of a verification pass.
struct Report {
    /// Docs whose text yielded trigrams and were checked against the index.
    checked: u32,
    /// Docs skipped because their record has no indexable text (no trigrams to check).
    skipped_empty: u32,
    /// Sampled doc IDs absent from their own rarest-trigram postings — the misaligned
    /// docs.
    mismatches: Vec<u32>,
}

/// Samples ~`samples` doc IDs evenly across `[0, store.len())` and verifies each is in
/// its own record's rarest-trigram postings within `idx`.
async fn verify<F: RangeFetch, G: RangeFetch>(
    idx: &Index<F>,
    store: &RecordStore<G>,
    samples: u32,
) -> Result<Report, IndexError> {
    let gram = idx.gram_size() as usize;
    let n = store.len();
    let mut rep = Report {
        checked: 0,
        skipped_empty: 0,
        mismatches: Vec::new(),
    };
    if n == 0 {
        return Ok(rep);
    }
    let step = (n / samples.max(1)).max(1);
    let mut d = 0u32;
    while d < n {
        let bytes = store.get(d).await?.unwrap_or_default();
        let text = record_text(&bytes);
        if ngram_keys(&text, gram).is_empty() {
            rep.skipped_empty += 1;
        } else {
            // Intersection of the rarest trigrams of d's own text. d is in EVERY one of
            // its trigrams' postings when aligned, so it must be in this intersection.
            let cands = idx.search_candidates(&text, RAREST_K).await?;
            if cands.binary_search(&d).is_err() {
                rep.mismatches.push(d);
            }
            rep.checked += 1;
        }
        match d.checked_add(step) {
            Some(next) => d = next,
            None => break,
        }
    }
    Ok(rep)
}

/// Builds a tiny aligned and a tiny misaligned index over the same in-memory record
/// store and asserts the verifier passes the first and catches the second.
fn run_selftest() {
    use roaring::RoaringBitmap;
    use roaringrange::build::{serialize_posting, write_index, write_records};
    use roaringrange::MemoryFetch;
    use std::collections::BTreeMap;

    let recs: Vec<Vec<u8>> = [
        r#"{"t":"consistently faster compressed bitmaps with roaring"}"#,
        r#"{"t":"optimizing druid with roaring bitmaps"}"#,
        r#"{"t":"trigram substring search index"}"#,
        r#"{"t":"observation of long-range proton collisions"}"#,
        r#"{"t":"product quantization vector recall"}"#,
        r#"{"t":"facet exclusion filters andnot"}"#,
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    let n = recs.len() as u32;
    let gram = 3usize;

    let mut bin = Vec::new();
    let mut idx_bytes = Vec::new();
    write_records(&mut bin, &mut idx_bytes, &recs).expect("write records");
    let store = block_on(RecordStore::open(
        MemoryFetch::new(idx_bytes),
        MemoryFetch::new(bin),
    ))
    .expect("open store");

    // Build an .rrs whose postings place doc d at `place(d)` — `identity` is aligned, a
    // shift is the misaligned case.
    let build = |place: &dyn Fn(u32) -> u32| -> Vec<u8> {
        let mut posts: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
        for d in 0..n {
            for k in ngram_keys(&record_text(&recs[d as usize]), gram) {
                posts.entry(k).or_default().insert(place(d));
            }
        }
        let entries: Vec<(u64, Vec<u8>)> = posts
            .iter()
            .map(|(k, bm)| (*k, serialize_posting(bm)))
            .collect();
        let mut out = Vec::new();
        write_index(&mut out, gram as u16, 2, entries).expect("write index");
        out
    };

    let aligned = block_on(Index::open(MemoryFetch::new(build(&|d| d)))).unwrap();
    let arep = block_on(verify(&aligned, &store, 100)).unwrap();
    assert!(arep.checked >= 1, "selftest should check some docs");
    assert_eq!(
        arep.mismatches.len(),
        0,
        "aligned index must verify clean, got mismatches {:?}",
        arep.mismatches
    );

    let shifted = block_on(Index::open(MemoryFetch::new(build(&|d| (d + 1) % n)))).unwrap();
    let mrep = block_on(verify(&shifted, &store, 100)).unwrap();
    assert!(
        !mrep.mismatches.is_empty(),
        "misaligned (shifted) index must be caught"
    );

    eprintln!(
        "selftest OK: aligned passes ({} checked, 0 mismatch); misaligned caught ({} mismatches)",
        arep.checked,
        mrep.mismatches.len()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--selftest") {
        run_selftest();
        return;
    }
    if args.len() < 4 || args.len() > 5 {
        eprintln!(
            "usage: verify_monolith_aligned <mono.rrs> <records.idx> <records.bin> <records.dict> [samples={DEFAULT_SAMPLES}]\n       verify_monolith_aligned --selftest"
        );
        std::process::exit(2);
    }
    let samples = args
        .get(4)
        .map(|s| s.parse().expect("samples"))
        .unwrap_or(DEFAULT_SAMPLES);

    let idx = block_on(Index::open(FileFetch::open(&args[0]).expect("open .rrs")))
        .expect("read .rrs header");
    let dict = std::fs::read(&args[3]).expect("read dict");
    let store = block_on(RecordStore::open_with_dict(
        FileFetch::open(&args[1]).expect("open records.idx"),
        FileFetch::open(&args[2]).expect("open records.bin"),
        dict,
    ))
    .expect("open record store");

    let rep = block_on(verify(&idx, &store, samples)).expect("verify");
    eprintln!(
        "verified {} docs ({} empty-text skipped) — {} mismatches",
        rep.checked,
        rep.skipped_empty,
        rep.mismatches.len()
    );
    if !rep.mismatches.is_empty() {
        let show = rep.mismatches.len().min(10);
        eprintln!(
            "MISALIGNED: doc ids absent from their own trigram postings (first {show}): {:?}",
            &rep.mismatches[..show]
        );
        eprintln!("This .rrs was built from a DIFFERENT records ordering than the records given. DO NOT upload.");
        std::process::exit(1);
    }
    if rep.checked == 0 {
        eprintln!("WARNING: 0 docs verified — every sampled record had empty text. Check inputs.");
        std::process::exit(3);
    }
    eprintln!("OK: monolith is doc-id-aligned with these records.");
}
