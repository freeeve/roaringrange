"""Train an IVFPQ index with FAISS and export it to the range-fetchable `RRVI`
format that the roaringrange reader serves over HTTP Range.

This is the production-scale build path (step 3 of the vector-search task): FAISS
trains `OPQ,IVF,PQ` and encodes every vector; this script extracts the trained
parts (OPQ rotation, coarse centroids, PQ codebooks, per-vector cluster + code)
and hands them to `roaringrange.write_rrvi_from_faiss`, which writes the `.rrvi`.
No retraining happens in Rust — the reader reads the FAISS-trained index directly.

Requires the optional `train` extra:  pip install 'roaringrange[train]'
(i.e. numpy + faiss-cpu). The roaringrange wheel itself has no such dependency.

Constraints / assumptions:
- 8-bit PQ codes (`PQ{m}` / `PQ{m}x8`) so each code is one byte per subquantizer,
  matching the RRVI on-disk layout. `m` must divide `dim`.
- Vectors are added with plain `add()`, so FAISS internal ids are row indices; we
  map them back to your `doc_ids`. `doc_id` must equal the text index's doc ID.
- For cosine/inner-product search, vectors are L2-normalized and FAISS is trained
  with METRIC_L2 (residual PQ is L2; the reader reports a cosine-like score). The
  query path must normalize queries the same way.
"""
from __future__ import annotations

import argparse

import numpy as np

import roaringrange as rr


def _require_faiss():
    try:
        import faiss  # noqa: F401

        return faiss
    except ImportError as e:  # pragma: no cover - environment dependent
        raise SystemExit(
            "faiss is required: pip install 'roaringrange[train]' (or faiss-cpu)"
        ) from e


def export_to_rrvi(
    vectors: np.ndarray,
    doc_ids: np.ndarray,
    out_path: str,
    nlist: int,
    m: int,
    metric: str = "ip",
    train_size: int | None = None,
    seed: int = 1234,
) -> rr.VectorBuildStats:
    """Trains `OPQ{m},IVF{nlist},PQ{m}` over `vectors` and writes `out_path`.

    `vectors` is `(n, dim)` float32; `doc_ids` is `(n,)` uint32 aligned to it.
    Returns the binding's `VectorBuildStats`.
    """
    faiss = _require_faiss()
    x = np.ascontiguousarray(vectors, dtype="float32")
    n, dim = x.shape
    if dim % m:
        raise ValueError(f"m={m} must divide dim={dim}")
    doc_ids = np.ascontiguousarray(doc_ids, dtype="uint32")
    if doc_ids.shape != (n,):
        raise ValueError("doc_ids must be shape (n,)")

    if metric in ("ip", "cosine", "inner_product", "dot"):
        faiss.normalize_L2(x)

    index = faiss.index_factory(dim, f"OPQ{m},IVF{nlist},PQ{m}", faiss.METRIC_L2)
    rng = np.random.default_rng(seed)
    if train_size is not None and train_size < n:
        sample = x[rng.choice(n, size=train_size, replace=False)]
    else:
        sample = x
    index.train(sample)
    index.add(x)

    # --- extract the trained parts -------------------------------------------
    # OPQ rotation: the first transform in the pre-transform chain. y = A·x.
    opq_vt = faiss.downcast_VectorTransform(index.chain.at(0))
    if opq_vt.d_in != dim or opq_vt.d_out != dim:
        raise ValueError("unexpected OPQ shape (need a square dim×dim rotation)")
    opq = faiss.vector_to_array(opq_vt.A).astype("<f4")  # d_out*d_in, row-major
    bias = faiss.vector_to_array(opq_vt.b)
    if bias.size and np.any(bias != 0):
        raise ValueError("OPQ has a non-zero bias, which RRVI does not model")

    # extract_index_ivf returns the IndexIVF base; downcast to reach .pq.
    ivf = faiss.downcast_index(faiss.extract_index_ivf(index))  # IndexIVFPQ
    if ivf.nlist != nlist:
        raise ValueError(f"trained nlist {ivf.nlist} != requested {nlist}")
    quantizer = faiss.downcast_index(ivf.quantizer)
    centroids = quantizer.reconstruct_n(0, nlist).reshape(nlist, dim).astype("<f4")

    pq = ivf.pq
    if pq.nbits != 8:
        raise ValueError("RRVI export requires 8-bit PQ codes")
    codebooks = faiss.vector_to_array(pq.centroids).astype("<f4")  # m*256*(dim/m)
    code_size = pq.code_size  # == m for 8-bit codes

    # Per-vector cluster + code, mapped from FAISS row ids back to doc ids.
    ids_out = np.empty(n, dtype="<u4")
    assign_out = np.empty(n, dtype="<u4")
    codes_out = np.empty((n, m), dtype="uint8")
    invlists = ivf.invlists
    pos = 0
    for cluster in range(nlist):
        ln = invlists.list_size(cluster)
        if ln == 0:
            continue
        fids = faiss.rev_swig_ptr(invlists.get_ids(cluster), ln).copy()
        fcodes = (
            faiss.rev_swig_ptr(invlists.get_codes(cluster), ln * code_size)
            .reshape(ln, code_size)
            .copy()
        )
        ids_out[pos : pos + ln] = doc_ids[fids]
        assign_out[pos : pos + ln] = cluster
        codes_out[pos : pos + ln] = fcodes[:, :m]
        pos += ln
    if pos != n:
        raise RuntimeError(f"recovered {pos} coded vectors, expected {n}")

    return rr.write_rrvi_from_faiss(
        out_path,
        dim,
        nlist,
        m,
        centroids.tobytes(),
        codebooks.tobytes(),
        ids_out.tobytes(),
        assign_out.tobytes(),
        codes_out.tobytes(),
        nbits=8,
        metric=metric,
        opq=opq.tobytes(),
    )


