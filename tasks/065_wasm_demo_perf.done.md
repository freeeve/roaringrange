# 065: perf(wasm/demo): skip wasted facet-count waves; parallelize boot; misc wasm-boundary costs

**Severity: MED.** Line refs @ 849f9c2.

## Findings

1. **MED -- `wasm.rs:433-443` (`filtered_ids`): always pays the full facet-count fetch wave, even when the demo discards it.** When a sidecar is open it unconditionally runs `counts_full(..., 64)`: up to 64 categories x 5 fields ~= 320 concurrent `count_category` calls (head fetch under meta-only boot and/or tail posting-subset fetches). The demo calls `facetFilter` on EVERY ranked-mode search -- including with zero active filters -- and at `index.html:2117` throws the counts away whenever `!facetHeadsReady` (`applyFacetCounts(facetHeadsReady ? fr.facetCounts() : null)`). During the post-boot head-streaming window every query pays hundreds of range GETs for nothing. Fix: an opt-out flag (or `top_per_field: 0` semantics) on `filtered_ids`; demo passes it when counts won't be shown and when no filters are active.
2. **LOW-MED -- demo boot serializes 5 cold opens on the bundle-miss path** (`index.html:2530-2582`). RrhcBundle -> index -> facets -> records(+dict) -> lookup awaited strictly in sequence (for per-phase perf attribution); the dict fetch (:2563) especially can run concurrently with earlier opens. Keep per-phase timing if wanted (Promise.all + per-promise timers).
3. **LOW -- `wasm.rs:1204-1206` (`WasmBitmap::page`): O(offset) per page** -- `iter().skip(offset).take(limit)` re-walks from the start; deep-paging a multi-million-doc bitmap re-iterates everything before the page each call. Use `RoaringBitmap::select`/rank for O(log n). (Same pattern in the Lambda at `main.rs:368`, harmless there at <=500 items.)
4. **LOW -- per-access Vec clones crossing the wasm boundary** (`wasm.rs:705, 713, 1620, 1626, 1733, 1739`): `FilteredIds::ids`, `RrviHits::ids/scores`, `RrtHits::ids/scores` getters clone the full Vec (and `facet_counts()` clones the JsValue) on EVERY property access; a JS loop touching `hits.ids` repeatedly silently re-copies. Convert to consuming methods or document "read once, cache in JS".
5. **LOW -- `wasm.rs:657, 771` (`counts_for`): counts truncated `as u32`** (wraps >4.29B) -- saturate like `WasmBitmap::len` (:1192) does. Theoretical at current corpus size; one-line.

## Acceptance

- rangeCacheStats/fetch-count comparison: a ranked query with no active filters during head-streaming issues ~0 facet-count fetches after the fix.
- Boot waterfall (devtools) shows overlapped opens on the fallback path.
- wasm-pack rebuild + deploy (remember: stale web/roaringrange.js symptom from memory), demo verified live.

## Outcome (DONE)

Deployed live: reader `roaringrange.13da5a419c.js` (+ `_bg.wasm`) at
https://openalex.evefreeman.com/ via `deploy.sh` (wasm-pack `--features "wasm zstd vector terms
splits hotcache"`), CloudFront invalidated. Changes in `rust/src/wasm.rs` +
`examples/openalex/web/index.html`.

- **Finding 1 (skip wasted facet-count waves):** `filtered_ids` gained a `want_counts` flag; the
  two `filterIds` bindings thread it. The demo passes `facetHeadsReady`, so during the post-boot
  head-streaming window (when the counts are discarded at `index.html`) the `counts_full` fetch
  wave (up to 64 x 5 category reads) is skipped entirely; filtering still runs. Confirmed the
  deployed glue exposes the 3-arg `filterIds` and the HTML calls it with `facetHeadsReady`.
- **Finding 2 (parallelize boot):** the zstd record-dictionary download now starts concurrently
  with the index/facet opens on the bundle-miss boot path (the live default -- there is currently
  no `.rrhc` object, so the demo boots via the fallback path, which this directly speeds up).
- **Finding 3 (O(log n) paging):** `WasmBitmap::page` uses rank/`select` instead of
  `iter().skip(offset)`, so deep-paging a multi-million-doc bitmap no longer re-walks the prefix.
- **Finding 4 (getter clones):** `FilteredIds`/`RrviHits`/`RrtHits` documented as read-once
  (each getter copies across the wasm boundary per access).
- **Finding 5 (counts_for truncation):** both `countsFor` bindings saturate with
  `u32::try_from(..).unwrap_or(u32::MAX)` instead of `as u32`.

Verified: full-feature wasm compiles; deployed reader assets serve 200 with
`application/wasm` / `text/javascript`; live HTML references the new hashed reader and the 3-arg
`filterIds` call. The interactive acceptance checks (devtools `rangeCacheStats` fetch-count with
no active filters; boot-waterfall overlap) are browser-manual and left for eyeballing on the live
demo -- the code paths that produce them are deployed and verified by inspection.
