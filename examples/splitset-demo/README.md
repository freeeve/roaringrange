# Standalone split-set (`RRSS`) demo

A no-backend browser demo of the split-set reader: tiered pruning, term-Bloom pruning, and
facet filtering over many small immutable `RRS` splits, served as static files over HTTP Range.
It doubles as a **local test harness** for the split-set wasm path before building/deploying a
full-size corpus.

## Build + serve

```sh
# 1. Generate the sample artifacts (split set + per-split facet sidecars + record store):
cd ../../rust
cargo run --release --features splits --example splitset_demo_data   # -> ../examples/splitset-demo/data/

# 2. (Re)build the wasm reader WITH the splits feature and copy it in:
wasm-pack build --target web --out-dir /tmp/rrwasm --features "wasm splits"
cp /tmp/rrwasm/roaringrange.js /tmp/rrwasm/roaringrange_bg.wasm ../examples/splitset-demo/

# 3. Serve (any static server that supports HTTP Range — Python's does):
cd ../examples/splitset-demo
python3 -m http.server 8080
# open http://localhost:8080/
```

## What it shows

- **`RrssIndex.open(manifestUrl, baseUrl)`** boots the `.rrss` manifest in two ranged reads;
  per-split `.rrs`/`.rrf` files are fetched (range) from `baseUrl/<name>` on demand.
- **`searchFiltered(query, limit, filters)`** runs the tiered short-circuit, skips splits via
  the term Bloom and the facet-presence summary (no fetch for a pruned split), and resolves the
  facet filter against each surviving split's own `RRSF` sidecar. `filters` is an array of
  `[field, category]` pairs (within a field categories OR, across fields AND).
- Records (doc id → title/field/year JSON) come from a matching `RRSR` store via
  `RrsRecords.getMany(ids)`.

The data is `splitset_demo_data.rs`'s 400 synthetic papers, byte-capped into a few tiers so the
pruning is visible. For the real thing — a full-size OpenAlex split set compared side-by-side
with the monolith — see the `Split set` mode in `examples/openalex`.
