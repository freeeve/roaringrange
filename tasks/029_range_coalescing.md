# 029 — Range coalescing in the fetch layer

**Status:** pending

A cold query issues several small ranged GETs whose offsets are often near one another
(dict block + neighboring dict block; several facet category heads; a page's record offset
pairs). S3 has no multi-range GET, but adjacent/near-adjacent ranges can be merged into one
request with bounded over-read.

## Design

1. A batching layer over `RangeFetch`: collect the ranges of one logical operation (the
   reader already issues them in `join_all` waves — e.g. `records.rs::get_many`,
   `facet.rs` head loads, `ResolvedFilter::combine`), sort by offset, merge ranges whose
   gap ≤ `coalesce_gap` (e.g. 64 KB) into one fetch, slice the results back out.
2. Over-read goes into the existing shared range cache, so it's not wasted — neighboring
   future reads hit it.
3. Tunables: `coalesce_gap`, max merged size (don't build 100 MB requests). Off for
   `MemoryFetch`/`FileFetch` (no per-request cost worth saving).

## Where it pays

- A 25-record page: 25 offset-pair reads (16 B each, often adjacent in the .idx) +
  25 blob slices (rank-clustered → near-adjacent) — can drop from ~50 requests to ~5.
- Facet boot top-N heads per field — one wave, often one merged read per field.

## Acceptance

- Perf bar request counts drop materially (target: records fetch ≤ 8 reqs for a page)
  with bytes growing ≤ 15%.
- Differential test: coalesced reads return byte-identical slices.
