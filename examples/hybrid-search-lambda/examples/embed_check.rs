//! Parity harness: embed a query with the Rust Embedder and print the vector as JSON, so it
//! can be compared (cosine) against the Python embed-lambda's output for the same query.
//! Run with ORT_DYLIB_PATH pointing at a local onnxruntime shared library.
#[path = "../src/embed.rs"]
mod embed;

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("EMBED_MODEL_DIR").unwrap_or_else(|_| "model".into()));
    let emb = embed::Embedder::load(&dir).expect("load embedder");
    let q = std::env::args().nth(1).unwrap_or_else(|| "cancer".into());
    let v = emb.embed(&q).expect("embed");
    eprintln!(
        "dim={} norm={:.6} first8={:?}",
        v.len(),
        v.iter().map(|x| x * x).sum::<f32>().sqrt(),
        &v[..8]
    );
    println!("{}", serde_json::to_string(&v).unwrap());
}
