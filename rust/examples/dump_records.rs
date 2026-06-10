//! Dump the first `n` records of an `RRSR` record store as `<doc_id>\t<json>`
//! lines — the corpus text feeding the vector-search embedder. Records are in
//! descending rank, so `0..n` is the top-`n` most-popular docs.
//!
//! Needs the `zstd` feature to inflate a compressed (version-2) store:
//! ```sh
//! cargo run --release --example dump_records --features zstd -- \
//!     records.idx records.bin records.dict <n> > head.jsonl
//! ```

use roaringrange::records::RecordStore;
use roaringrange::FileFetch;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: dump_records <idx> <bin> <dict> <n>");
        std::process::exit(2);
    }
    let n: u32 = args[4].parse().expect("n");
    let dict = std::fs::read(&args[3]).expect("read dict");
    let store = futures::executor::block_on(RecordStore::open_with_dict(
        FileFetch::open(&args[1]).expect("open idx"),
        FileFetch::open(&args[2]).expect("open bin"),
        dict,
    ))
    .expect("open record store");

    let n = n.min(store.len());
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for id in 0..n {
        let bytes = futures::executor::block_on(store.get(id))
            .expect("get")
            .expect("record present");
        write!(w, "{id}\t").unwrap();
        w.write_all(&bytes).unwrap();
        w.write_all(b"\n").unwrap();
    }
    w.flush().unwrap();
    eprintln!("dumped {n} records");
}
