//! Verifier / demo CLI: open a built index + record store and run a query,
//! printing the ranked doc IDs and their records. Proves the full pipeline —
//! build (this crate) → read (the reader crate) — round-trips.
//!
//! Usage:
//!   cargo run --release --example query -- <rrs> <records.idx> <records.bin> <query> [limit]

use futures::executor::block_on;
use roaringrange::{Index, MemoryFetch, RecordStore};
use std::fs;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: query <rrs> <records.idx> <records.bin> <query> [limit]");
        std::process::exit(2);
    }
    let rrs = fs::read(&a[1]).expect("read rrs");
    let ridx = fs::read(&a[2]).expect("read records.idx");
    let rbin = fs::read(&a[3]).expect("read records.bin");
    let query = &a[4];
    let limit: usize = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(10);

    let idx = block_on(Index::open(MemoryFetch::new(rrs))).expect("open index");
    let store = block_on(RecordStore::open(
        MemoryFetch::new(ridx),
        MemoryFetch::new(rbin),
    ))
    .expect("open records");
    println!(
        "index: {} ngrams (gram_size {}); records: {}",
        idx.ngram_count(),
        idx.gram_size,
        store.len()
    );

    let ids = block_on(idx.search(query, limit)).expect("search");
    println!("query {query:?} -> {} hit(s):", ids.len());
    for id in ids {
        let rec = block_on(store.get(id))
            .expect("get record")
            .unwrap_or_default();
        println!("  doc {id}: {}", String::from_utf8_lossy(&rec));
    }
}
