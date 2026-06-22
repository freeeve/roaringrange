//! Inspects records and checks trigram containment, to confirm what a doc id
//! actually resolves to and whether it is a legitimate trigram match for a query.
//!
//!   cargo run --release --features zstd --example dump_record -- \
//!       records.idx records.bin records.dict "<query>" <id> [id ...]
//!
//! For each id it prints the title and whether the doc's FULL indexed text contains
//! every trigram of `<query>` (via the same `record_text` + `ngram_keys` the builder
//! uses) — i.e. whether the trigram index legitimately matches it.

use futures::executor::block_on;
use roaringrange::ngram_keys;
use roaringrange::records::RecordStore;
use roaringrange::FileFetch;
use std::collections::HashSet;

const GRAM: usize = 3;

/// Byte-identical to `build_trigram_monolith::record_text`: `"<t> <ab> <a> <v>"`.
fn record_text(bytes: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let (title, abstract_, authors, venue) = (g("t"), g("ab"), g("a"), g("v"));
    let mut text =
        String::with_capacity(title.len() + abstract_.len() + authors.len() + venue.len() + 3);
    text.push_str(title);
    for f in [abstract_, authors, venue] {
        if !f.is_empty() {
            text.push(' ');
            text.push_str(f);
        }
    }
    text
}

fn title_of(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("t").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!(
            "usage: dump_record <records.idx> <records.bin> <records.dict> <query> <id> [id ...]"
        );
        std::process::exit(2);
    }
    let dict = std::fs::read(&args[2]).expect("read dict");
    let store = block_on(RecordStore::open_with_dict(
        FileFetch::open(&args[0]).expect("open idx"),
        FileFetch::open(&args[1]).expect("open bin"),
        dict,
    ))
    .expect("open store");
    let query = &args[3];
    let qtri: HashSet<u64> = ngram_keys(query, GRAM).into_iter().collect();
    println!(
        "store {} records | query {:?} -> {} distinct trigrams",
        store.len(),
        query,
        qtri.len()
    );
    for a in &args[4..] {
        let id: u32 = a.parse().expect("id");
        match block_on(store.get(id)).expect("get") {
            Some(bytes) => {
                let text = record_text(&bytes);
                let dtri: HashSet<u64> = ngram_keys(&text, GRAM).into_iter().collect();
                let missing: Vec<u64> = qtri.difference(&dtri).copied().collect();
                let t = title_of(&bytes);
                println!(
                    "[{id}] match={} (doc has {} trigrams, {} of query missing) | {}",
                    missing.is_empty(),
                    dtri.len(),
                    missing.len(),
                    &t[..t.len().min(80)]
                );
            }
            None => println!("[{id}] (out of range)"),
        }
    }
}
