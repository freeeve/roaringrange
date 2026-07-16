# roaringrange

The core Rust crate behind [roaringrange](../README.md): static, range-fetchable
full-text search on roaring bitmaps. One crate, three roles:

- **build writers** ([`build`](src/build.rs)) — write the on-disk formats from
  split postings: `write_index` (`RRS`), `write_facets` (`RRSF`), `write_records`
  / `RecordWriter` (`RRSR`), plus `build::chunk` for a corpus whose index exceeds
  RAM. Output is byte-identical to the Go builder in [`../go`](../go).
- **reader** — [`Index`](src/index.rs), [`FacetIndex`](src/facet.rs),
  [`RecordStore`](src/records.rs), and the [`Catalog`](src/catalog.rs) facade that
  bundles all three; each answers a query with a few small ranged reads.
- **WASM bindings** (the `wasm` feature, [`wasm.rs`](src/wasm.rs)) — `RrsIndex` /
  `RrsCatalog` / `RrsRecords` for the browser, built with `wasm-pack`. With the
  `vector` feature added, also `RrviIndex` (similarity search).
- **vector search** (the `vector` feature, off by default) — a range-fetchable
  IVFPQ **similarity** index: the pure-Rust reader [`VectorIndex`](src/vector.rs)
  (boot → `nprobe` nearest clusters → asymmetric-distance scan → top-k), its
  browser binding `RrviIndex` (`wasm-pack build --features "wasm vector"`), a
  native dependency-free IVFPQ trainer [`build_ivfpq`](src/vector_build.rs), and a
  FAISS-export path [`build_ivfpq_from_parts`](src/vector_build.rs). Adds no
  third-party dependencies. See [`../VECTORS.md`](../VECTORS.md).

See [`../FORMAT.md`](../FORMAT.md), [`../FACETS.md`](../FACETS.md), and
[`../RECORDS.md`](../RECORDS.md) for the frozen on-disk specs (and
[`../VECTORS.md`](../VECTORS.md) for the `RRVI` vector index).

## How a query reads

Boot downloads the 16-byte header plus the sparse index once (tens of KB) and
keeps the sparse keys in memory. Each query then does, per n-gram: an in-memory
sparse binary search → one ranged dictionary-block read → binary search within
the block → one ranged posting read. Postings are portable RoaringBitmaps
(`RoaringBitmap::deserialize_from`), byte-identical to the Go builder's output.
Doc IDs are assigned in descending static rank, so each posting splits into a
**head** (the top 65,536) and a **tail**: a query ANDs the small heads for the
ranked top-K and fetches a tail only when paging past it.

## Transport behind a trait

All byte access goes through the [`RangeFetch`](src/fetch.rs) trait:

```rust
pub trait RangeFetch {
    fn read(&self, offset: u64, len: usize)
        -> impl std::future::Future<Output = Result<Vec<u8>, FetchError>>;
}
```

`Index` / `FacetIndex` / `RecordStore` / `Catalog` are generic over `RangeFetch`,
so the same core serves native callers (the in-memory [`MemoryFetch`](src/fetch.rs),
used by the tests) and the browser (a `fetch()`-backed impl behind the `wasm`
feature) with no core changes between the two. Reads are issued in concurrent
waves, so a query costs a near-constant number of round-trips regardless of how
many n-grams it has.

## Browser fetch layer & high-RTT knobs

The wasm reader schedules its own network reads rather than dumping them on the
browser's per-origin queue:

- every ranged read carries `priority: "high"`, deduplicates against identical
  in-flight reads (singleflight), and passes a bounded FIFO **fetch window**
  (default 8; `setFetchWindow(n)`, `0` = unbounded; `fetchWindowStats()` returns
  `[limit, active, parked]`);
- a shared LRU **range cache** memoizes completed reads (`setRangeCacheMb`,
  `rangeCacheStats`).

To diagnose a slow facet price, `setFacetTrace(true)` records the pricing
**wave structure** and `facetTrace()` drains it: an `Array<{wave, reads, bytes,
targets}>` with one `A`/`B`/`C` triple per contributing split. The waves are two
dependent round trips (A = head + tail-header prefixes, then C = the needed tail
containers; B is a rare header re-read), all splits concurrent — *not* a
per-category serial chain. Wave A dominates: it issues ~one scattered
tail-header read per priced category per split, so its read count scales with
`topPerField × fields × contributing splits`, capped by the fetch window rather
than reduced by it. If the traced reads land quickly but the panel updates
slowly, the delay is downstream of the fetch (dwell/render), not the waves.

Per-store/CDN tuning, all optional:

- `RrsRecords.setCoalesceGap(bytes)` widens `getMany`'s wave-merge gap (fewer,
  larger reads on high-RTT origins); `preloadIdxPrefix(n)` / `preloadIdx()` /
  `setResidentIdx(bytes)` make the offset table resident so page hydrations skip
  the `.idx` wave entirely (doc id == rank, so a prefix covers the hot pages).
- `RrssIndex.facetCounts(ids, topPerField?)` prices only what a facet panel
  renders; `countsFor(ids, pairs)` prices expanded long-tail categories exactly.
  Split sets built with `with_facet_digest(k)` / Go `SetFacetDigest(k)` carry a
  per-split **facet digest** in the manifest, so pricing reads no sidecar meta
  at all (see `SPLITSET.md`, summary tag 3).
- `RrssIndex.openBundle(manifest, base, rrhc)` boots every split — trigram
  **and** term (`RRTI`) — from one inlined-boot bundle emitted by
  `write_splitset_bundle` / Go `WriteSplitsetBundle`, collapsing per-split
  cold-open round trips into a single GET.

## Build & test

Native (host target) — the `wasm-bindgen` deps are optional, so it builds and
tests as-is:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
# vector (similarity) search is behind a non-default feature:
cargo test  --features vector
cargo clippy --all-targets --features vector -- -D warnings
```

Browser bundle:

```sh
wasm-pack build --target web --features wasm
# → pkg/roaringrange.js + roaringrange_bg.wasm
```

The [top-level README](../README.md) has the end-to-end quick start (build an
index in Rust or Go, read it in the browser); [`../python`](../python) wraps the
build writers for Python.
