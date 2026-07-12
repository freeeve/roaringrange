//! Generates the **case-sensitive** RRS trigram-monolith conformance golden: a
//! fixed mixed-case corpus folded into one v4 `RRSI` index with case normalization OFF, printed
//! as `rrs_monolith_cs <hex>`. The corpus mixes "Roaring"/"roaring" so case-sensitive keying
//! produces different trigrams than the default folding path. Saved to
//! go/testdata/rrs_monolith_cs_build_golden.txt and asserted by both go/monolithbuild_test.go
//! and Rust build_tests::rrs_monolith_cs_golden_matches.
//!
//!   cargo run --release --example gen_rrs_monolith_cs_golden
//!
//! Same layout as gen_rrs_monolith_golden but with ngram_keys_with(.., false) +
//! write_index_with(.., false): keys are not lowercased and the header is v4.

use std::collections::HashMap;

use roaring::RoaringBitmap;
use roaringrange::build::{serialize_posting, write_index_with, DEFAULT_STRIDE};
use roaringrange::ngram_keys_with;

const GRAM: usize = 3;

/// The case-sensitive fixture corpus; doc id == position. "Roaring" (doc 0) and "roaring"
/// (doc 1) key on distinct trigrams; the empty doc at id 2 consumes an id without trigrams.
fn fixture_docs() -> Vec<&'static str> {
    vec!["Roaring Bitmaps", "roaring range", "", "Bitmap Range INDEX"]
}

pub fn build_golden() -> Vec<u8> {
    let mut open: HashMap<u64, RoaringBitmap> = HashMap::new();
    for (id, text) in fixture_docs().iter().enumerate() {
        for k in ngram_keys_with(text, GRAM, false) {
            open.entry(k).or_default().insert(id as u32);
        }
    }
    let entries: Vec<(u64, Vec<u8>)> = open
        .into_iter()
        .map(|(k, bm)| (k, serialize_posting(&bm)))
        .collect();
    let mut out = Vec::new();
    write_index_with(&mut out, GRAM as u16, DEFAULT_STRIDE, entries, false)
        .expect("write_index_with");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rrs_monolith_cs {hex}");
}
