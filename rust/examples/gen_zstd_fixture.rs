//! Generates the zstd record-store fixtures the wasm decode test consumes.
//!
//! Dictionary training and compression need the native C `zstd` crate, so the
//! fixtures are produced here (native, `--features zstd`) and checked in; the
//! browser only ever *decodes*, which is exactly what the wasm test exercises
//! against these bytes. Run from `rust/`:
//!
//!   cargo run --example gen_zstd_fixture --features zstd
//!
//! Writes `tests/fixtures/records.{dict,idx,bin}` plus `records.expect` (the
//! decoded bytes of doc 0, so the wasm test asserts against a single source).
//!
//! Native-only: it uses the C-backed encoder/trainer (`crate::build`), which is
//! excluded from wasm32. Gated so `wasm-pack test` (which compiles examples for
//! wasm) skips it.
#[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
fn main() -> std::io::Result<()> {
    use std::fs;
    use std::path::Path;

    // A small, realistic, repetitive corpus so the trained dictionary has shared
    // substrings to exploit (mirrors the native round-trip test's records).
    let recs: Vec<Vec<u8>> = (0..200u32)
        .map(|i| format!("record number {i} about science and research").into_bytes())
        .collect();
    let samples: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
    let dict = roaringrange::build::train_record_dict(&samples, 4 * 1024)?;

    let mut bin = Vec::new();
    let mut idx = Vec::new();
    roaringrange::build::write_records_zstd(&mut bin, &mut idx, &recs, &dict, 19)?;

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("records.dict"), &dict)?;
    fs::write(dir.join("records.idx"), &idx)?;
    fs::write(dir.join("records.bin"), &bin)?;
    fs::write(dir.join("records.expect"), &recs[0])?;

    println!(
        "wrote fixtures to {}: dict={}B idx={}B bin={}B (200 recs, doc0={:?})",
        dir.display(),
        dict.len(),
        idx.len(),
        bin.len(),
        String::from_utf8_lossy(&recs[0]),
    );
    Ok(())
}

// Fallback for any config where the real `main` is gated out: the feature is off,
// or the target is wasm32 (no native encoder there).
#[cfg(not(all(feature = "zstd", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!("run natively with --features zstd to generate the fixtures");
}