def report_recall(vectors: np.ndarray, nlist: int, m: int, metric: str, k: int = 10,
                  nprobe: int = 8, n_queries: int = 200, seed: int = 7) -> float:
    """Trains the same index and reports FAISS IVFPQ recall@k vs a flat baseline,
    a quick check that the index quality is acceptable before a full export."""
    faiss = _require_faiss()
    x = np.ascontiguousarray(vectors, dtype="float32")
    n, dim = x.shape
    if metric in ("ip", "cosine", "inner_product", "dot"):
        faiss.normalize_L2(x)
    rng = np.random.default_rng(seed)
    q = x[rng.choice(n, size=min(n_queries, n), replace=False)].copy()

    flat = faiss.IndexFlatL2(dim)
    flat.add(x)
    _, exact = flat.search(q, k)

    index = faiss.index_factory(dim, f"OPQ{m},IVF{nlist},PQ{m}", faiss.METRIC_L2)
    index.train(x)
    index.add(x)
    faiss.extract_index_ivf(index).nprobe = nprobe  # set on the IVF, not the pre-transform
    _, approx = index.search(q, k)

    hits = sum(len(set(a) & set(e)) for a, e in zip(approx, exact))
    return hits / (len(q) * k)


def _demo(args):
    """Synthetic end-to-end run so the script is exercisable without a corpus."""
    rng = np.random.default_rng(0)
    centers = rng.normal(scale=5.0, size=(args.nlist, args.dim)).astype("float32")
    blocks = [c + rng.normal(scale=0.5, size=(args.per, args.dim)) for c in centers]
    x = np.vstack(blocks).astype("float32")
    doc_ids = np.arange(x.shape[0], dtype="uint32")
    stats = export_to_rrvi(x, doc_ids, args.out, args.nlist, args.m, metric=args.metric)
    print("wrote", args.out, "->", stats)
    recall = report_recall(x, args.nlist, args.m, args.metric)
    print(f"FAISS IVFPQ recall@10 vs flat: {recall:.3f}")


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", default="vectors.rrvi")
    p.add_argument("--dim", type=int, default=64)
    p.add_argument("--m", type=int, default=16)
    p.add_argument("--nlist", type=int, default=64)
    p.add_argument("--per", type=int, default=200, help="points per synthetic blob")
    p.add_argument("--metric", default="ip")
    _demo(p.parse_args())
