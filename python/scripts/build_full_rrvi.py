"""Build a model2vec semantic `RRVI` over the WHOLE corpus by streaming — the
shared `rrvi_stream.build_rrvi_streaming` pipeline (records → embed → FAISS
`OPQ,IVF,PQ` → `.rrvi`) with the in-browser-compatible model2vec embedder
(`potion-retrieval-32M`, the same matrix the demo's wasm embedder uses).

    python build_full_rrvi.py <N> <out.rrvi> [nlist] [m]

N is the number of docs (e.g. 484369476, or a small value to test). doc_id order
is the dump order (rank order), shared with the text index + record store.
"""
from __future__ import annotations

import sys

import faiss
import numpy as np
from model2vec import StaticModel

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from rrvi_stream import build_rrvi_streaming  # noqa: E402

N = int(sys.argv[1])
OUT = sys.argv[2]
NLIST = int(sys.argv[3]) if len(sys.argv) > 3 else 16384
M = int(sys.argv[4]) if len(sys.argv) > 4 else 32

model = StaticModel.from_pretrained("minishlab/potion-retrieval-32M")


def embed(texts):
    emb = np.asarray(model.encode(texts), dtype="float32")
    faiss.normalize_L2(emb)
    return emb


build_rrvi_streaming(N, OUT, embed, nlist=NLIST, m=M, batch=200_000, log_every=5_000_000)
