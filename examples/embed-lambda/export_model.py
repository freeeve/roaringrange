"""Export a sentence-transformers embedder (default EmbeddingGemma-300M) to the
artifacts the embed Lambda serves: a single ONNX graph (transformer + pooling +
any dense + normalize baked in), the tokenizer, and a `recipe.json` carrying the
query/document prompt strings + output dim. Then VALIDATE that the Lambda's
onnxruntime recipe reproduces `SentenceTransformer.encode_query` to cosine
> 0.999 — the #1 correctness check (corpus and query must share the exact recipe).

Run locally (needs torch + sentence-transformers + the model; license-gated):
    pip install 'roaringrange[gemma]' optimum onnxruntime
    python export_model.py --model google/embeddinggemma-300m --out model --dim 512

The Dockerfile copies the resulting `model/` into the image; the runtime needs
only onnxruntime + tokenizers (no torch).
"""
from __future__ import annotations

import argparse
import json
import os

import numpy as np


def export(model_name: str, out_dir: str, dim: int):
    import torch
    from sentence_transformers import SentenceTransformer

    st = SentenceTransformer(model_name)
    st.eval()
    os.makedirs(out_dir, exist_ok=True)

    # Wrap the full ST pipeline so the ONNX graph maps tokenized input directly to
    # the final (pooled, dense-projected, normalized) sentence embedding — the
    # handler then only tokenizes and (MRL-) truncates, replicating no recipe math.
    class FullPipeline(torch.nn.Module):
        def __init__(self, model):
            super().__init__()
            self.model = model

        def forward(self, input_ids, attention_mask):
            out = self.model({"input_ids": input_ids, "attention_mask": attention_mask})
            return out["sentence_embedding"]

    wrap = FullPipeline(st)
    enc = st.tokenizer(
        ["the quick brown fox"], padding=True, truncation=True, return_tensors="pt"
    )
    onnx_path = os.path.join(out_dir, "model.onnx")
    torch.onnx.export(
        wrap,
        (enc["input_ids"], enc["attention_mask"]),
        onnx_path,
        input_names=["input_ids", "attention_mask"],
        output_names=["embedding"],
        dynamic_axes={
            "input_ids": {0: "batch", 1: "seq"},
            "attention_mask": {0: "batch", 1: "seq"},
            "embedding": {0: "batch"},
        },
        opset_version=17,
    )

    # tokenizer.json (+ tokenizer config) for the runtime `tokenizers` loader.
    st.tokenizer.save_pretrained(out_dir)

    # Prompt strings (prepended to the text). EmbeddingGemma is asymmetric.
    prompts = dict(getattr(st, "prompts", {}) or {})
    query_prompt = prompts.get("query", prompts.get("Retrieval-query", ""))
    document_prompt = prompts.get("document", prompts.get("Retrieval-document", ""))
    recipe = {
        "model": model_name,
        "dim": dim,
        "query_prompt": query_prompt,
        "document_prompt": document_prompt,
        "all_prompts": prompts,
    }
    with open(os.path.join(out_dir, "recipe.json"), "w") as f:
        json.dump(recipe, f, indent=2)
    print(f"exported -> {out_dir} (query_prompt={query_prompt!r})")
    return st


def validate(st, out_dir: str, dim: int):
    """Replicates the Lambda recipe with onnxruntime + tokenizers and asserts it
    matches sentence-transformers' encode_query."""
    import onnxruntime as ort
    from tokenizers import Tokenizer

    recipe = json.load(open(os.path.join(out_dir, "recipe.json")))
    tok = Tokenizer.from_file(os.path.join(out_dir, "tokenizer.json"))
    sess = ort.InferenceSession(
        os.path.join(out_dir, "model.onnx"), providers=["CPUExecutionProvider"]
    )
    in_names = {i.name for i in sess.get_inputs()}

    def lambda_recipe(text):
        enc = tok.encode(recipe["query_prompt"] + text)
        feed = {}
        if "input_ids" in in_names:
            feed["input_ids"] = np.array([enc.ids], dtype=np.int64)
        if "attention_mask" in in_names:
            feed["attention_mask"] = np.array([enc.attention_mask], dtype=np.int64)
        out = sess.run(None, feed)[0][0]
        v = out[:dim].astype(np.float32)
        n = np.linalg.norm(v)
        return v / n if n else v

    queries = [
        "self-supervised representation learning",
        "CRISPR genome editing",
        "programming language for statistical data analysis",
    ]
    ref = st.encode_query(queries, truncate_dim=dim, normalize_embeddings=True)
    worst = 1.0
    for q, r in zip(queries, ref):
        mine = lambda_recipe(q)
        cos = float(np.dot(mine, r) / (np.linalg.norm(mine) * np.linalg.norm(r)))
        worst = min(worst, cos)
        print(f"  cos={cos:.5f}  {q!r}")
    if worst < 0.999:
        raise SystemExit(f"recipe MISMATCH (worst cos {worst:.5f}) — Lambda would query a different space")
    print(f"OK: Lambda onnx recipe matches encode_query (worst cos {worst:.5f})")


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
