# roaringrange (Python)

Build **static, range-fetchable full-text search datasets** from Python, then
search millions of records **in the browser with no backend**. These bindings
wrap the core Rust `build` module, so the files they emit are byte-identical to
the Go and Rust builders and are read by the same WASM reader.

## What it produces

A `Builder.build(out_dir)` writes the four files the reader serves over HTTP
Range:

| file | format | contents |
|---|---|---|
| `index.rrs`  | `RRSI` | trigram text index (popularity-split postings) |
| `index.rrf`  | `RRSF` | facet sidecar (field → category → doc-ID bitmap, with counts) |
| `records.idx` / `records.bin` | `RRSR` | per-doc record bytes (your encoding) |

Upload them to S3/CloudFront and point the [WASM reader](../rust) at the URLs.

## Install (dev)

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
