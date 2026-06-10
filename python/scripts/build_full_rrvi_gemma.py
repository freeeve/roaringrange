"""Build a Gemma (EmbeddingGemma-300M, mode 1) semantic `RRVI` over the corpus by
streaming — the shared `rrvi_stream.build_rrvi_streaming` pipeline, embedding
documents with EmbeddingGemma (MRL-truncated to 512, asymmetric *document*
prompt) instead of model2vec. The query side (the Lambda / `encode_query`) must
use the same model — that is the one correctness invariant.

    python build_full_rrvi_gemma.py <N> <out.rrvi> [nlist] [m]
"""
from __future__ import annotations

import os
import sys

# faiss and torch each bundle an OpenMP runtime; on macOS loading both aborts (segfault)
# unless duplicate libs are allowed. Set this before importing either, and import torch
# (via sentence_transformers) BEFORE faiss.
os.environ.setdefault("KMP_DUPLICATE_LIB_OK", "TRUE")
os.environ.setdefault("OMP_NUM_THREADS", "4")

from sentence_transformers import SentenceTransformer  # noqa: E402  torch — before faiss
import faiss  # noqa: E402
import numpy as np  # noqa: E402

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from rrvi_stream import build_rrvi_streaming  # noqa: E402

N = int(sys.argv[1])
OUT = sys.argv[2]
NLIST = int(sys.argv[3]) if len(sys.argv) > 3 else 16384
M = int(sys.argv[4]) if len(sys.argv) > 4 else 32
DIM = 512
ENCODE_BATCH = 64  # the transformer's internal forward-pass batch (MPS memory)

model = SentenceTransformer("google/embeddinggemma-300m", truncate_dim=DIM, device="mps")
print("loaded EmbeddingGemma-300M (truncate_dim=512, mps)", flush=True)


def embed(texts):
    emb = np.asarray(
        model.encode_document(texts, batch_size=ENCODE_BATCH, show_progress_bar=False),
        dtype="float32",
    )
    faiss.normalize_L2(emb)
    return emb


# Small doc batches: the transformer dominates, and a 200k-text batch would spend
# hours inside one embed call between progress logs.
build_rrvi_streaming(N, OUT, embed, nlist=NLIST, m=M, dim=DIM, batch=4096, log_every=1_000_000)
