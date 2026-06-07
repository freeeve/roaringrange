# Standalone split-set (`RRSS`) demo

A no-backend browser demo of the split-set reader: tiered pruning, term-Bloom pruning, and
facet filtering over many small immutable `RRS` splits, served as static files over HTTP Range.
It doubles as a **local test harness** for the split-set wasm path before building/deploying a
full-size corpus.

## Build + serve

```sh
# 1. Generate the sample artifacts (split set + per-split facet sidecars + record store + the
#    .rrhc boot bundle):
cd ../../rust
cargo run --release --features "splits hotcache" --example splitset_demo_data   # -> ../examples/splitset-demo/data/

# 2. (Re)build the wasm reader WITH the splits + hotcache features and copy it in:
wasm-pack build --target web --out-dir /tmp/rrwasm --features "wasm splits hotcache"
cp /tmp/rrwasm/roaringrange.js /tmp/rrwasm/roaringrange_bg.wasm ../examples/splitset-demo/

# 3. Serve with a Range-aware server (the reader requires HTTP Range; Python's stock
#    `http.server` does NOT honor it, so use the bundled serve.py):
cd ../examples/splitset-demo
python3 serve.py 8080
# open http://localhost:8080/            (boots from the .rrhc bundle)
# open http://localhost:8080/?nobundle   (forces per-split cold opens, for comparison)
```

## What it shows

- **`RrssIndex.openBundle(manifestUrl, baseUrl, rrhcUrl)`** boots the `.rrss` manifest and the
  `.rrhc` boot bundle in one parallel wave, then opens each queried split from its **inlined
  boot** (no per-split header GET) — the N per-split opens collapse into the single bundle GET.
  The status line reports how many split boots came resident; `?nobundle` switches to the plain
  per-split path below for comparison.
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
