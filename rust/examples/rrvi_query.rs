//! Query an `RRVI` similarity index from the command line — a tiny harness for
//! validating a file produced by the FAISS exporter (`python/scripts/
//! faiss_to_rrvi.py`) against FAISS's own search.
//!
//! Run with the `vector` feature:
//! ```sh
//! cargo run --release --example rrvi_query --features vector -- \
//!     index.rrvi queries.bin <k> <nprobe>
//! ```
//! `queries.bin` is little-endian `[u32 n][u32 dim][f32 n*dim]`. For each query a
//! line of space-separated top-k doc IDs is printed to stdout.

use roaringrange::vector::VectorIndex;
use roaringrange::MemoryFetch;

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: rrvi_query <index.rrvi> <queries.bin> <k> <nprobe>");
        std::process::exit(2);
    }
    let k: usize = args[3].parse().expect("k");
    let nprobe: usize = args[4].parse().expect("nprobe");

    let rrvi = std::fs::read(&args[1]).expect("read rrvi");
    let qbytes = std::fs::read(&args[2]).expect("read queries");
    let n = read_u32(&qbytes, 0) as usize;
    let dim = read_u32(&qbytes, 4) as usize;
    let floats = &qbytes[8..];

    let idx =
        futures::executor::block_on(VectorIndex::open(MemoryFetch::new(rrvi))).expect("open RRVI");

    let mut out = String::new();
    for i in 0..n {
        let base = i * dim * 4;
        let q: Vec<f32> = (0..dim)
            .map(|d| f32::from_le_bytes(floats[base + d * 4..base + d * 4 + 4].try_into().unwrap()))
            .collect();
        let hits = futures::executor::block_on(idx.search(&q, k, nprobe)).expect("search");
        let line: Vec<String> = hits.iter().map(|h| h.doc_id.to_string()).collect();
        out.push_str(&line.join(" "));
        out.push('\n');
    }
    print!("{out}");
}
