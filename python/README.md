# roaringrange (Python)

Build **static, range-fetchable search datasets** from Python, then search
millions of records **in the browser with no backend**. These bindings wrap the
core Rust `build` module, so the files they emit are byte-identical to the Go and
Rust builders and are read by the same WASM reader. Two index types: a **trigram
text index** (`Builder`) and a **similarity / vector index** (`VectorBuilder`).

## What it produces

`Builder.build(out_dir)` writes the four files the text reader serves over HTTP
Range; `VectorBuilder.build(path)` writes one `.rrvi` similarity index:

| file | format | contents |
|---|---|---|
| `index.rrs`  | `RRSI` | trigram text index (popularity-split postings) |
| `index.rrf`  | `RRSF` | facet sidecar (field → category → doc-ID bitmap, with counts) |
| `records.idx` / `records.bin` | `RRSR` | per-doc record bytes (your encoding) |
| `*.rrvi` | `RRVI` | IVFPQ similarity index (range-fetched coarse clusters + PQ codes) |

Upload them to S3/CloudFront and point the [WASM reader](../rust) at the URLs.

## Install

Prebuilt **abi3 wheels** (one wheel for CPython 3.8+) are published to PyPI:

```sh
pip install roaringrange
```

CI builds and tests the extension on **CPython 3.12, 3.13, and 3.14**.

### From source (dev)

```sh
cd python
maturin develop --release      # builds + installs into the active venv
# or: maturin build --release   # produces a wheel in target/wheels/
```

Requires a Rust toolchain and `pip install maturin`.

## Usage

```python
import roaringrange as rr, json

b = rr.Builder(gram_size=3)
for row in rows:                              # rows from a DataFrame, DB, JSONL, …
    b.add(
        rank=row["citations"],                # higher rank = listed first (doc-ID order)
        text=f'{row["title"]} {row["abstract"]}',   # tokenized into trigram keys
        record=json.dumps({"t": row["title"], "y": row["year"]}).encode(),
        facets={"year": [str(row["year"])], "type": [row["type"]]},  # field → categories
    )

stats = b.build("out/")        # writes out/index.rrs, index.rrf, records.idx, records.bin
print(stats)                   # BuildStats(docs=..., ngrams=..., fields=...)
```

`rr.tokenize(text, gram_size=3)` returns the n-gram keys a string maps to — useful
for understanding why a query does or doesn't match.

## Vector / similarity search

`VectorBuilder` trains an IVFPQ index over your embeddings and writes a single
`.rrvi` file that the WASM reader range-fetches like the text index. Use the
**same `doc_id`** as the text index so a vector hit maps to the same record (and
can hybridize with trigram search). Vectors are L2-normalized for the default
`"ip"` (cosine) metric.

```python
import roaringrange as rr

vb = rr.VectorBuilder(dim=256, nlist=4096, m=32, metric="ip")  # m must divide dim
for doc_id, embedding in enumerate(embeddings):     # embeddings: any float sequences
    vb.add(doc_id, embedding.tolist())              # numpy row → list of floats
# or in one call: vb.add_many([(i, e.tolist()) for i, e in enumerate(embeddings)])

stats = vb.build("out/vectors.rrvi")
print(stats)   # VectorBuildStats(vectors=..., dim=256, nlist=..., m=32, nbits=8)
```

Parameters: `nlist` coarse clusters (≈ `4·√N`, clamped to the vector count),
`m` PQ subquantizers (must divide `dim`), `nbits` (1–8) → `2^nbits` codes per
subspace, `metric` `"ip"`/`"cosine"` or `"l2"`. Training is deterministic
(`seed`, `kmeans_iters`). One `.rrvi` per embedding model — each model is a
different vector space. See [`../VECTORS.md`](../VECTORS.md) for the byte layout.

This pure-Rust trainer suits small/medium corpora and tests; at very large scale
train with FAISS and export the same `RRVI` layout (the reader is identical).

### Scale: train with FAISS, export to RRVI

For large corpora, train `OPQ,IVF,PQ` with FAISS and export the trained parts —
no retraining in Rust. `python/scripts/faiss_to_rrvi.py` does this end to end
(install the extra: `pip install 'roaringrange[train]'` for numpy + faiss-cpu):

```python
from faiss_to_rrvi import export_to_rrvi
stats = export_to_rrvi(vectors, doc_ids, "vectors.rrvi", nlist=4096, m=32, metric="ip")
```

Under the hood it calls the low-level `roaringrange.write_rrvi_from_faiss(...)`,
which takes the FAISS arrays (OPQ rotation, coarse centroids, PQ codebooks,
per-vector cluster + 8-bit codes) as little-endian byte buffers — so the wheel
needs no numpy dependency. The export is verified against the Rust reader
(recall@10 ≈ 0.9995 vs FAISS's own search on the same index).

### Embedding text (mode 2: model2vec, no backend)

`python/scripts/model2vec_embed.py` embeds text with a model2vec **static** model
(`minishlab/potion-retrieval-32M`, 512-d, mean-pooled token vectors — no
transformer, fast on CPU) and builds a `.rrvi`. Install the extra:
`pip install 'roaringrange[embed]'`.

```python
from model2vec_embed import build_rrvi_from_texts
stats, _ = build_rrvi_from_texts(titles, doc_ids, "vectors.rrvi", nlist=256, m=32)
```

It's "mode 2" because the *same* model2vec recipe can run in the browser at query
time, so similarity search needs no backend at all. The query embedding **must**
use the identical model + pooling as the corpus, or the spaces won't match.

## Notes

- **Ranking is baked in.** Doc IDs are assigned in descending `rank`, so the
  top-K of any query is free at read time (no query-time scoring). Pick a good
  rank signal (citations, holdings, popularity, …).
- **Records are opaque.** `record=` is raw bytes; the format never dictates your
  schema. Decode them however you like on the client.
- **In-memory build.** This builds the whole index in RAM — ideal for up to many
  millions of records. For corpora whose index exceeds memory, the core crate's
  chunked path (`build::chunk`) is the route; exposing it here is a follow-up.

MIT — see [../LICENSE](../LICENSE).
