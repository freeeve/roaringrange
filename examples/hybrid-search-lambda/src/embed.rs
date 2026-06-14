//! In-process EmbeddingGemma query embedder — a byte-faithful Rust port of the Python
//! `embed-lambda` (`handler.py`). Eliminates the cross-Lambda hop for the hybrid vector arm:
//! the query is embedded here, in the same container that searches the indexes.
//!
//! The recipe MUST match the corpus embed exactly or the RRVI similarity search degrades:
//! query-prompt + tokenize → transformer ONNX (`last_hidden_state`) → masked mean-pool →
//! the model's dense layers → L2-normalize → MRL-truncate to `dim` → L2-normalize. Same
//! ONNX graph (`model.int8.onnx`), same `tokenizer.json`, same `recipe.json`, and the dense
//! weights as raw little-endian f32 (`dense_w0/b0/w1/b1.bin`, converted from `dense.npz`).
//!
//! onnxruntime is loaded dynamically (`ort` `load-dynamic`) from `ORT_DYLIB_PATH`, so the
//! native lib ships in the container image rather than being linked at build time.

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use std::time::Instant;
use tokenizers::Tokenizer;

/// Hidden size of EmbeddingGemma-300M (`last_hidden_state` width, pooled width).
const HIDDEN: usize = 768;
/// Bottleneck width of the first dense layer (768 → 3072 → 768).
const BOTTLENECK: usize = 3072;

/// Dense activation, matching `handler.py::_activation`. The shipped recipe is `Identity`
/// for both layers; the others are ported so a recipe change can't silently mis-embed.
fn activation(name: &str, x: &mut [f32]) {
    match name {
        "Identity" | "Linear" | "" => {}
        "Tanh" => x.iter_mut().for_each(|v| *v = v.tanh()),
        "ReLU" => x.iter_mut().for_each(|v| *v = v.max(0.0)),
        "GELU" | "GELUActivation" => x
            .iter_mut()
            .for_each(|v| *v = 0.5 * *v * (1.0 + libm_erf(*v / std::f32::consts::SQRT_2))),
        other => panic!("unsupported dense activation {other:?}"),
    }
}

/// `erf` via the Abramowitz–Stegun 7.1.26 approximation (no libm dep). Only used for the
/// GELU path, which the shipped Identity recipe never hits; kept for recipe-change safety.
fn libm_erf(x: f32) -> f32 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-x * x).exp();
    s * y
}

/// `y = x · Wᵀ + b`, where `w` is the row-major `(out, inp)` weight matrix (row `o` is
/// output `o`'s weights over the inputs) — i.e. exactly numpy's `x @ W.T + b`.
fn linear(x: &[f32], w: &[f32], b: &[f32], out: usize, inp: usize) -> Vec<f32> {
    let mut y = vec![0f32; out];
    for o in 0..out {
        let row = &w[o * inp..o * inp + inp];
        let mut acc = b[o];
        for i in 0..inp {
            acc += x[i] * row[i];
        }
        y[o] = acc;
    }
    y
}

fn l2_normalize(x: &mut [f32]) {
    let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        x.iter_mut().for_each(|v| *v /= norm);
    }
}

