//! Generates the cross-language fixtures for the Go records-zstd builder (task
//! 049). Unlike the byte-for-byte goldens, the zstd record store is verified by
//! round-trip across encoders (libzstd vs klauspost), not golden bytes — so this
//! writes the shared inputs both sides build/read against:
//!
//!   - `records_corpus.bin` — the fixture records, length-prefixed
//!     (`u32 count`, then `count × (u32 len + bytes)`); the source of truth both
//!     the Go and Rust tests decode and compare against.
//!   - `records.dict` — a zstd dictionary trained over representative samples,
//!     shipped as the `*.dict` sidecar and used by both encoders/decoders.
//!   - `records_rust_zstd.{bin,idx}` — the store as built by the native libzstd
//!     [`write_records_zstd`]; the Go reader inflates it through klauspost
//!     (proving Rust-encode → Go-decode).
//!
//! The companion Go test writes `records_go_zstd.{bin,idx}` (klauspost encode),
//! which the Rust `go_built_zstd_store_reads_back_through_ruzstd` test inflates
//! through ruzstd (proving Go-encode → Rust-decode). Run after changing the
//! fixture corpus, then re-run the Go test with `RR_UPDATE_FIXTURES=1`:
//!
//!   cargo run --release --example gen_records_zstd_fixture --features zstd

use std::fs;
use std::path::Path;

use roaringrange::build::{train_record_dict, write_records_zstd};

/// The fixture records: self-similar JSON (shared keys compress well against the
/// dictionary) plus the two edge cases — a zero-length record (no tag) and a tiny
/// `{}` that cannot shrink (kept raw, tag 0).
pub fn fixture_records() -> Vec<Vec<u8>> {
    let big: Vec<&str> = vec![
        r#"{"id":"W1","title":"roaring bitmaps for fast set operations","venue":"VLDB","year":2020,"abstract":"compressed bitmaps that combine sorted arrays and bitsets so set operations stay fast on modern hardware."}"#,
        r#"{"id":"W2","title":"better bitmap performance with roaring bitmaps","venue":"SPE","year":2016,"abstract":"a bitmap compression scheme that is consistently faster and smaller than the prior alternatives in practice."}"#,
        r#"{"id":"W3","title":"range queries over roaring bitmaps in column stores","venue":"DEBU","year":2021,"abstract":"a study of range fetchable bitmap indexes for analytic queries over compressed columnar data held at rest."}"#,
        r#"{"id":"W6","title":"vectorized processing of compressed bitmaps","venue":"DAMON","year":2022,"abstract":"using simd to evaluate predicates directly on roaring containers without materializing intermediate result sets."}"#,
        r#"{"id":"W7","title":"consistently smaller compressed bitmaps with roaring","venue":"SPE","year":2018,"abstract":"extends roaring bitmaps with run containers to shrink dense and clustered postings even further in practice."}"#,
        r#"{"id":"W8","title":"a survey of bitmap index compression techniques","venue":"CSUR","year":2019,"abstract":"surveys word aligned hybrid, roaring, and related bitmap compression schemes used by modern search and analytic engines."}"#,
    ];
    vec![
        big[0].as_bytes().to_vec(),
        big[1].as_bytes().to_vec(),
        big[2].as_bytes().to_vec(),
        Vec::new(),     // zero-length record: stays addressable, no tag
        b"{}".to_vec(), // tiny record: compression cannot shrink it, kept raw (tag 0)
        big[3].as_bytes().to_vec(),
        big[4].as_bytes().to_vec(),
        big[5].as_bytes().to_vec(),
    ]
}

/// A larger templated sample set so the dictionary trainer has enough signal;
/// the records share the corpus's JSON key structure, so the trained dictionary
/// helps the fixture records compress.
fn training_samples() -> Vec<Vec<u8>> {
    (0..256u32)
        .map(|i| {
            format!(
                r#"{{"id":"W{i}","title":"a study of bitmap index number {i}","venue":"Journal of Bitmaps","year":20{:02},"abstract":"compressed bitmap indexes for fast set operations on modern hardware and analytic search workloads."}}"#,
                i % 25
            )
            .into_bytes()
        })
        .collect()
}

/// Encodes the records length-prefixed: `u32 count`, then `count × (u32 len + bytes)`.
fn encode_corpus(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for rec in records {
        out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        out.extend_from_slice(rec);
    }
    out
}

fn main() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../go/testdata");
    let records = fixture_records();

    let samples = training_samples();
    let sample_refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();
    let dict = train_record_dict(&sample_refs, 4096).expect("train dictionary");
    assert!(!dict.is_empty(), "trained dictionary should be non-empty");

    let mut bin = Vec::new();
    let mut idx = Vec::new();
    write_records_zstd(&mut bin, &mut idx, &records, &dict, 19).expect("write_records_zstd");

    fs::write(out_dir.join("records_corpus.bin"), encode_corpus(&records)).unwrap();
    fs::write(out_dir.join("records.dict"), &dict).unwrap();
    fs::write(out_dir.join("records_rust_zstd.bin"), &bin).unwrap();
    fs::write(out_dir.join("records_rust_zstd.idx"), &idx).unwrap();

    println!(
        "wrote {} fixture records, {}-byte dict, {}-byte rust store to {}",
        records.len(),
        dict.len(),
        bin.len(),
        out_dir.display()
    );
}
