//! Generates the RRSB (`.rrb`) conformance golden for the Go BM25 builder.
//! Builds the sidecar over a fixed corpus and a synthetic dictionary (the corpus's
//! distinct plain-tokenized terms in lexicographic order, ascending head_offs), then
//! prints `rrsb <hex>` — saved to `go/testdata/rrsb_build_golden.txt` and asserted by
//! both `go/bm25_test.go` and the Rust `rrsb_golden_matches` test.
//!
//!   cargo run --release --features terms --example gen_rrsb_golden

use roaringrange::bm25::{DEFAULT_B, DEFAULT_K1};
use roaringrange::{write_impacts, ImpactsAccumulator, Tokenizer};
use std::collections::BTreeSet;

/// The shared fixed corpus (plain tokenizer: lowercase alphanumeric runs, no
/// stemming/stopwords) — kept byte-identical to `go/bm25_test.go`.
pub const CORPUS: [&str; 4] = [
    "the quick brown fox jumps over the lazy dog",
    "quick brown bitmaps roaring over data",
    "roaring fox bitmaps fast and quick",
    "the lazy dog and the quick fox",
];

/// Builds the `.rrb` bytes for [`CORPUS`] with a synthetic dict — the shared
/// construction the Rust test and the Go test both reproduce.
pub fn build_golden() -> Vec<u8> {
    let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
    for d in &CORPUS {
        acc.add_doc(d);
    }
    let tok = Tokenizer::plain();
    let mut terms: BTreeSet<String> = BTreeSet::new();
    for d in &CORPUS {
        for t in tok.tokenize(d) {
            terms.insert(t);
        }
    }
    let dict: Vec<(String, u64)> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| (t.clone(), (i as u64) * 16 + 100))
        .collect();
    let mut out = Vec::new();
    write_impacts(&mut out, &dict, &acc, DEFAULT_K1, DEFAULT_B).expect("write_impacts");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rrsb {hex}");
}
