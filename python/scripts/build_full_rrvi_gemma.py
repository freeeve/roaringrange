"""Build a Gemma (EmbeddingGemma-300M, mode 1) semantic `RRVI` over the corpus by
streaming, the same FAISS `OPQ,IVF,PQ` pipeline as `build_full_rrvi.py` but embedding
documents with EmbeddingGemma (MRL-truncated to 512, asymmetric *document* prompt)
instead of model2vec. The query side (the Lambda / `encode_query`) must use the same
model — that is the one correctness invariant.

    python build_full_rrvi_gemma.py <N> <out.rrvi> [nlist] [m]
"""
from __future__ import annotations

import gc
import json
import os
import subprocess
import sys
import time

# faiss and torch each bundle an OpenMP runtime; on macOS loading both aborts (segfault)
# unless duplicate libs are allowed. Set this before importing either, and import torch
# (via sentence_transformers) BEFORE faiss.
os.environ.setdefault("KMP_DUPLICATE_LIB_OK", "TRUE")
os.environ.setdefault("OMP_NUM_THREADS", "4")

from sentence_transformers import SentenceTransformer  # noqa: E402  torch — before faiss
import faiss  # noqa: E402
import numpy as np  # noqa: E402
import roaringrange as rr  # noqa: E402

N = int(sys.argv[1])
OUT = sys.argv[2]
NLIST = int(sys.argv[3]) if len(sys.argv) > 3 else 16384
M = int(sys.argv[4]) if len(sys.argv) > 4 else 32
DIM, BATCH, TRAIN_SAMPLE = 512, 4096, 2_000_000
ENCODE_BATCH = 64  # the transformer's internal forward-pass batch (MPS memory)
DUMP = "rust/target/release/examples/dump_records"
IDX, BIN, DICT = (
    "/tmp/oa-out/records-full.idx",
    "/tmp/oa-out/records-full.bin",
    "/tmp/oa-out/openalex-full.dict",
)

t0 = time.time()


def log(msg):
    print(f"[{time.time() - t0:7.0f}s] {msg}", flush=True)


model = SentenceTransformer("google/embeddinggemma-300m", truncate_dim=DIM, device="mps")
log("loaded EmbeddingGemma-300M (truncate_dim=512, mps)")
index = faiss.index_factory(DIM, f"OPQ{M},IVF{NLIST},PQ{M}", faiss.METRIC_L2)

proc = subprocess.Popen([DUMP, IDX, BIN, DICT, str(N)], stdout=subprocess.PIPE, bufsize=1 << 22)


def embed(texts):
    emb = np.asarray(
        model.encode_document(texts, batch_size=ENCODE_BATCH, show_progress_bar=False),
        dtype="float32",
    )
    faiss.normalize_L2(emb)
    return emb


def batches():
    ids, texts = [], []
    for line in proc.stdout:
        did, _, js = line.partition(b"\t")
        try:
            rec = json.loads(js)
        except ValueError:
            continue
        ids.append(int(did))
        texts.append(((rec.get("t") or "") + " " + (rec.get("ab") or "")).strip())
        if len(ids) >= BATCH:
            yield np.asarray(ids, dtype="int64"), embed(texts)
            ids, texts = [], []
    if ids:
        yield np.asarray(ids, dtype="int64"), embed(texts)


gen = batches()

# Phase 1: buffer a training sample, train OPQ/IVF/PQ.
buf, ntrain = [], 0
for bids, bvecs in gen:
    buf.append((bids, bvecs))
    ntrain += len(bids)
    if ntrain >= TRAIN_SAMPLE:
        break
log(f"training OPQ{M},IVF{NLIST},PQ{M} on {ntrain:,} sampled vectors...")
index.train(np.concatenate([v for _, v in buf]))
log("trained; adding all vectors (streaming)...")

# Phase 2: add the buffered batches, then stream the rest.
added = 0
for bids, bvecs in buf:
    index.add_with_ids(bvecs, bids)
    added += len(bids)
buf = None
for bids, bvecs in gen:
    index.add_with_ids(bvecs, bids)
    added += len(bids)
    if added % 1_000_000 < BATCH:
        log(f"added {added:,}")
log(f"added all {added:,}; extracting trained parts...")

opq_vt = faiss.downcast_VectorTransform(index.chain.at(0))
opq = faiss.vector_to_array(opq_vt.A).astype("<f4")
ivf = faiss.downcast_index(faiss.extract_index_ivf(index))
quant = faiss.downcast_index(ivf.quantizer)
centroids = quant.reconstruct_n(0, NLIST).reshape(NLIST, DIM).astype("<f4")
codebooks = faiss.vector_to_array(ivf.pq.centroids).astype("<f4")
code_size = ivf.pq.code_size
n = index.ntotal
ids_out = np.empty(n, "<u4")
assign_out = np.empty(n, "<u4")
codes_out = np.empty((n, M), "uint8")
invlists = ivf.invlists
pos = 0
for cluster in range(NLIST):
    ln = invlists.list_size(cluster)
    if ln == 0:
        continue
    fids = faiss.rev_swig_ptr(invlists.get_ids(cluster), ln).copy()
    fcodes = (
        faiss.rev_swig_ptr(invlists.get_codes(cluster), ln * code_size).reshape(ln, code_size).copy()
    )
    ids_out[pos : pos + ln] = fids.astype("<u4")
    assign_out[pos : pos + ln] = cluster
    codes_out[pos : pos + ln] = fcodes[:, :M]
    pos += ln

index = ivf = invlists = quant = opq_vt = None
gc.collect()
log(f"writing {OUT} ({pos:,} vectors)...")
stats = rr.write_rrvi_from_faiss(
    OUT, DIM, NLIST, M,
    centroids.tobytes(), codebooks.tobytes(),
    ids_out[:pos].tobytes(), assign_out[:pos].tobytes(), codes_out[:pos].tobytes(),
    nbits=8, metric="ip", opq=opq.tobytes(),
)
log(f"done: {stats}")
