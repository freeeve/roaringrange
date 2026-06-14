"""Train FAISS OPQ/IVF/PQ over an `embed_gemma_memmap.py` float32 memmap and write the `RRVI` —
the build step that script's docstring defers to.

Reads the first `N` (already L2-normalized) vectors from `<prefix>.f32` and their doc ids from
`<prefix>.ids`, then reuses `faiss_to_rrvi.export_to_rrvi` (metric `ip` → normalize + METRIC_L2,
the same recipe as `build_full_rrvi_gemma.py`, so the query Lambda stays compatible). Because it
only reads the first `N` rows, it can run against a **partial / in-progress** embed: pass an
`N` at or below the embedder's checkpoint and it reads only the written prefix, without
disturbing the still-running embed.

    python build_rrvi_from_memmap.py <prefix> <N> <out.rrvi> [nlist=4096] [m=32]

The doc ids in `<prefix>.ids` are the corpus doc ids (rank order, doc 0 = most cited), so the
RRVI's `doc_id` aligns with the records store and the text index.
"""
from __future__ import annotations

import os

# faiss bundles an OpenMP runtime; on macOS loading it alongside numpy's BLAS OpenMP aborts
# (SIGBUS/segfault) unless duplicate libs are allowed. Set this before numpy/faiss import.
os.environ.setdefault("KMP_DUPLICATE_LIB_OK", "TRUE")
os.environ.setdefault("OMP_NUM_THREADS", "4")

import sys  # noqa: E402

# This script only TRAINS over an existing memmap — it never embeds, so do NOT import torch:
# torch and faiss-cpu each bundle their own OpenMP runtime, and loading both on macOS arm64
# SIGSEGVs inside faiss training (faiss-cpu 1.14.3). KMP_DUPLICATE_LIB_OK (set above) covers
# the numpy-BLAS-vs-faiss OpenMP overlap; faiss alone trains cleanly.
import faiss  # noqa: E402,F401
import numpy as np  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from faiss_to_rrvi import export_to_rrvi  # noqa: E402

DIM = 512

PREFIX = sys.argv[1]
N = int(sys.argv[2])
OUT = sys.argv[3]
NLIST = int(sys.argv[4]) if len(sys.argv) > 4 else 4096
M = int(sys.argv[5]) if len(sys.argv) > 5 else 32

# Map only the first N rows of the (10M-sized) memmaps — the stable, already-written
# prefix. The read-only maps are passed straight to faiss: the embedder already
# L2-normalized every row, so `normalized=True` skips export_to_rrvi's in-place
# renormalize — which would otherwise write through the no-copy view into the
# read-only pages (SIGBUS) or force a ~N×512×4-byte RAM copy.
vectors = np.memmap(f"{PREFIX}.f32", dtype="float32", mode="r", shape=(N, DIM))
doc_ids = np.memmap(f"{PREFIX}.ids", dtype="uint32", mode="r", shape=(N,))
print(
    f"loaded {N:,} x {DIM} from {PREFIX}.f32  (doc_ids {int(doc_ids[0])}..{int(doc_ids[-1])})",
    flush=True,
)

stats = export_to_rrvi(
    vectors,
    doc_ids,
    OUT,
    NLIST,
    M,
    metric="ip",
    train_size=min(N, 2_000_000),
    normalized=True,
)
print(f"wrote {OUT}: {stats}", flush=True)
