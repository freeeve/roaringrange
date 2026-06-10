//! Live performance comparison across the demo's search backends, all range-fetched
//! from the live CloudFront origin over HTTP. For each query it reports per-mode
//! request count, bytes fetched, wall time, and hit count, so the modes can be
//! compared apples-to-apples on the *same* corpus and reader code the browser runs:
//!
//!   * trigram monolith (`.rrs`)     vs  trigram split set (`.rrss`)   — split / not-split
//!   * trigram (`.rrs`)  vs  term (`.rrt`)  vs  semantic (`.rrvi`)      — index type
//!   * each of the above with and without a facet filter                — facet support
//!
//! Reuses the curl-backed `RangeFetch` from `candidates.rs` (one `curl -r` per ranged
//! read, counting requests and bytes). Boot (the one-time resident download) is reported
//! separately from per-query search, mirroring the demo's boot-once / range-per-query model.
//!
//!   cargo run --release --features "terms splits vector" --example live_bench -- [BASE_URL]
//!
//! BASE_URL defaults to the live demo origin.

use futures::executor::block_on;
use roaringrange::{
    FacetIndex, FetchError, Index, Model2vec, RangeFetch, SplitFetcher, SplitSet, TermIndex,
    VectorIndex,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Shared request/byte counters; every fetch created for a measurement shares one
/// instance so a query's total spans the index file and (for splits) every split file.
#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    reqs: AtomicU64,
}

impl Counters {
    fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.reqs.store(0, Ordering::Relaxed);
    }
    fn snap(&self) -> (u64, u64) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.reqs.load(Ordering::Relaxed),
        )
    }
}

/// A [`RangeFetch`] that shells one `curl -r offset-end` per ranged read, counting requests
/// and bytes into shared [`Counters`] — the same pattern `candidates.rs` uses on the live index.
#[derive(Clone)]
struct CurlFetch {
    url: String,
    c: Arc<Counters>,
}

impl RangeFetch for CurlFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("{}-{}", offset, offset + len as u64 - 1);
        let out = std::process::Command::new("curl")
            .args(["-s", "--fail", "-r", &range, &self.url])
            .output()
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        if !out.status.success() {
            return Err(FetchError::Transport(format!(
                "curl {:?} {}",
                out.status, self.url
            )));
        }
        self.c.reqs.fetch_add(1, Ordering::Relaxed);
        self.c
            .bytes
            .fetch_add(out.stdout.len() as u64, Ordering::Relaxed);
        Ok(out.stdout)
    }
}

/// Resolves a split's data-file name against the split-set's base URL, sharing the counters.
struct CurlSplits {
    base: String,
    c: Arc<Counters>,
}

impl SplitFetcher for CurlSplits {
    type Fetch = CurlFetch;
    fn fetch_named(&self, name: &str) -> CurlFetch {
        CurlFetch {
            url: format!("{}/{}", self.base, name),
            c: self.c.clone(),
        }
    }
}

