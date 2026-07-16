//! Byte-accounting probe for the split-set facet pricing path: builds a synthetic
//! high-cardinality sidecar shaped like a real enriched corpus (several fields, zipf-ish
//! category sizes, docs spanning many buckets), runs `facet_counts_top` against it with a
//! recording fetch, and prints what each read actually fetched — the local reproduction for
//! remote byte-volume reports.
//!
//!   cargo run --release --features "splits zstd" --example facet_wave_probe -- \
//!       [docs=400000] [cats_per_field=20000] [top_per_field=16] [pool=64]

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
    let mut state = 0x5eed_5eed_5eed_5eedu64;
    for _ in 0..docs {
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(fields.len());
        for (fname, n) in fields {
            let u = (next(&mut state) >> 11) as f64 / (1u64 << 53) as f64;
            let cat = ((u * u * u) * n as f64) as u64; // strong skew toward low ids
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
