"""Shared streaming `RRVI` builder: record store → embed → FAISS `OPQ,IVF,PQ` → `.rrvi`,
without ever materializing the full vector set (~990 GB of f32 at 484M docs).

`build_rrvi_streaming` pipes the record store through the `dump_records` example,
batches `"<title> <abstract>"` texts through a caller-supplied `embed(texts)`
callable (which must return unit-norm float32 `(len(texts), dim)` — the metric-ip
contract), trains on the first `train_sample` vectors, streams the rest in with
`add_with_ids` (a record that fails to parse is skipped without shifting doc IDs),
then extracts the trained parts via `faiss_to_rrvi.extract_ivfpq` and writes the
`.rrvi`. The embedder is the ONLY thing that varies between the model2vec and
Gemma builds — see `build_full_rrvi.py` / `build_full_rrvi_gemma.py`.

doc_id order is the dump order (rank order), shared with the text index + record
store.
"""
from __future__ import annotations

import gc
import json
import subprocess
import sys
import time
from typing import Callable

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from faiss_to_rrvi import extract_ivfpq  # noqa: E402

DUMP = "rust/target/release/examples/dump_records"
IDX, BIN, DICT = (
    "/tmp/oa-out/records-full.idx",
    "/tmp/oa-out/records-full.bin",
    "/tmp/oa-out/openalex-full.dict",
)


def build_rrvi_streaming(
    n: int,
    out_path: str,
    embed: Callable[[list[str]], np.ndarray],
    *,
    nlist: int = 16384,
    m: int = 32,
    dim: int = 512,
    batch: int = 200_000,
    train_sample: int = 2_000_000,
    log_every: int = 5_000_000,
):
    """Streams `n` records into a trained `OPQ{m},IVF{nlist},PQ{m}` and writes
    `out_path`. `embed(texts)` returns unit-norm float32 `(len(texts), dim)`.
    `batch` is the doc count per embed call; `log_every` the added-progress
    interval. Returns the binding's `VectorBuildStats`.
    """
    import faiss  # deferred: the caller controls env/import order (torch before faiss)
    import roaringrange as rr

    t0 = time.time()

    def log(msg):
        print(f"[{time.time() - t0:7.0f}s] {msg}", flush=True)

    index = faiss.index_factory(dim, f"OPQ{m},IVF{nlist},PQ{m}", faiss.METRIC_L2)
    proc = subprocess.Popen(
        [DUMP, IDX, BIN, DICT, str(n)], stdout=subprocess.PIPE, bufsize=1 << 22
    )

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
            if len(ids) >= batch:
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
        if ntrain >= train_sample:
            break
    log(f"training OPQ{m},IVF{nlist},PQ{m} on {ntrain:,} sampled vectors...")
    index.train(np.concatenate([v for _, v in buf]))
    log("trained; adding all vectors (streaming)...")

    # Phase 2: add the buffered batches, then stream the rest. add_with_ids keeps
    # the doc_id explicit, so a skipped (unparseable) record never shifts ids.
    added = 0
    for bids, bvecs in buf:
        index.add_with_ids(bvecs, bids)
        added += len(bids)
    buf = None
    for bids, bvecs in gen:
        index.add_with_ids(bvecs, bids)
        added += len(bids)
        if added % log_every < batch:
            log(f"added {added:,}")
    log(f"added all {added:,}; extracting trained parts...")

    # FAISS ids ARE the doc ids (add_with_ids), so no row→doc mapping.
    opq, centroids, codebooks, ids_out, assign_out, codes_out, pos = extract_ivfpq(
        index, nlist, m
    )
    # Free the FAISS index (~19 GB of codes+ids at 484M) before the write doubles
    # the extracted arrays into byte buffers — keeps the peak well under box RAM.
    index = None
    gc.collect()
    log(f"writing {out_path} ({pos:,} vectors)...")
    stats = rr.write_rrvi_from_faiss(
        out_path, dim, nlist, m,
        centroids.tobytes(), codebooks.tobytes(),
        ids_out[:pos].tobytes(), assign_out[:pos].tobytes(), codes_out[:pos].tobytes(),
        nbits=8, metric="ip", opq=opq.tobytes(),
    )
    log(f"done: {stats}")
    return stats
