//! Measure idea-2 (rarest-trigram candidate seeding) on a live index, for the
//! queries that fetched badly under full intersection.
//!
//! For each query it reports the full strict-AND (result count + bytes fetched),
//! then the candidate set from seeding only the k rarest trigrams (count + bytes)
//! for small k. Idea-2 egress ≈ seed bytes + candidate_count × record size (the
//! verify fetch). It wins when a small k already yields a small candidate set.
//!
//!   cargo run --release --example candidates -- [URL] "machine learning" "posthuman became"

use futures::executor::block_on;
use roaringrange::{ngram_keys, FetchError, Index, RangeFetch};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    reqs: AtomicU64,
}

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
            return Err(FetchError::Transport(format!("curl {:?}", out.status)));
        }
        self.c.reqs.fetch_add(1, Ordering::Relaxed);
        self.c
            .bytes
            .fetch_add(out.stdout.len() as u64, Ordering::Relaxed);
        Ok(out.stdout)
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let url = if args.first().is_some_and(|a| a.starts_with("http")) {
        args.remove(0)
    } else {
        "https://openalex.evefreeman.com/openalex-47m.rrs".to_string()
    };
    let queries = if args.is_empty() {
        vec![
            "machine learning".to_string(),
            "posthuman became".to_string(),
        ]
    } else {
        args
    };

    let c = Arc::new(Counters::default());
    let idx = block_on(Index::open(CurlFetch {
        url,
        c: Arc::clone(&c),
    }))
    .expect("open index");
    let n = idx.gram_size as usize;

    let mb = |b: u64| b as f64 / 1024.0 / 1024.0;
    let reset = || {
        c.bytes.store(0, Ordering::Relaxed);
        c.reqs.store(0, Ordering::Relaxed);
    };
    let read = || {
        (
            c.bytes.load(Ordering::Relaxed),
            c.reqs.load(Ordering::Relaxed),
        )
    };

    for q in &queries {
        let ngrams = ngram_keys(q, n).len();
        println!("\n=== {:?}  ({} trigrams) ===", q, ngrams);

        reset();
        let full = block_on(idx.search(q, 100_000_000)).expect("search");
        let (fb, fr) = read();
        println!(
            "  full AND : {:>8} results    {:>5} reqs   {:>7.1} MB",
            full.len(),
            fr,
            mb(fb)
        );

        for k in 1..=ngrams.min(4) {
            reset();
            let cands = block_on(idx.search_candidates(q, k)).expect("candidates");
            let (sb, sr) = read();
            let verify_mb = cands.len() as f64 * 1024.0 / 1024.0 / 1024.0; // @ ~1 KB/record
            println!(
                "  seed k={} : {:>8} candidates {:>5} reqs   {:>7.1} MB seed   +~{:>6.1} MB verify  => ~{:.1} MB total",
                k,
                cands.len(),
                sr,
                mb(sb),
                verify_mb,
                mb(sb) + verify_mb
            );
        }
    }
}
