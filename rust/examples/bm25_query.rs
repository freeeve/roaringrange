//! Queries a local `.rrt` + `.rrb` pair with BM25 candidate-window rerank, next
//! to the plain static-rank search — the local harness for an `RRSB` build:
//!
//!   cargo run --release --features terms --example bm25_query -- \
//!     openalex-1m-stem.rrt openalex-1m-stem.rrb "machine learning" [M] [K]

use futures::executor::block_on;
use roaringrange::bm25::{search_bm25, ImpactIndex};
use roaringrange::{FileFetch, TermIndex};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: bm25_query RRT RRB QUERY [M=1000] [K=10]");
        std::process::exit(2);
    }
    let m: usize = a.get(3).map(|s| s.parse().expect("M")).unwrap_or(1000);
    let k: usize = a.get(4).map(|s| s.parse().expect("K")).unwrap_or(10);
    let terms = block_on(TermIndex::open(FileFetch::open(&a[0]).expect("rrt"))).expect("open rrt");
    let impacts =
        block_on(ImpactIndex::open(FileFetch::open(&a[1]).expect("rrb"))).expect("open rrb");
    let plain = block_on(terms.search(&a[2], k)).expect("search");
    println!("static rank: {plain:?}");
    let scored = block_on(search_bm25(&terms, &impacts, &a[2], m, k)).expect("bm25");
    println!("bm25 rerank (m={m}):");
    for s in scored {
        println!("  doc {:>9}  score {:.3}", s.doc_id, s.score);
    }
}
