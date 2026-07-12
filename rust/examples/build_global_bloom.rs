//! Build a **global term-Bloom sidecar** over a split set's whole vocabulary — the
//! absent-term prune for a summary-stripped manifest. Reads every local split's
//! dictionary keys (header + dict region only, never the postings), unions them, and writes
//! the standard Bloom layout (`[k u32][nbits u32][bits]`).
//!
//! The output is consumed REMOTELY by `RemoteBloom`: a reader probes `k` byte positions per
//! key instead of downloading the filter, so the file's size costs storage only. The split
//! query path consults it lazily — after the top tier yields nothing — so a term absent from
//! the whole corpus ends the tier descent in ~`k` one-byte reads instead of opening every
//! split.
//!
//!   cargo run --release --features splits --example build_global_bloom -- <splits_dir> <out.bloom> [bits_per_key=10]

use roaringrange::bloom_build;
use std::collections::HashSet;
use std::os::unix::fs::FileExt;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: build_global_bloom <splits_dir> <out.bloom> [bits_per_key=10]");
        std::process::exit(2);
    }
    let bits_per_key: u32 = args
        .get(3)
        .map(|s| s.parse().expect("bits_per_key (u32)"))
        .unwrap_or(10);

    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&args[1])
        .expect("read splits dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rrs"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no .rrs splits in {}", args[1]);

    let t0 = Instant::now();
    let mut keys: HashSet<u64> = HashSet::new();
    for (i, path) in names.iter().enumerate() {
        let f = std::fs::File::open(path).expect("open split");
        let mut header = [0u8; 16];
        f.read_exact_at(&mut header, 0).expect("read header");
        assert_eq!(&header[0..4], b"RRSI", "{}: not an RRS", path.display());
        assert_eq!(
            u16::from_le_bytes([header[4], header[5]]),
            3,
            "{}: reader is v3-only",
            path.display()
        );
        let ngrams = u32::from_le_bytes(header[8..12].try_into().unwrap()) as u64;
        let stride = u32::from_le_bytes(header[12..16].try_into().unwrap()) as u64;
        assert!(stride > 0 || ngrams == 0, "{}: zero stride", path.display());
        let sparse_count = if ngrams == 0 {
            0
        } else {
            ngrams.div_ceil(stride)
        };
        let dict_start = 16 + sparse_count * 8;
        // The dictionary region only: ngrams × 20-byte entries, key first.
        let mut dict = vec![0u8; (ngrams * 20) as usize];
        f.read_exact_at(&mut dict, dict_start).expect("read dict");
        for e in 0..ngrams as usize {
            keys.insert(u64::from_le_bytes(
                dict[e * 20..e * 20 + 8].try_into().unwrap(),
            ));
        }
        if (i + 1) % 25 == 0 || i + 1 == names.len() {
            eprintln!(
                "[{:6.0}s] {}/{} splits, {} distinct keys",
                t0.elapsed().as_secs_f64(),
                i + 1,
                names.len(),
                keys.len()
            );
        }
    }

    let mut sorted: Vec<u64> = keys.into_iter().collect();
    sorted.sort_unstable();
    let bloom = bloom_build(&sorted, bits_per_key);
    std::fs::write(&args[2], &bloom).expect("write bloom");
    eprintln!(
        "[{:6.0}s] wrote {} ({:.1} MB, {} keys @ {} bits/key) — upload next to the splits and \
         wire it via the resolver's global-Bloom name",
        t0.elapsed().as_secs_f64(),
        args[2],
        bloom.len() as f64 / (1u64 << 20) as f64,
        sorted.len(),
        bits_per_key
    );
}
