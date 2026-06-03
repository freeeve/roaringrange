"""Embed text with a model2vec static model and build an `RRVI` similarity index
(vector-search task, mode 2 — "pure static": the same model can run in the browser
at query time, so no backend is needed for similarity search).

`minishlab/potion-retrieval-32M` produces 512-d embeddings by mean-pooling static
token vectors — no transformer, no GPU, fast on CPU. This script embeds your
corpus text and builds a `.rrvi` with the in-wheel `VectorBuilder` (the native
IVFPQ trainer; no FAISS needed). Use the SAME `doc_id` as the text index so a hit
maps to the same record.

Install the embedder:  pip install model2vec
(the roaringrange wheel itself has no such dependency).

CRITICAL recipe note: the query side must embed with the *same* model + pooling so
the query and document vectors share a space. Mode 2's whole point is running this
exact model2vec recipe in the browser for the query (a follow-up: a wasm tokenizer
+ the token-embedding matrix); a hosted embedder must match it byte-for-byte.
"""
from __future__ import annotations

import argparse

import numpy as np

import roaringrange as rr

DEFAULT_MODEL = "minishlab/potion-retrieval-32M"


def embed_texts(texts, model_name: str = DEFAULT_MODEL) -> np.ndarray:
    """Returns an `(n, dim)` float32 array of model2vec embeddings for `texts`."""
    try:
        from model2vec import StaticModel
    except ImportError as e:  # pragma: no cover - environment dependent
        raise SystemExit("model2vec is required: pip install model2vec") from e
    model = StaticModel.from_pretrained(model_name)
    return np.asarray(model.encode(list(texts)), dtype="float32")


def build_rrvi_from_texts(
    texts,
    doc_ids,
    out_path: str,
    nlist: int,
    m: int,
    metric: str = "ip",
    model_name: str = DEFAULT_MODEL,
):
    """Embeds `texts` and builds `out_path` with `VectorBuilder`. Returns
    `(VectorBuildStats, embeddings)`."""
    emb = embed_texts(texts, model_name)
    n, dim = emb.shape
    doc_ids = list(doc_ids)
    if len(doc_ids) != n:
        raise ValueError("doc_ids length must match the number of texts")
    if dim % m:
        raise ValueError(f"m={m} must divide the embedding dim={dim}")

    vb = rr.VectorBuilder(dim=dim, nlist=nlist, m=m, metric=metric)
    vb.add_many([(int(doc_ids[i]), emb[i].tolist()) for i in range(n)])
    return vb.build(out_path), emb


_SAMPLE = [
    "Deep residual learning for image recognition with convolutional networks",
    "Attention is all you need: the transformer architecture for sequence models",
    "A method for stochastic optimization of neural network training (Adam)",
    "Generative adversarial networks for image synthesis",
    "BERT: pre-training of deep bidirectional transformers for language understanding",
    "Random forests for classification and regression",
    "Support vector machines and the kernel trick",
    "The structure of the nucleic acids: the double helix of DNA",
    "General relativity and the curvature of spacetime",
    "CRISPR-Cas9 genome editing in human cells",
    "Observation of gravitational waves from a binary black hole merger",
    "A fast quicksort algorithm for sorting arrays in place",
]


def _demo(args):
    """Embed a few sample titles, build a tiny RRVI, and write a query vector so
    the result can be checked with the Rust `rrvi_query` example."""
    import struct

    texts = _SAMPLE
    doc_ids = np.arange(len(texts), dtype="uint32")
    stats, emb = build_rrvi_from_texts(
        texts, doc_ids, args.out, args.nlist, args.m, metric="ip", model_name=args.model
    )
    print("wrote", args.out, "->", stats, "dim", emb.shape[1])

    query = "neural network attention model for translation"
    qv = embed_texts([query], args.model)[0].astype("<f4")
    with open(args.query_out, "wb") as f:
        f.write(struct.pack("<II", 1, emb.shape[1]))
        f.write(qv.tobytes())
    print(f"query {query!r} -> {args.query_out}")
    print("check with: cargo run --release --example rrvi_query --features vector -- "
          f"{args.out} {args.query_out} 3 {args.nlist}")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", default="vectors.rrvi")
    p.add_argument("--query-out", default="query.bin")
    p.add_argument("--model", default=DEFAULT_MODEL)
    p.add_argument("--m", type=int, default=32, help="PQ subquantizers (must divide dim)")
    p.add_argument("--nlist", type=int, default=4)
    _demo(p.parse_args())
