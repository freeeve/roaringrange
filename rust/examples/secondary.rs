//! Build a tiny two-order corpus in memory and demonstrate a secondary index:
//! search in a *secondary* rank order (newest-first), map the result page back to
//! primary doc IDs through the permutation, and fetch the primary-keyed records.
//!
//!   cargo run --example secondary
//!
//! No network or files — everything is `MemoryFetch`, so this doubles as an
//! end-to-end build→read smoke test for the format pieces in `SORTCOLS.md`.

use futures::executor::block_on;
use roaring::RoaringBitmap;
use roaringrange::build::{serialize_posting, write_index, write_perm, write_records};
use roaringrange::{ngram_keys, MemoryFetch, RecordStore, SecondaryIndex};
use std::collections::BTreeMap;

/// One source document: its text, publication year, and the primary (citation)
/// rank position it was assigned at primary-build time.
struct Doc {
    primary_id: u32,
    year: u16,
    text: &'static str,
}

/// Builds a secondary `RRS` index (trigram postings keyed by *secondary* doc ID)
/// from `(secondary_id, text)` pairs.
fn build_secondary_index(docs: &[(u32, &str)]) -> MemoryFetch {
    let mut postings: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
    for &(sec_id, text) in docs {
        for key in ngram_keys(text, 3) {
            postings.entry(key).or_default().insert(sec_id);
        }
    }
    let entries: Vec<(u64, Vec<u8>)> = postings
        .iter()
        .map(|(k, bm)| (*k, serialize_posting(bm)))
        .collect();
    let mut out = Vec::new();
    write_index(&mut out, 3, 0, entries).unwrap();
    MemoryFetch::new(out)
}

fn main() {
    // Corpus already assigned primary (citation-rank) doc IDs 0..4.
    let corpus = [
        Doc {
            primary_id: 0,
            year: 2010,
            text: "alpha beta",
        },
        Doc {
            primary_id: 1,
            year: 2024,
            text: "alpha gamma",
        },
        Doc {
            primary_id: 2,
            year: 2020,
            text: "alpha delta",
        },
        Doc {
            primary_id: 3,
            year: 2024,
            text: "beta gamma",
        },
    ];

    // Secondary order = year descending, ties broken by primary doc ID ascending
    // (so "newest, then most-cited"). The k-th entry is the primary doc placed at
    // secondary doc ID k.
    let mut order: Vec<&Doc> = corpus.iter().collect();
    order.sort_by(|a, b| b.year.cmp(&a.year).then(a.primary_id.cmp(&b.primary_id)));
    let perm_primary_of_secondary: Vec<u32> = order.iter().map(|d| d.primary_id).collect();

    // Secondary text index: doc texts in secondary-ID order.
    let sec_docs: Vec<(u32, &str)> = order
        .iter()
        .enumerate()
        .map(|(sec_id, d)| (sec_id as u32, d.text))
        .collect();
    let index = build_secondary_index(&sec_docs);

    // Permutation store (secondary_id -> primary_id).
    let mut perm_bytes = Vec::new();
    write_perm(&mut perm_bytes, perm_primary_of_secondary.clone()).unwrap();
    let perm = MemoryFetch::new(perm_bytes);

    // Records stay keyed by PRIMARY doc ID — shared with the primary index.
    let mut records: Vec<Vec<u8>> = vec![Vec::new(); corpus.len()];
    for d in &corpus {
        records[d.primary_id as usize] =
            format!("{{\"text\":\"{}\",\"year\":{}}}", d.text, d.year).into_bytes();
    }
    let (mut bin, mut idx) = (Vec::new(), Vec::new());
    write_records(&mut bin, &mut idx, &records).unwrap();
    let store = block_on(RecordStore::open(
        MemoryFetch::new(idx),
        MemoryFetch::new(bin),
    ))
    .unwrap();

    // Search "alpha" in the secondary (newest-first) order.
    let secondary = block_on(SecondaryIndex::open(index, perm)).unwrap();
    let mut cursor = block_on(secondary.search_cursor("alpha", 0)).unwrap();
    let primary_ids = block_on(cursor.page(0, 10)).unwrap();

    println!("query \"alpha\" — newest first (primary doc IDs): {primary_ids:?}");
    let recs = block_on(store.get_many(&primary_ids)).unwrap();
    for (pid, rec) in primary_ids.iter().zip(&recs) {
        let body = rec
            .as_deref()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        println!("  primary {pid}: {body}");
    }

    // primary 1 (2024) before primary 2 (2020) before primary 0 (2010); doc 3 has
    // no "alpha". This is the newest-first ordering, mapped to primary IDs.
    assert_eq!(primary_ids, vec![1, 2, 0], "newest-first primary mapping");
    println!("OK");
}
