//! One-off measurement: how much would an inverted-posting (store the
//! complement + a flag) encoding save on a query's tail postings?
//!
//! For each query trigram it fetches the tail posting from the live index over
//! HTTP Range (via `curl`, so no added dependency), buckets docs into 65,536-doc
//! blocks, and compares each block's serialized size to its complement's. A
//! bitmap container is a flat 8 KB for any cardinality in (4096, 61440]; only
//! blocks denser than ~94% shrink when inverted (complement becomes an array or
//! empty). The output shows, per trigram and per query, how many bytes inversion
//! would actually recover.
//!
//!   cargo run --release --example density -- [URL] "machine learning" "posthuman became"

use futures::executor::block_on;
use roaringrange::{ngram_keys, FetchError, Index, RangeFetch};

struct CurlFetch {
    url: String,
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
        Ok(out.stdout)
    }
}

/// Serialized size of a roaring container holding `card` of 65,536 docs: an array
/// (2 bytes/doc) up to 4096, otherwise a flat 8 KB bitmap.
fn block_bytes(card: u32) -> u64 {
    if card <= 4096 {
        2 * card as u64
    } else {
        8192
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

    let idx = block_on(Index::open(CurlFetch { url })).expect("open index");
    let n = idx.gram_size() as usize;

    for q in &queries {
        let keys = ngram_keys(q, n);
        println!("\n=== {:?}  ({} trigrams) ===", q, keys.len());
        let (mut q_posting, mut q_min) = (0u64, 0u64);
        for &key in &keys {
            let tail = match block_on(idx.tail(key)) {
                Ok(Some(bm)) => bm,
                Ok(None) => {
                    println!("  {:016x}  absent", key);
                    continue;
                }
                Err(e) => {
                    println!("  {:016x}  error: {e}", key);
                    continue;
                }
            };
            let mut per_block = std::collections::BTreeMap::<u16, u32>::new();
            for d in tail.iter() {
                *per_block.entry((d >> 16) as u16).or_default() += 1;
            }
            let (mut posting, mut minb, mut inv_blocks) = (0u64, 0u64, 0usize);
            for &card in per_block.values() {
                let p = block_bytes(card);
                let c = block_bytes(65536 - card);
                posting += p;
                minb += p.min(c);
                if c < p {
                    inv_blocks += 1;
                }
            }
            q_posting += posting;
            q_min += minb;
            let save = if posting > 0 {
                100.0 * (posting - minb) as f64 / posting as f64
            } else {
                0.0
            };
            println!(
                "  {:016x}  card={:>9}  blocks={:>4}  >94%-blocks={:>4}  posting={:>7} KB  inverted={:>7} KB  save={:>5.1}%",
                key, tail.len(), per_block.len(), inv_blocks, posting / 1024, minb / 1024, save
            );
        }
        let save = if q_posting > 0 {
            100.0 * (q_posting - q_min) as f64 / q_posting as f64
        } else {
            0.0
        };
        println!(
            "  --- tail total: posting={} MB  with-inversion={} MB  save={:.1}% ---",
            q_posting / 1024 / 1024,
            q_min / 1024 / 1024,
            save
        );
    }
}
