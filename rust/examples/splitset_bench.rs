//! Side-by-side benchmark: monolithic `RRS` vs a tiered `RRSS` split set over the **same**
//! synthetic corpus and query log — the table in `tasks/007_split_set_index.in-progress.md`.
//!
//! It builds one corpus twice (a single huge-cap split == today's monolith, and a small-cap
//! tiered split set), then runs each query through an instrumented in-memory `RangeFetch` that
//! counts bytes and requests. It reports boot cost and per-query cost (top-K, no filter) for a
//! common term and a rare term, so the tiered short-circuit's bandwidth win is visible.
//!
//!   cargo run --release --features splits --example splitset_bench
//!
//! Note: v1 opens each split fresh per query, so the tiered run re-pays tier-0's tiny boot on
//! every query; the planned `RRHC` boot bundle inlines those tier-0 headers, which this
//! harness does not yet model.

use futures::executor::block_on;
use roaringrange::{
    FetchError, Index, Policy, RangeFetch, SplitBuildConfig, SplitFetcher, SplitSet,
    SplitSetBuilder,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    reqs: AtomicU64,
}

impl Counters {
    fn reset(&self) {
        self.bytes.store(0, Relaxed);
        self.reqs.store(0, Relaxed);
    }
    fn read(&self) -> (u64, u64) {
        (self.bytes.load(Relaxed), self.reqs.load(Relaxed))
    }
}

/// An in-memory [`RangeFetch`] that counts the bytes and requests it serves.
#[derive(Clone)]
struct CountFetch {
    bytes: Arc<Vec<u8>>,
    c: Arc<Counters>,
}

impl RangeFetch for CountFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let (s, e) = (offset as usize, offset as usize + len);
        if e > self.bytes.len() {
            return Err(FetchError::OutOfRange {
                offset,
                len,
                available: self.bytes.len() as u64,
            });
        }
        self.c.reqs.fetch_add(1, Relaxed);
        self.c.bytes.fetch_add(len as u64, Relaxed);
        Ok(self.bytes[s..e].to_vec())
    }
}

/// A [`SplitFetcher`] over the named split blobs, sharing one [`Counters`].
struct CountResolver {
    files: HashMap<String, Arc<Vec<u8>>>,
    c: Arc<Counters>,
}

impl SplitFetcher for CountResolver {
    type Fetch = CountFetch;
    fn fetch_named(&self, name: &str) -> CountFetch {
        CountFetch {
            bytes: self
                .files
                .get(name)
                .cloned()
                .unwrap_or_else(|| Arc::new(Vec::new())),
            c: Arc::clone(&self.c),
        }
    }
}

