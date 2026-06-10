//! Print specific records by doc ID from a local zstd record store — the id-targeted
//! sibling of `dump_records` (which dumps the first N sequentially). For grounding
//! what a query's ranked doc IDs actually are.
//!
//!   cargo run --release --features zstd --example get_records -- <idx> <bin> <dict> <id> [id ...]

use futures::executor::block_on;
use roaringrange::records::RecordStore;
use roaringrange::FileFetch;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: get_records <idx> <bin> <dict> <id> [id ...]");
        std::process::exit(2);
    }
    let dict = std::fs::read(&args[3]).expect("read dict");
    let store = block_on(RecordStore::open_with_dict(
        FileFetch::open(&args[1]).expect("open idx"),
        FileFetch::open(&args[2]).expect("open bin"),
        dict,
    ))
    .expect("open store");
    for id_s in &args[4..] {
        let id: u32 = id_s.parse().expect("doc id");
        match block_on(store.get(id)).expect("get") {
            Some(bytes) => println!("{id}\t{}", String::from_utf8_lossy(&bytes)),
            None => println!("{id}\t(out of range)"),
        }
    }
}
