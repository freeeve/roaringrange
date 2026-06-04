"""Export the artifacts the embed Lambda serves (default EmbeddingGemma-300M),
then VALIDATE the Lambda recipe against sentence-transformers.

Raw `torch.onnx.export` trips on Gemma3's attention and optimum's
sentence-transformers export clashes with current ST, so we split it: optimum
exports just the **transformer** to ONNX (well supported), and we extract the
model's post-steps from the ST model — masked mean-pool (config), the **dense**
layers (`dense.npz` + their activations), and the query/document **prompts** — into
`recipe.json`. The handler replays exactly those steps. Validation imports the real
`handler` and asserts cos(handler.embed_query, encode_query) > 0.999.

Run locally (needs torch + sentence-transformers + optimum-onnx + onnxruntime + the
gated model):
    pip install 'roaringrange[gemma]' optimum optimum-onnx onnxruntime
    # accept terms at hf.co/google/embeddinggemma-300m, then `huggingface-cli login`
    python export_model.py --model google/embeddinggemma-300m --out model --dim 512
"""
from __future__ import annotations

import argparse
import json
import os

import numpy as np

QUERIES = [
    "self-supervised representation learning",
    "CRISPR genome editing",
    "programming language for statistical data analysis",
]


def export(model_name: str, out_dir: str, dim: int):
    from optimum.exporters.onnx import main_export
    from sentence_transformers import SentenceTransformer

    os.makedirs(out_dir, exist_ok=True)

    # 1) transformer -> ONNX (outputs last_hidden_state). library_name="transformers"
    #    avoids optimum's sentence-transformers path (incompatible with ST 5.x).
    main_export(
        model_name,
        output=out_dir,
        task="feature-extraction",
        library_name="transformers",
        opset=17,
    )

    # 2) extract the post-transformer recipe straight from the ST model.
    st = SentenceTransformer(model_name, device="cpu")
    pool = st[1].get_config_dict()
    if not (pool.get("pooling_mode_mean_tokens") or pool.get("pooling_mode") == "mean"):
        raise SystemExit(f"expected mean pooling, got {pool}")

    dense_acts = []
    npz = {}
    di = 0
    for mod in st:
        if type(mod).__name__ == "Dense":
            w = mod.linear.weight.detach().cpu().numpy().astype("float32")
            bias = mod.linear.bias
            b = (
                bias.detach().cpu().numpy().astype("float32")
                if bias is not None
                else np.zeros(w.shape[0], dtype="float32")
            )
            npz[f"W{di}"] = w
            npz[f"b{di}"] = b
            dense_acts.append(type(mod.activation_function).__name__)
            di += 1
    np.savez(os.path.join(out_dir, "dense.npz"), **npz)

    prompts = dict(getattr(st, "prompts", {}) or {})
    recipe = {
        "model": model_name,
        "dim": dim,
        "pooling": "mean",
        "n_dense": di,
        "dense_acts": dense_acts,
        "query_prompt": prompts.get("query", prompts.get("Retrieval-query", "")),
        "document_prompt": prompts.get("document", prompts.get("Retrieval-document", "")),
        "all_prompts": prompts,
    }
    with open(os.path.join(out_dir, "recipe.json"), "w") as f:
        json.dump(recipe, f, indent=2)
    print(f"exported -> {out_dir}: {di} dense layer(s) {dense_acts}, "
          f"query_prompt={recipe['query_prompt']!r}")
    return st


def validate(st, out_dir: str, dim: int):
    # Import the REAL handler against the just-written artifacts (no recipe drift).
    os.environ["EMBED_MODEL_DIR"] = os.path.abspath(out_dir)
    import importlib

    import handler

    importlib.reload(handler)

    ref = st.encode_query(QUERIES, truncate_dim=dim, normalize_embeddings=True)
    worst = 1.0
    for q, r in zip(QUERIES, ref):
        mine = handler.embed_query(q)
        cos = float(np.dot(mine, r) / (np.linalg.norm(mine) * np.linalg.norm(r)))
        worst = min(worst, cos)
        print(f"  cos={cos:.5f}  {q!r}")
    if worst < 0.999:
        raise SystemExit(f"recipe MISMATCH (worst cos {worst:.5f}) — Lambda would query a different space")
    print(f"OK: Lambda recipe matches encode_query (worst cos {worst:.5f})")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--model", default="google/embeddinggemma-300m")
    p.add_argument("--out", default="model")
    p.add_argument("--dim", type=int, default=512)
    a = p.parse_args()
    st = export(a.model, a.out, a.dim)
    validate(st, a.out, a.dim)


if __name__ == "__main__":
    main()
