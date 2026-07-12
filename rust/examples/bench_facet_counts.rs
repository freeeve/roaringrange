//! Benchmarks `FacetIndex::counts()` (head-only, in-memory, zero-fetch) vs
//! `counts_full()` (head + tail, range-fetched) over a real `.rrf` facet sidecar —
//! quantifying the cost the head+tail full-count fix added to `FilteredIds.facetCounts()`.
//! It wraps
//! the file fetcher to tally how many range-reads and bytes each path issues (each read
//! ≈ one browser network round-trip), plus wall-clock.
//!
//!   curl -o /tmp/search.rrf https://dev.deeplibby.com/artifacts/search.rrf
//!   cargo run --release --example bench_facet_counts -- /tmp/search.rrf [corpus_n] [stride]

use futures::executor::block_on;
use roaring::RoaringBitmap;
use roaringrange::facet::FacetIndex;
use roaringrange::{FetchError, FileFetch, RangeFetch};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// A `RangeFetch` wrapper that tallies reads + bytes (shared across clones).
#[derive(Clone)]
struct Counting<F> {
    inner: F,
    reads: Arc<AtomicUsize>,
    bytes: Arc<AtomicU64>,
}

impl<F: RangeFetch> RangeFetch for Counting<F> {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
        self.inner.read(offset, len).await
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: bench_facet_counts <search.rrf> [n] [stride]");
    let n: u32 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_774_281);
    let stride: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let fetch = Counting {
        inner: FileFetch::open(&path).expect("open .rrf"),
        reads: Arc::new(AtomicUsize::new(0)),
        bytes: Arc::new(AtomicU64::new(0)),
    };
    let snap = || {
        (
            fetch.reads.load(Ordering::Relaxed),
            fetch.bytes.load(Ordering::Relaxed),
        )
    };

    // Boot A: heads-loaded (the RrsIndex cursor path — `counts()` needs resident heads).
    let t = Instant::now();
    let _heads = block_on(FacetIndex::open(fetch.clone())).expect("open FacetIndex");
    println!(
        "open()       boot {:>9.2?}  reads +{}  bytes +{}",
        t.elapsed(),
        snap().0,
        snap().1
    );

    // Boot B: meta-only (the RrfFacets browse path). `facets()` (names +
    // full-corpus counts) is ready from this; postings load on demand at filter time.
    let (mr0, mb0) = snap();
    let t = Instant::now();
    let facets = block_on(FacetIndex::open_meta(fetch.clone())).expect("open_meta");
    let ncats: usize = facets.fields().iter().map(|f| f.categories.len()).sum();
    println!(
        "open_meta()  boot {:>9.2?}  reads +{}  bytes +{} | {} fields, {ncats} categories\n",
        t.elapsed(),
        snap().0 - mr0,
        snap().1 - mb0,
        facets.fields().len()
    );

    // A large, corpus-spanning filtered result (every `stride`-th doc), mimicking a
    // facet drill-down survivor set (e.g. the ~186K Spanish docs in the task repro).
    let result: RoaringBitmap = (0..n).step_by(stride).collect();
    let tail_buckets = (n / 65_536) as usize;
    println!(
        "result: {} docs spanning ~{tail_buckets} tail buckets\n",
        result.len()
    );

    let run = |label: &str, f: &dyn Fn()| {
        let (r0, b0) = snap();
        let t = Instant::now();
        f();
        let (r1, b1) = snap();
        println!(
            "{label:<22} {:>9.2?}  reads +{}  bytes +{}",
            t.elapsed(),
            r1 - r0,
            b1 - b0
        );
    };

    run("counts() head-only", &|| {
        let _ = facets.counts(&result);
    });
    run("counts_full(.., 0)", &|| {
        let _ = block_on(facets.counts_full(&result, 0)).expect("counts_full uncapped");
    });
    run("counts_full(.., 64)", &|| {
        let _ = block_on(facets.counts_full(&result, 64)).expect("counts_full capped");
    });
}
