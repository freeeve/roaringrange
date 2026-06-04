"""Compare embedders on the OpenAlex subset before committing to a full embed (the
spec's "decide the keeper first"). For each natural-language query it embeds the
corpus and the query with the chosen embedder, ranks by exact cosine, and prints
the top-k titles — eyeball model2vec (mode 2) vs Gemma (mode 1) side by side.

    # mode 2 (no extra setup):
    python python/scripts/bench_embedders.py head.jsonl --embedder model2vec --limit 20000
    # mode 1 (needs the gemma extra + HF access):
    python python/scripts/bench_embedders.py head.jsonl --embedder gemma --limit 20000

Mixes keyword queries (token overlap — easy for model2vec) with paraphrase queries
(no shared words — where a semantic model should pull ahead), so the difference is
visible. Uses exact cosine (numpy), so it measures the embedder, not the index.
"""
from __future__ import annotations

import argparse
import json
import sys

import numpy as np

sys.path.insert(0, "python/scripts")

# Keyword queries (token overlap with titles) + paraphrase queries (little/no
# shared vocabulary — these separate a semantic model from a bag-of-tokens one).
DEFAULT_QUERIES = [
    "CRISPR genome editing",
    "transformer attention neural machine translation",
    "black hole gravitational waves detection",
    "programming language for statistical data analysis",  # → R
    "a method to measure protein concentration in a sample",  # → Lowry
    "deep neural network for recognizing objects in images",  # → ResNet/ImageNet
]


def read_corpus(path, limit):
    ids, titles, texts = [], [], []
    with open(path, encoding="utf-8") as f:
        for line in f:
            did, _, js = line.partition("\t")
            rec = json.loads(js)
            t = rec.get("t") or ""
            ab = rec.get("ab") or ""
            ids.append(int(did))
            titles.append(t)
            texts.append((t + " " + ab).strip())
            if limit and len(ids) >= limit:
                break
    return ids, titles, texts


def embed(embedder, texts, queries, model_name):
    """Returns (corpus_emb, query_emb) as float32 arrays for the chosen embedder."""
    if embedder == "model2vec":
        from model2vec_embed import embed_texts

        name = model_name or "minishlab/potion-retrieval-32M"
        return embed_texts(texts, name), embed_texts(queries, name)
    if embedder == "gemma":
        import gemma_embed

        model = gemma_embed.load(model_name or gemma_embed.DEFAULT_MODEL)
        return (
            gemma_embed.embed_documents(model, texts),
            gemma_embed.embed_query(model, queries),
        )
    raise SystemExit(f"unknown embedder {embedder!r} (use model2vec or gemma)")


def normalize_rows(x):
    n = np.linalg.norm(x, axis=1, keepdims=True)
    n[n == 0] = 1.0
    return x / n


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("jsonl")
    p.add_argument("--embedder", choices=["model2vec", "gemma"], default="model2vec")
    p.add_argument("--model", default="")
    p.add_argument("--limit", type=int, default=20000)
    p.add_argument("--k", type=int, default=5)
    p.add_argument("query", nargs="*", help="override the default query set")
    a = p.parse_args()

    queries = a.query or DEFAULT_QUERIES
    ids, titles, texts = read_corpus(a.jsonl, a.limit)
    print(f"corpus {len(texts)} docs; embedder={a.embedder}", flush=True)

    corpus_emb, query_emb = embed(a.embedder, texts, queries, a.model)
    corpus_emb = normalize_rows(np.asarray(corpus_emb, dtype="float32"))
    query_emb = normalize_rows(np.asarray(query_emb, dtype="float32"))

    sims = query_emb @ corpus_emb.T  # cosine (rows normalized)
    for qi, q in enumerate(queries):
        top = np.argsort(-sims[qi])[: a.k]
        print(f"\nquery: {q!r}")
        for rank, j in enumerate(top, 1):
            print(f"  {rank}. ({sims[qi, j]:.3f}) [{ids[j]}] {titles[j][:88]}")


if __name__ == "__main__":
    main()
