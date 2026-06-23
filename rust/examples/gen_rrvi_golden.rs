//! Generates the RRVI (`.rrvi`) + RRVR (`.rrvr`) conformance goldens for the Go vector
//! serializers (task 050): a fixed IVFPQ model assembled with `build_ivfpq_from_parts`
//! (no training → fully deterministic, no FMA-sensitive float math) serialized to RRVI,
//! and a fixed bf16 re-rank blob. Printed as `rrvi <hex>` / `rrvr <hex>`, saved to
//! `go/testdata/{rrvi,rrvr}_build_golden.txt` and asserted by `go/vector_test.go` and the
//! Rust `build_tests` drift-guards.
//!
//! All fixture floats are exact in f32 and written as literals, so Go and Rust serialize
//! byte-for-byte without depending on cross-language float arithmetic. The re-rank uses
//! `l2_normalize = false` for the same reason (normalization's sum-of-squares is
//! FMA-fragile); the bf16 rounding itself is deterministic and is what this checks.
//!
//!   cargo run --release --features vector --example gen_rrvi_golden

use roaringrange::{build_ivfpq_from_parts, write_rerank, IvfpqParts, Metric};

fn fixture_parts() -> IvfpqParts {
    IvfpqParts {
        dim: 4,
        nlist: 2,
        m: 2,
        nbits: 2,
        metric: Metric::L2,
        // nlist*dim = 8
        centroids: vec![0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5],
        // m*ksub*dsub = 2*4*2 = 16
        codebooks: vec![
            0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 1.0, -1.0, 1.25, -1.25, 1.5, -1.5, 1.75, -1.75,
            2.0, -2.0,
        ],
        // dim*dim = 16 (identity), to exercise the OPQ flag + blob
        opq: Some(vec![
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]),
        ids: vec![10, 20, 30, 40, 50],
        assignments: vec![0, 1, 0, 1, 0],
        codes: vec![1, 2, 0, 3, 2, 1, 3, 0, 1, 1], // 5*m, each < ksub=4
    }
}

pub fn rrvi_golden() -> Vec<u8> {
    build_ivfpq_from_parts(fixture_parts())
        .expect("from_parts")
        .to_bytes()
}

pub fn rrvr_golden() -> Vec<u8> {
    let dim = 4;
    let vectors = vec![
        vec![1.1, -2.2, 0.0, 3.5],
        vec![100.25, -0.125, 42.0, 7.7],
        vec![0.0, 0.0, 0.0, 0.0],
    ];
    let mut out = Vec::new();
    write_rerank(&mut out, dim, &vectors, false).expect("write_rerank");
    out
}

fn main() {
    for (name, bytes) in [("rrvi", rrvi_golden()), ("rrvr", rrvr_golden())] {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        println!("{name} {hex}");
    }
}
