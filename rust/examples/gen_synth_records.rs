//! Generates a synthetic `RRSR` record store for build benchmarks — N JSON records shaped
//! like the OpenAlex corpus (`{"t": title, "ab": abstract}`) with a zipf-skewed vocabulary,
//! deterministic across runs. Useful when the real corpus isn't on the box but a build's
//! throughput needs measuring against a realistically sized store:
//!
//!   cargo run --release --features zstd --example gen_synth_records -- <N> <out_dir>
//!
//! Writes `records-synth.{bin,idx,dict}` into `out_dir`.

use roaringrange::build::{train_record_dict, write_records_zstd};
use std::path::Path;

/// splitmix64 — a tiny deterministic PRNG so the corpus is reproducible without a rand dep.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// A zipf-ish word id in `[0, vocab)`: squaring a unit sample skews mass toward low ids, so a
/// few words are very common and the tail is rare — shaping term postings like real text.
fn word_id(state: &mut u64, vocab: u64) -> u64 {
    let u = (next(state) >> 11) as f64 / (1u64 << 53) as f64;
    ((u * u) * vocab as f64) as u64
}

/// One record's JSON: an 8-word title and a 40-word abstract over a 50k-word vocabulary.
fn record(state: &mut u64) -> Vec<u8> {
    const VOCAB: u64 = 50_000;
    let mut words = |n: usize| {
        let mut s = String::new();
        for i in 0..n {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&format!("w{:05}", word_id(state, VOCAB)));
        }
        s
    };
    let t = words(8);
    let ab = words(40);
    format!(r#"{{"t":"{t}","ab":"{ab}"}}"#).into_bytes()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: gen_synth_records <N> <out_dir>");
        std::process::exit(2);
    }
    let n: usize = args[1].parse().expect("N");
    let out_dir = Path::new(&args[2]);
    std::fs::create_dir_all(out_dir).expect("create out dir");

    let mut state = 0x5eed_5eed_5eed_5eedu64;
    let records: Vec<Vec<u8>> = (0..n).map(|_| record(&mut state)).collect();

    let sample_refs: Vec<&[u8]> = records.iter().take(1024).map(|r| r.as_slice()).collect();
    let dict = train_record_dict(&sample_refs, 16 * 1024).expect("train dictionary");

    let mut bin = Vec::new();
    let mut idx = Vec::new();
    write_records_zstd(&mut bin, &mut idx, &records, &dict, 3).expect("write records");

    std::fs::write(out_dir.join("records-synth.bin"), &bin).expect("write bin");
    std::fs::write(out_dir.join("records-synth.idx"), &idx).expect("write idx");
    std::fs::write(out_dir.join("records-synth.dict"), &dict).expect("write dict");
    println!(
        "wrote {n} records ({:.1} MB compressed) + dict to {}",
        bin.len() as f64 / (1024.0 * 1024.0),
        out_dir.display()
    );
}
