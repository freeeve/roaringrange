//! Opens an `RRSS` split set from disk and runs a query — a smoke check that a built split set
//! (e.g. from `build_trigram_splitset` / `build_term_splitset`) is openable and searchable.
//!
//!   cargo run --release --features splits --example splitset_verify -- <manifest.rrss> <query> [k=10]
//!
//! Resolves each split's `data_file` against the manifest's directory via positional file reads,
//! the same shape the browser's `RrssIndex` uses over HTTP range reads. Prints the split count
//! and the top-k matching doc ids.

use futures::executor::block_on;
use roaringrange::{FileFetch, SplitFetcher, SplitSet};
use std::path::{Path, PathBuf};

/// Resolves a split's `data_file` to a file in `dir` (the manifest's directory).
struct DirResolver {
    dir: PathBuf,
}

impl SplitFetcher for DirResolver {
    type Fetch = FileFetch;
    fn fetch_named(&self, name: &str) -> FileFetch {
        FileFetch::open(self.dir.join(name)).expect("open split file")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: splitset_verify <manifest.rrss> <query> [k=10]");
        std::process::exit(2);
    }
    let rrss = PathBuf::from(&args[1]);
    let query = &args[2];
    let k: usize = args.get(3).map(|s| s.parse().expect("k")).unwrap_or(10);
    let dir = rrss.parent().unwrap_or(Path::new(".")).to_path_buf();

    let ss = block_on(SplitSet::open(
        FileFetch::open(&rrss).expect("open manifest"),
    ))
    .expect("parse manifest");
    println!("opened {} : {} splits", rrss.display(), ss.splits().len());

    let resolver = DirResolver { dir };
    let hits = block_on(ss.search(&resolver, query, k)).expect("search");
    let shown = hits.len().min(k);
    println!(
        "query {query:?} -> {} hits (top {}): {:?}",
        hits.len(),
        shown,
        &hits[..shown]
    );
}
