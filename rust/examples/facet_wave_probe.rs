//! Byte-accounting probe for the split-set facet pricing path: builds a synthetic
//! high-cardinality sidecar shaped like a real enriched corpus (several fields, zipf-ish
//! category sizes, docs spanning many buckets), runs `facet_counts_top` against it with a
//! recording fetch, and prints what each read actually fetched — the local reproduction for
//! remote byte-volume reports.
//!
//!   cargo run --release --features "splits zstd" --example facet_wave_probe -- \
//!       [docs=400000] [cats_per_field=20000] [top_per_field=16] [pool=64] [digest=0]
//!
//! `digest`: `0` boots the whole sidecar meta; `k` builds a v1 facet digest (top-`k`/field);
//! `-k` builds the v2 digest (tail directory resident). Run `k` then `-k` to A/B v1 vs v2.

use futures::executor::block_on;
use roaringrange::{MemoryFetch, RangeFetch, SplitFetcher, SplitSet};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone)]
struct Recording {
    inner: MemoryFetch,
    name: String,
    log: Rc<RefCell<Vec<(String, u64, usize)>>>,
}

impl RangeFetch for Recording {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, roaringrange::FetchError> {
        self.log.borrow_mut().push((self.name.clone(), offset, len));
        self.inner.read(offset, len).await
    }
}

struct Resolver {
    files: HashMap<String, Vec<u8>>,
    log: Rc<RefCell<Vec<(String, u64, usize)>>>,
}

impl SplitFetcher for Resolver {
    type Fetch = Recording;
    fn fetch_named(&self, name: &str) -> Recording {
        Recording {
            inner: match self.files.get(name) {
                Some(b) => MemoryFetch::new(b.clone()),
                None => MemoryFetch::missing(),
            },
            name: name.to_string(),
            log: self.log.clone(),
        }
    }
}

/// splitmix64, for a deterministic synthetic corpus.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: usize| -> usize {
        args.get(i)
            .map(|s| s.parse().expect("numeric arg"))
            .unwrap_or(d)
    };
    let docs = arg(1, 400_000) as u32;
    let cats_per_field = arg(2, 20_000) as u64;
    let top = arg(3, 16);
    let pool = arg(4, 64) as u32;
    // digest: 0 -> full-meta boot (wave A opens each tail header); k -> build a v1 facet digest
    // (directory-less). Negative -k -> v2 digest (tail directory resident, so wave A skips
    // large-tail header reads) — the A/B baseline is v1 (+k) vs v2 (-k).
    let digest_arg = args.get(5).map(|s| s.as_str());
    let (digest, digest_v2) = match digest_arg {
        Some(s) if s.starts_with('-') => (s[1..].parse::<usize>().unwrap_or(0), true),
        Some(s) => (s.parse::<usize>().unwrap_or(0), false),
        None => (0, false),
    };

    // One split holding every doc (like one of deeplibby's ~466k-doc splits). Fields
    // mirror the deeplibby shape: two low-cardinality fields and four high-cardinality
    // ones with zipf-ish assignment (low category ids are dense).
    let fields: [(&str, u64); 6] = [
        ("language", 40),
        ("format", 12),
        ("subject", cats_per_field),
        ("bisac", cats_per_field / 4),
        ("people", cats_per_field),
        ("places", cats_per_field / 2),
    ];
    let mut b = roaringrange::SplitSetBuilder::new(roaringrange::SplitBuildConfig::new(
        roaringrange::Policy::Tiered,
        1 << 30,
        3,
        "probe",
    ));
    if digest > 0 {
        b = if digest_v2 {
            b.with_facet_digest_v2(digest)
        } else {
            b.with_facet_digest(digest)
        };
    }
    let mut state = 0x5eed_5eed_5eed_5eedu64;
    for doc in 0..docs {
        // Doc rank == insertion order (doc 0 is highest-rank -> bucket 0 == the head).
        let rank_frac = doc as f64 / docs as f64;
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(fields.len());
        for (fname, n) in fields {
            let u = (next(&mut state) >> 11) as f64 / (1u64 << 53) as f64;
            // Low-cardinality fields (language/format) stay uniform across ranks, so their
            // categories are present in the head. High-cardinality fields are rank-LOCALIZED:
            // a category's docs cluster in a rank band, so most categories live entirely in
            // the tail (head_size == 0) -- the shape of a real enriched corpus (people/places),
            // and exactly where the v2 tail directory removes wave-A header reads.
            let cat = if n <= 64 {
                ((u * u * u) * n as f64) as u64
            } else {
                let jitter = (u - 0.5) * 0.06; // small smear so bands overlap a little
                (((rank_frac + jitter).clamp(0.0, 0.999)) * n as f64) as u64
            };
            pairs.push((fname.to_string(), format!("c{cat:06}")));
        }
        b.add_faceted("abc", &pairs).unwrap();
    }
    let built = b.finish().unwrap();
    assert_eq!(built.splits.len(), 1);
    let rrf_len = built.facets[0].1.len();

    let files: HashMap<String, Vec<u8>> = built
        .splits
        .iter()
        .chain(built.facets.iter())
        .cloned()
        .collect();
    let log = Rc::new(RefCell::new(Vec::new()));
    let resolver = Resolver {
        files,
        log: log.clone(),
    };
    let ss = block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap();

    // A fused-pool-like result: `pool` random ids across the whole doc range (spans many
    // 64K buckets, like a rank-fused 500-id pool split 8 ways).
    let ids: Vec<u32> = (0..pool)
        .map(|_| (next(&mut state) % docs as u64) as u32)
        .collect();

    roaringrange::set_facet_trace(true);
    let counts = block_on(ss.facet_counts_top(&resolver, &ids, top)).unwrap();
    let waves = roaringrange::take_facet_trace();
    let priced: usize = counts.iter().map(|f| f.categories.len()).sum();

    let reads = log.borrow();
    let rrf: Vec<_> = reads
        .iter()
        .filter(|(n, _, _)| n.ends_with(".rrf"))
        .collect();
    let total: usize = rrf.iter().map(|(_, _, l)| l).sum();
    let meta: usize = rrf
        .iter()
        .filter(|(_, o, _)| *o == 0)
        .map(|(_, _, l)| l)
        .sum();
    let mut sizes: Vec<usize> = rrf.iter().map(|(_, _, l)| *l).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    println!(
        "sidecar {:.2} MB | docs {docs} | pool {pool} | top {top} | priced cats {priced}",
        rrf_len as f64 / (1024.0 * 1024.0),
    );
    println!(
        ".rrf reads: {} | total {:.2} MB | meta(off=0) {:.2} MB | pricing {:.2} MB",
        rrf.len(),
        total as f64 / (1024.0 * 1024.0),
        meta as f64 / (1024.0 * 1024.0),
        (total - meta) as f64 / (1024.0 * 1024.0),
    );
    println!("largest reads: {:?}", &sizes[..sizes.len().min(12)]);

    // Pricing-wave structure: one A/B/C triple per contributing split, all splits'
    // waves issued concurrently. `reads` is the coalesced requests the wave fires.
    let (mut wa, mut wb, mut wc) = (0usize, 0usize, 0usize);
    for w in &waves {
        match w.wave {
            "A" => wa += w.reads,
            "B" => wb += w.reads,
            "C" => wc += w.reads,
            _ => {}
        }
    }
    println!(
        "pricing waves: {} splits x (A,B,C) | reads A={wa} B={wb} C={wc} | dependent depth {}",
        waves.len() / 3,
        if waves.is_empty() { 0 } else { 3 },
    );
}
