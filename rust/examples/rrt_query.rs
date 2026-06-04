//! Query an `RRTI` term index from the command line — a tiny harness for
//! validating a `.rrt` produced by the builder (`roaringrange.write_term_index`).
//!
//! Run with the `terms` feature:
//! ```sh
//! cargo run --release --example rrt_query --features terms -- \
//!     index.rrt <search|prefix|fuzzy|complete> "<query>" [k]
//! ```
//! Prints space-separated top-`k` doc IDs (or terms, for `complete`) to stdout.

use roaringrange::{MemoryFetch, TermIndex};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: rrt_query <index.rrt> <search|prefix|fuzzy|complete> <query> [k]");
        std::process::exit(2);
    }
    let mode = args[2].as_str();
    let query = args[3].as_str();
    let k: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);

    let bytes = std::fs::read(&args[1]).expect("read rrt");
    let idx =
        futures::executor::block_on(TermIndex::open(MemoryFetch::new(bytes))).expect("open RRTI");
    eprintln!("terms in dictionary: {}", idx.len());

    let join = |ids: Vec<u32>| {
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let line = match mode {
        "search" => join(futures::executor::block_on(idx.search(query, k)).expect("search")),
        "prefix" => {
            join(futures::executor::block_on(idx.search_prefix(query, k)).expect("search_prefix"))
        }
        "fuzzy" => {
            join(futures::executor::block_on(idx.search_fuzzy(query, 1, k)).expect("search_fuzzy"))
        }
        "complete" => idx.complete(query, k).join(" "),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };
    println!("{line}");
}