fn read_f32(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() % 4 != 0 {
        return Err(format!("{} not a multiple of 4 bytes", path.display()));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// The embedder, loaded once per warm container and reused across invocations. The ort
/// `Session` is behind a `Mutex` so the embedder can live in the shared `&'static` handler
/// state (Lambda runs one request per container, so there is no real contention).
pub struct Embedder {
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    query_prompt: String,
    dim: usize,
    dense_acts: Vec<String>,
    w0: Vec<f32>,
    b0: Vec<f32>,
    w1: Vec<f32>,
    b1: Vec<f32>,
}

impl Embedder {
    /// Load the ONNX session, tokenizer, recipe, and dense weights from `dir` (the baked
    /// `model/`). `ORT_DYLIB_PATH` must point at the onnxruntime shared library.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let recipe: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join("recipe.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("recipe.json: {e}"))?;
        let query_prompt = recipe
            .get("query_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let dim = recipe
            .get("dim")
            .and_then(|v| v.as_u64())
            .unwrap_or(HIDDEN as u64) as usize;
        let dense_acts: Vec<String> = recipe
            .get("dense_acts")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer: {e}"))?;
        // onnxruntime can't read CPU topology on Graviton (cpuinfo probe fails), so set the
        // intra-op thread count explicitly. The model is already graph-optimized + quantized
        // at export time (optimum), so cap runtime optimization at Level1 — the expensive
        // extended/layout passes (Level3, the default) dominate cold-start session creation
        // and are redundant here.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let t = Instant::now();
        let session = Session::builder()
            .map_err(|e| format!("ort builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level1)
            .map_err(|e| format!("ort opt level: {e}"))?
            .with_intra_threads(threads)
            .map_err(|e| format!("ort threads: {e}"))?
            .commit_from_file(dir.join("model.int8.onnx"))
            .map_err(|e| format!("ort load model: {e}"))?;
        eprintln!(
            "embed: ONNX session built in {:.1}s ({threads} threads)",
            t.elapsed().as_secs_f32()
        );

        Ok(Self {
            session: std::sync::Mutex::new(session),
            tokenizer,
            query_prompt,
            dim,
            dense_acts,
            w0: read_f32(&dir.join("dense_w0.bin"))?,
            b0: read_f32(&dir.join("dense_b0.bin"))?,
            w1: read_f32(&dir.join("dense_w1.bin"))?,
            b1: read_f32(&dir.join("dense_b1.bin"))?,
        })
    }

    /// Embed a query into the `dim`-d unit vector matching the Gemma RRVI's corpus recipe.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        // add_special_tokens=true matches the Python tokenizers default (`Tokenizer.encode`),
        // so the BOS/EOS the corpus embed saw are present here too.
        let enc = self
            .tokenizer
            .encode(format!("{}{}", self.query_prompt, text), true)
            .map_err(|e| format!("tokenize: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&i| i as i64).collect();
        let seq = ids.len();

        let ids_t =
            Tensor::from_array(([1usize, seq], ids)).map_err(|e| format!("ids tensor: {e}"))?;
        let mask_t = Tensor::from_array(([1usize, seq], mask.clone()))
            .map_err(|e| format!("mask tensor: {e}"))?;

        // Masked mean-pool over the sequence: sum(lhs[t] * mask[t]) / sum(mask). Computed
        // inside the session lock so `lhs` (borrowed from the run outputs) stays valid.
        let mut pooled = vec![0f32; HIDDEN];
        {
            let mut session = self.session.lock().unwrap();
            let outputs = session
                .run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t])
                .map_err(|e| format!("ort run: {e}"))?;
            // last_hidden_state: [1, seq, HIDDEN].
            let (shape, lhs) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("extract: {e}"))?;
            let hidden = *shape.last().unwrap() as usize;
            if hidden != HIDDEN {
                return Err(format!("hidden {hidden} != {HIDDEN}"));
            }
            let mut denom = 0f32;
            for t in 0..seq {
                let m = mask[t] as f32;
                if m == 0.0 {
                    continue;
                }
                denom += m;
                let row = &lhs[t * hidden..t * hidden + hidden];
                for h in 0..hidden {
                    pooled[h] += row[h] * m;
                }
            }
            let denom = denom.max(1.0);
            pooled.iter_mut().for_each(|v| *v /= denom);
        }

        // Dense head: 768 → 3072 → 768, activation per recipe (Identity in the shipped model).
        let mut x = linear(&pooled, &self.w0, &self.b0, BOTTLENECK, HIDDEN);
        if let Some(a) = self.dense_acts.first() {
            activation(a, &mut x);
        }
        let mut x = linear(&x, &self.w1, &self.b1, HIDDEN, BOTTLENECK);
        if let Some(a) = self.dense_acts.get(1) {
            activation(a, &mut x);
        }

        // L2 → MRL-truncate to dim → L2.
        l2_normalize(&mut x);
        x.truncate(self.dim.min(x.len()));
        l2_normalize(&mut x);
        Ok(x)
    }
}