/// Downloads a whole small object (the model2vec matrix) in one request.
fn curl_whole(url: &str) -> Vec<u8> {
    let out = std::process::Command::new("curl")
        .args(["-s", "--fail", url])
        .output()
        .expect("curl");
    assert!(out.status.success(), "curl failed: {url}");
    out.stdout
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.2} MB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://openalex.evefreeman.com".to_string());
    let c = Arc::new(Counters::default());
    let fetch = |path: &str| CurlFetch {
        url: format!("{base}/{path}"),
        c: c.clone(),
    };

    let queries = [
        "machine learning",
        "crispr gene editing",
        "quantum computing",
        "deep residual learning",
    ];
    // A facet present in the OpenAlex sidecar; within-field categories OR, across fields AND.
    let facet: Vec<(String, String)> = vec![("type".into(), "article".into())];
    const K: usize = 25; // one result page
    const NPROBE: usize = 8;
    const SEM_K: usize = 250;

    eprintln!("== boot (one-time resident download) from {base} ==");
    macro_rules! boot {
        ($label:expr, $open:expr) => {{
            c.reset();
            let t = Instant::now();
            let r = block_on($open).expect(concat!("open ", $label));
            let (b, q) = c.snap();
            eprintln!(
                "  {:<14} {:>10}  {:>3} reqs  {:>5} ms",
                $label,
                fmt_bytes(b),
                q,
                t.elapsed().as_millis()
            );
            r
        }};
    }
    // The live monolith may still be RRSI v2 mid-migration while this reader is v3-only; skip it
    // (and its rows) rather than abort, so the readable modes still benchmark.
    let tri = {
        c.reset();
        let t = Instant::now();
        match block_on(Index::open(fetch("openalex-full.rrs"))) {
            Ok(r) => {
                let (b, q) = c.snap();
                eprintln!(
                    "  {:<14} {:>10}  {:>3} reqs  {:>5} ms",
                    "trigram .rrs",
                    fmt_bytes(b),
                    q,
                    t.elapsed().as_millis()
                );
                Some(r)
            }
            Err(e) => {
                eprintln!(
                    "  {:<14} SKIPPED — {e:?} (monolith not v3 yet)",
                    "trigram .rrs"
                );
                None
            }
        }
    };
    let term = boot!(
        "term .rrt",
        TermIndex::open(fetch("openalex-484m-stem.rrt"))
    );
    let vidx = boot!(
        "vector .rrvi",
        VectorIndex::open(fetch("openalex-484m.rrvi"))
    );
    let facets = boot!("facets .rrf", FacetIndex::open(fetch("openalex-full.rrf")));
    // The trigram split manifest is ~727 MB (Bloom-dominated), so opening it pulls that whole
    // blob resident — opt in with SPLIT_BENCH=1 only.
    let split = if std::env::var("SPLIT_BENCH").is_ok() {
        Some(boot!(
            "split .rrss",
            SplitSet::open(fetch("openalex-trigram-split/openalex.rrss"))
        ))
    } else {
        eprintln!(
            "  {:<14} SKIPPED — set SPLIT_BENCH=1 (manifest ~727 MB resident at boot)",
            "split .rrss"
        );
        None
    };
    let sres = CurlSplits {
        base: format!("{base}/openalex-trigram-split"),
        c: c.clone(),
    };
    let m2v_bytes = curl_whole(&format!("{base}/potion.rrm2"));
    eprintln!(
        "  model2vec .rrm2  {:>10}  (whole-file query embedder)",
        fmt_bytes(m2v_bytes.len() as u64)
    );
    let m2v = Model2vec::from_bytes(&m2v_bytes).expect("rrm2");
    if let Some(s) = split.as_ref() {
        eprintln!("  split set has {} splits", s.splits().len());
    }
    eprintln!();

    // Facet post-filter over a ranked doc-ID list in the monolith doc-ID space (trigram/term/
    // semantic share it). Fetches the selected category's posting(s) and keeps members — the
    // bytes/requests are counted, matching the demo's `filterIds`.
    let facet_filter = |ids: &[u32]| -> Vec<u32> {
        let rf = facets.resolve(&facet);
        if rf.is_empty() {
            return ids.to_vec();
        }
        let mask = block_on(rf.full_bitmap()).expect("facet bitmap");
        ids.iter()
            .copied()
            .filter(|id| mask.contains(*id))
            .collect()
    };

    println!(
        "{:<26} {:>6} {:>6} {:>11} {:>8}",
        "mode", "hits", "reqs", "bytes", "ms"
    );
    println!("{}", "-".repeat(62));

    let row = |label: &str, f: &dyn Fn() -> usize| {
        c.reset();
        let t = Instant::now();
        let n = f();
        let ms = t.elapsed().as_millis();
        let (b, q) = c.snap();
        println!(
            "{:<26} {:>6} {:>6} {:>11} {:>8}",
            label,
            n,
            q,
            fmt_bytes(b),
            ms
        );
    };

    for qy in queries {
        println!("\nquery: {qy:?}");
        if let Some(tri) = tri.as_ref() {
            row("  trigram mono", &|| {
                block_on(tri.search(qy, K)).unwrap().len()
            });
            row("  trigram mono +facet", &|| {
                facet_filter(&block_on(tri.search(qy, K)).unwrap()).len()
            });
        }
        if let Some(split) = split.as_ref() {
            row("  trigram split", &|| {
                block_on(split.search(&sres, qy, K)).unwrap().len()
            });
            row("  trigram split +facet", &|| {
                block_on(split.search_filtered(&sres, qy, &facet, K))
                    .unwrap()
                    .len()
            });
        }
        row("  term mono", &|| {
            block_on(term.search(qy, K)).unwrap().len()
        });
        row("  term mono +facet", &|| {
            facet_filter(&block_on(term.search(qy, K)).unwrap()).len()
        });
        let qv = m2v.embed(qy);
        row("  semantic", &|| {
            block_on(vidx.search(&qv, SEM_K, NPROBE)).unwrap().len()
        });
        row("  semantic +facet", &|| {
            let hits = block_on(vidx.search(&qv, SEM_K, NPROBE)).unwrap();
            let ids: Vec<u32> = hits.iter().map(|h| h.doc_id).collect();
            facet_filter(&ids).len()
        });
    }
}
