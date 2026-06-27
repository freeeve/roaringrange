//! Head-to-head benchmark: a **trigram** (`RRS`) split set vs a **term/FST** (`RRTI`) split set
//! over the *same* corpus, query log, and byte cap — the "try both" comparison for
//! `tasks/007_split_set_index`. Builds the corpus both ways, then runs each query through an
//! instrumented in-memory `RangeFetch` that counts bytes and requests, reporting on-S3 size, the
//! per-query bytes/requests, and the top-K overlap (the two index models match *different* things
//! — trigram substrings vs whole stemmed tokens — so results can legitimately differ; we report
//! overlap rather than asserting equality).
//!
//!   cargo run --release --features "splits terms" --example term_vs_trigram_bench
//!
//! The same `SplitSet` reader serves both: it dispatches on the manifest's body-kind byte, so the
//! only thing that differs below is which builder produced the splits.

use futures::executor::block_on;
use roaringrange::{
    FetchError, Policy, RangeFetch, SplitBuildConfig, SplitFetcher, SplitSet, SplitSetBuilder,
    TermSplitBuildConfig, TermSplitSetBuilder,
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
/// biased toward low indices (so `term000` is common, `term199` rare). Fed later in index order
/// == rank order. No RNG crate — an xorshift keeps it reproducible.
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
                    let idx = ((r * r) * vocab.len() as f64) as usize % vocab.len();
                    vocab[idx].as_str()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

type Files = HashMap<String, Arc<Vec<u8>>>;

/// Builds a tiered **trigram** (`RRS`) split set over `docs` at `byte_cap`.
fn build_trigram(docs: &[String], byte_cap: u64) -> (Vec<u8>, Files) {
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "trigram".to_string(),
        sortcol: None,
        bloom_bits_per_key: 0,
        case_sensitive: false,
    });
    for d in docs {
        b.add_text(d).unwrap();
    }
    collect(b.finish().unwrap())
}

/// Builds a tiered **term/FST** (`RRTI`) split set over `docs` at `byte_cap`.
fn build_term(docs: &[String], byte_cap: u64) -> (Vec<u8>, Files) {
    let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap,
        head_boundary: 0,
        name_prefix: "term".to_string(),
        sortcol: None,
        language: None,
        stopwords: false,
        case_sensitive: false,
    });
    for d in docs {
        b.add_text(d).unwrap();
    }
    collect(b.finish().unwrap())
}

fn collect(built: roaringrange::BuiltSplitSet) -> (Vec<u8>, Files) {
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
    let cap = 64 * 1024u64;
    let docs = corpus(n);
    let kb = |b: u64| b as f64 / 1024.0;

    let (tri_manifest, tri_files) = build_trigram(&docs, cap);
    let (term_manifest, term_files) = build_term(&docs, cap);

    let total = |fs: &Files| -> u64 { fs.values().map(|b| b.len() as u64).sum() };
    let open_ss = |m: &[u8]| {
        block_on(SplitSet::open(CountFetch {
            bytes: Arc::new(m.to_vec()),
            c: Arc::new(Counters::default()),
        }))
        .unwrap()
    };
    let tri = open_ss(&tri_manifest);
    let term = open_ss(&term_manifest);

    println!(
        "corpus: {n} docs × 8 words, 200-word vocab, top-{k}, per-split cap {} KB\n",
        cap / 1024
    );
    println!(
        "trigram RRS  : {:>3} splits, {:>8.1} KB splits + {:>5.1} KB manifest",
        tri.splits().len(),
        kb(total(&tri_files)),
        kb(tri_manifest.len() as u64),
    );
    println!(
        "term    RRTI : {:>3} splits, {:>8.1} KB splits + {:>5.1} KB manifest\n",
        term.splits().len(),
        kb(total(&term_files)),
        kb(term_manifest.len() as u64),
    );

    let run = |ss: &SplitSet, files: &Files, q: &str| -> ((u64, u64), Vec<u32>) {
        let c = Arc::new(Counters::default());
        let resolver = CountResolver {
            files: files.clone(),
            c: Arc::clone(&c),
        };
        let hits = block_on(ss.search(&resolver, q, k)).unwrap();
        (c.read(), hits)
    };

    // Jaccard overlap of the two top-K lists — 1.0 == identical, 0.0 == disjoint.
    let overlap = |a: &[u32], b: &[u32]| -> f64 {
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }
        let sa: std::collections::HashSet<_> = a.iter().collect();
        let sb: std::collections::HashSet<_> = b.iter().collect();
        let inter = sa.intersection(&sb).count() as f64;
        let union = sa.union(&sb).count() as f64;
        inter / union
    };

    println!(
        "  {:<20} | {:^22} | {:^22} | overlap",
        "query", "trigram RRS", "term RRTI"
    );
    for q in ["term000", "term007", "term150", "zzzabsent"] {
        let ((tb, tr), th) = run(&tri, &tri_files, q);
        let ((eb, er), eh) = run(&term, &term_files, q);
        println!(
            "  {:<20} | {:>4} reqs {:>9} B | {:>4} reqs {:>9} B | {:.2}",
            format!("{q} ({} hits)", th.len()),
            tr,
            tb,
            er,
            eb,
            overlap(&th, &eh),
        );
    }
    println!(
        "\n(term postings store each doc once per whole token; trigram stores it once per distinct\n \
         3-gram, so the term index is smaller and a rare *token* prunes splits its common trigrams\n \
         can't. Overlap < 1.0 is expected where trigram substring matches differ from token matches.)"
    );
}
