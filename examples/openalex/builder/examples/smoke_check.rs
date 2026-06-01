//! Smoke check for a builder run: opens the compressed record store with its
//! trained dictionary and reads doc 0, then opens the DOI lookup and resolves one
//! identifier to its doc ID. Run as:
//!
//!   cargo run --release --example smoke_check -- <idx> <bin> <dict> <rril> <doi>
//!
//! Exits non-zero (with a message) on any failure; prints the resolved record and
//! doc ID on success.

use futures::executor::block_on;
use roaringrange::{Lookup, MemoryFetch, RecordStore};
use std::fs;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("usage: smoke_check <idx> <bin> <dict> <rril> <doi>");
        std::process::exit(2);
    }
    let (idx_p, bin_p, dict_p, rril_p, doi) = (&a[1], &a[2], &a[3], &a[4], &a[5]);

    let idx = MemoryFetch::new(fs::read(idx_p).expect("read idx"));
    let bin = MemoryFetch::new(fs::read(bin_p).expect("read bin"));
    let dict = fs::read(dict_p).expect("read dict");

    let store = block_on(RecordStore::open_with_dict(idx, bin, dict)).expect("open store");
    println!("record store: {} records", store.len());
    let rec = block_on(store.get(0))
        .expect("read doc 0")
        .expect("doc 0 present");
    let text = String::from_utf8_lossy(&rec);
    println!("doc 0 record ({} bytes): {}", rec.len(), text);
    assert!(text.contains("\"id\":"), "doc 0 missing id key");

    let lk_bytes = fs::read(rril_p).expect("read rril");
    let lk = block_on(Lookup::open(MemoryFetch::new(lk_bytes))).expect("open lookup");
    println!("lookup: {} entries", lk.len());
    let docs = block_on(lk.lookup(doi)).expect("lookup");
    assert!(!docs.is_empty(), "DOI {doi} did not resolve");
    println!("DOI {doi} -> docs {docs:?}");

    println!("SMOKE OK");
}
