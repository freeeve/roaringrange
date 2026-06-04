"""Build a model2vec semantic `RRVI` over the WHOLE corpus by streaming, so the
~990 GB of f32 vectors at 484M never materialize. Pipes the record store through
`dump_records` → model2vec → FAISS `OPQ,IVF,PQ` in batches (`add_with_ids`, so a
record that fails to parse is simply skipped without shifting doc IDs), then
extracts the trained parts and writes the `.rrvi`.

    python build_full_rrvi.py <N> <out.rrvi> [nlist] [m]

N is the number of docs (e.g. 484369476, or a small value to test). doc_id order
is the dump order (rank order), shared with the text index + record store.
"""
from __future__ import annotations

import json
import subprocess
import sys
import time

import faiss
import numpy as np
import roaringrange as rr
from model2vec import StaticModel

N = int(sys.argv[1])
OUT = sys.argv[2]
NLIST = int(sys.argv[3]) if len(sys.argv) > 3 else 16384
M = int(sys.argv[4]) if len(sys.argv) > 4 else 32
DIM, BATCH, TRAIN_SAMPLE = 512, 200_000, 2_000_000
DUMP = "rust/target/release/examples/dump_records"
IDX, BIN, DICT = (
    "/tmp/oa-out/records-full.idx",
    "/tmp/oa-out/records-full.bin",
    "/tmp/oa-out/openalex-full.dict",
)

t0 = time.time()


def log(msg):
    print(f"[{time.time() - t0:7.0f}s] {msg}", flush=True)


model = StaticModel.from_pretrained("minishlab/potion-retrieval-32M")
index = faiss.index_factory(DIM, f"OPQ{M},IVF{NLIST},PQ{M}", faiss.METRIC_L2)

proc = subprocess.Popen([DUMP, IDX, BIN, DICT, str(N)], stdout=subprocess.PIPE, bufsize=1 << 22)


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
            emb = np.asarray(model.encode(texts), dtype="float32")
            faiss.normalize_L2(emb)
            yield np.asarray(ids, dtype="int64"), emb
            ids, texts = [], []
    if ids:
        emb = np.asarray(model.encode(texts), dtype="float32")
        faiss.normalize_L2(emb)
        yield np.asarray(ids, dtype="int64"), emb


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

# Phase 2: add the buffered batches, then stream the rest. add_with_ids keeps the
# doc_id explicit, so a skipped (unparseable) record never shifts ids.
added = 0
for bids, bvecs in buf:
    index.add_with_ids(bvecs, bids)
    added += len(bids)
buf = None
for bids, bvecs in gen:
    index.add_with_ids(bvecs, bids)
    added += len(bids)
    if added % 5_000_000 < BATCH:
        log(f"added {added:,}")
log(f"added all {added:,}; extracting trained parts...")

# Extract OPQ rotation + centroids + PQ codebooks + per-vector (id, cluster, code).
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
    fids = faiss.rev_swig_ptr(invlists.get_ids(cluster), ln).copy()  # doc IDs (add_with_ids)
    fcodes = (
        faiss.rev_swig_ptr(invlists.get_codes(cluster), ln * code_size)
        .reshape(ln, code_size)
        .copy()
    )
    ids_out[pos : pos + ln] = fids.astype("<u4")
    assign_out[pos : pos + ln] = cluster
    codes_out[pos : pos + ln] = fcodes[:, :M]
    pos += ln
# Free the FAISS index (~19 GB of codes+ids) before the big write doubles the
# extracted arrays into byte buffers — keeps the peak well under box RAM.
import gc

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
