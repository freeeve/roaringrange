//! Opens a local trigram `.rrs` index over a `pread`-backed file and runs strict-AND searches —
//! the local-file sibling of `candidates.rs` (which fetches over HTTP). Handy for smoke-verifying a
//! freshly built monolith without serving it.
//!
//!   cargo run --release --example query_rrs -- <index.rrs> [query ...]
//!
//! Results are rank-ordered doc IDs (0 = most-cited), the same numbering the records/facets use.

use futures::executor::block_on;
use roaringrange::{FileFetch, Index};
use std::time::Instant;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: query_rrs <index.rrs> [query ...]");
        std::process::exit(2);
    }
    let path = args.remove(0);
    let queries = if args.is_empty() {
        vec!["machine learning".to_string()]
    } else {
        args
    };

    let fetch = FileFetch::open(&path).expect("open .rrs");
    let idx = block_on(Index::open(fetch)).expect("open index");
    println!("opened {path} (gram_size={})", idx.gram_size());

    for q in &queries {
        let t = Instant::now();
        let hits = block_on(idx.search(q, 20)).expect("search");
        let top: Vec<u32> = hits.iter().take(8).copied().collect();
        println!(
            "  {:?}: {} hits in {:.1}ms  top doc ids: {:?}",
            q,
            hits.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            top
        );
    }
}
