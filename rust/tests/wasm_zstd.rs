//! Runtime proof that the zstd record-decode path works on `wasm32` — the actual
//! browser target — not just natively. The fixtures are generated natively (the C
//! `zstd` encoder/dictionary trainer is native-only) by
//! `cargo run --example gen_zstd_fixture --features zstd`; the browser only ever
//! decodes, which is exactly what this exercises.
//!
//! Run on wasm:   wasm-pack test --node --features zstd
//! (node runner; no browser install needed). Gated to wasm32 so native
//! `cargo test` skips it (the native round-trip is covered in build_tests.rs).
#![cfg(all(target_arch = "wasm32", feature = "zstd"))]

use roaringrange::{MemoryFetch, RecordStore};
use wasm_bindgen_test::wasm_bindgen_test;

const DICT: &[u8] = include_bytes!("fixtures/records.dict");
const IDX: &[u8] = include_bytes!("fixtures/records.idx");
const BIN: &[u8] = include_bytes!("fixtures/records.bin");
const EXPECT: &[u8] = include_bytes!("fixtures/records.expect");

/// Opens the dictionary-compressed fixture store and asserts doc 0 inflates to
/// its original bytes — the dictionary decode running on the wasm32 ruzstd build.
#[wasm_bindgen_test]
async fn zstd_record_inflates_on_wasm() {
    let store = RecordStore::open_with_dict(
        MemoryFetch::new(IDX.to_vec()),
        MemoryFetch::new(BIN.to_vec()),
        DICT.to_vec(),
    )
    .await
    .expect("open compressed store with dict");

    assert_eq!(store.len(), 200, "fixture has 200 records");

    let rec = store
        .get(0)
        .await
        .expect("get(0) ok")
        .expect("doc 0 present");
    assert_eq!(rec, EXPECT, "doc 0 must inflate to its original bytes");
}

/// Without the dictionary a compressed record must surface a clean error on wasm,
/// never a panic (the browser would otherwise crash the module).
#[wasm_bindgen_test]
async fn missing_dict_errors_not_panics_on_wasm() {
    let store = RecordStore::open(
        MemoryFetch::new(IDX.to_vec()),
        MemoryFetch::new(BIN.to_vec()),
    )
    .await
    .expect("open store without dict");
    assert!(
        store.get(0).await.is_err(),
        "a compressed record without a dictionary must error"
    );
}
