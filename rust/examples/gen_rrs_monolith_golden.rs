//! Generates the RRS trigram-monolith conformance golden for the Go monolith builder:
//! a fixed small corpus folded into one v3 `RRSI` index, printed as
//! `rrs_monolith <hex>`. The corpus shares trigrams across docs (multi-doc postings) and
//! includes an empty doc, so doc id 3 lands in postings only if the empty doc (id 2) still
//! advanced the doc-ID space. Saved to go/testdata/rrs_monolith_build_golden.txt and asserted
//! by both go/monolithbuild_test.go and Rust build_tests::rrs_monolith_golden_matches.
//!
//!   cargo run --release --example gen_rrs_monolith_golden
//!
//! This is the in-memory equivalent of build_trigram_monolith's chunked path: tokenize each
//! doc with ngram_keys, accumulate one RoaringBitmap per trigram, serialize via
//! serialize_posting, and lay them out key-sorted with write_index.

use std::collections::HashMap;

use roaring::RoaringBitmap;
use roaringrange::build::{serialize_posting, write_index, DEFAULT_STRIDE};
use roaringrange::ngram_keys;

const GRAM: usize = 3;

/// The fixture corpus; doc id == position. The empty doc at id 2 consumes an id without
/// adding trigrams, so the next doc's postings carry id 3.
fn fixture_docs() -> Vec<&'static str> {
    vec!["roaring bitmaps", "roaring range", "", "bitmap range index"]
}

pub fn build_golden() -> Vec<u8> {
    let mut open: HashMap<u64, RoaringBitmap> = HashMap::new();
    for (id, text) in fixture_docs().iter().enumerate() {
        for k in ngram_keys(text, GRAM) {
            open.entry(k).or_default().insert(id as u32);
        }
    }
    let entries: Vec<(u64, Vec<u8>)> = open
        .into_iter()
        .map(|(k, bm)| (k, serialize_posting(&bm)))
        .collect();
    let mut out = Vec::new();
    write_index(&mut out, GRAM as u16, DEFAULT_STRIDE, entries).expect("write_index");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rrs_monolith {hex}");
}