/// Deterministic Zipf-ish corpus: `n` docs of 8 words each drawn from a 200-word vocabulary
/// biased toward low indices (so `term000` is common, `term199` rare). Fed later in index
/// order == rank order. No RNG crate — an xorshift keeps it reproducible.
fn corpus(n: usize) -> Vec<String> {
    let vocab: Vec<String> = (0..200).map(|w| format!("term{w:03}")).collect();
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..n)
        .map(|_| {
            (0..8)
                .map(|_| {
                    let r = (next() % 10_000) as f64 / 10_000.0;
                    // r*r biases toward 0 -> low-index (common) terms.
                    let idx = ((r * r) * vocab.len() as f64) as usize % vocab.len();
                    vocab[idx].as_str()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Builds a tiered split set over `docs` at `byte_cap` (with `bloom_bits_per_key` per-split
/// term Bloom filters, `0` to disable) and returns the manifest + split blobs.
fn build(
    docs: &[String],
    byte_cap: u64,
    bloom_bits_per_key: u32,
) -> (Vec<u8>, HashMap<String, Arc<Vec<u8>>>) {
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "bench".to_string(),
        sortcol: None,
        bloom_bits_per_key,
    });
    for d in docs {
        b.add_text(d).unwrap();
    }
    let built = b.finish().unwrap();
    let files = built
        .splits
        .into_iter()
        .map(|(name, bytes)| (name, Arc::new(bytes)))
        .collect();
    (built.manifest, files)
}

fn main() {
    let n = 50_000usize;
    let k = 10usize;
    let docs = corpus(n);
    let mb = |b: u64| b as f64 / 1024.0 / 1024.0;

    // Monolith: one huge-cap split == today's RRS. Tiered: small cap -> many splits, built
    // both without and with per-split term Bloom filters.
    let (_mono_manifest, mono_files) = build(&docs, 1 << 30, 0);
    let (rrss_manifest, rrss_files) = build(&docs, 64 * 1024, 0);
    let (bloom_manifest, bloom_files) = build(&docs, 64 * 1024, 10);
    let mono_bytes = Arc::clone(&mono_files["bench-s00000.rrs"]);

    let total =
        |fs: &HashMap<String, Arc<Vec<u8>>>| -> u64 { fs.values().map(|b| b.len() as u64).sum() };
    let open_ss = |m: &[u8]| {
        block_on(SplitSet::open(CountFetch {
            bytes: Arc::new(m.to_vec()),
            c: Arc::new(Counters::default()),
        }))
        .unwrap()
    };
    let rrss = open_ss(&rrss_manifest);
    let bloom = open_ss(&bloom_manifest);

    println!("corpus: {n} docs, top-{k} queries, gram=3");
    println!(
        "monolith   : 1 split,  {:.2} MB on S3",
        mb(mono_bytes.len() as u64)
    );
    println!(
        "rrss       : {} splits, {:.2} MB on S3",
        rrss.splits().len(),
        mb(total(&rrss_files))
    );
    println!(
        "rrss+bloom : {} splits, {:.2} MB on S3 (+{:.2} MB of Bloom filters in the manifest)\n",
        bloom.splits().len(),
        mb(total(&bloom_files)),
        mb(bloom_manifest.len() as u64 - rrss_manifest.len() as u64)
    );

    let row = |label: &str, m: (u64, u64), r: (u64, u64), b: (u64, u64)| {
        println!(
            "  {label:<18} | mono {:>4} reqs {:>8} B | rrss {:>4} reqs {:>8} B | +bloom {:>4} reqs {:>8} B",
            m.1, m.0, r.1, r.0, b.1, b.0
        );
    };

    // Per-query (top-K, no filter): runs the query through the monolith and both split sets
    // over an instrumented fetch, asserting all three agree.
    let run_mono = |q: &str| {
        let c = Arc::new(Counters::default());
        let idx = block_on(Index::open(CountFetch {
            bytes: Arc::clone(&mono_bytes),
            c: Arc::clone(&c),
        }))
        .unwrap();
        c.reset();
        let hits = block_on(idx.search(q, k)).unwrap();
        (c.read(), hits)
    };
    let run_rrss = |ss: &SplitSet, files: &HashMap<String, Arc<Vec<u8>>>, q: &str| {
        let c = Arc::new(Counters::default());
        let resolver = CountResolver {
            files: files.clone(),
            c: Arc::clone(&c),
        };
        let hits = block_on(ss.search(&resolver, q, k)).unwrap();
        (c.read(), hits)
    };

    // term000 common, term007 mid, term150 rare, zzz absent. Rare/absent terms are where the
    // Bloom shines: without it a tiered query that can't fill k descends through every split.
    for q in ["term000", "term007", "term150", "zzzabsent"] {
        let (mc, mh) = run_mono(q);
        let (rc, rh) = run_rrss(&rrss, &rrss_files, q);
        let (bc, bh) = run_rrss(&bloom, &bloom_files, q);
        row(&format!("{q} ({} hits)", mh.len()), mc, rc, bc);
        assert_eq!(mh, rh, "monolith vs rrss disagree on {q}");
        assert_eq!(mh, bh, "monolith vs rrss+bloom disagree on {q}");
    }
    println!("\n(per-query includes opening each tier read; the RRHC boot bundle would amortize tier-0.\n Bloom filters let a query skip splits whose vocabulary can't contain its n-grams.)");
}
