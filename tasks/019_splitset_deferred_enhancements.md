# 019 — Split-set (`RRSS`) deferred enhancements

**Status:** pending (optional; spun off from task 007)

Task 007 is done — the split-set index (format, byte-capped builder, pruning reader,
base+delta lifecycle, Go conformance, wasm `RrssIndex`, Python bindings, dual trigram/term
bodies) is shipped, and the demo fold-in landed via task 015. These items were explicitly
deferred in 007 step 8 / `SPLITSET.md` and are not regressions — they are future-quality
additions. None block any consumer.

## Deferred items

1. **Time min/max summary (TLV tag 3).** No `SUMMARY_TAG_TIME` exists yet; `SPLITSET.md`
   marks the blob "reserved". Add a per-split time-range summary so date-bounded queries can
   prune splits without a fetch (companion to the existing facet-presence tag 2).
2. **Term-Bloom on RRTI-bodied splits (summary tag 1).** Trigram-bodied splits carry a
   Bloom summary for n-gram pruning; term-bodied splits write only tag 2 today
   (`splitset_build.rs` — "Term Bloom summaries are deferred"). Add a term Bloom so
   term-bodied splits prune on a missing query term without a fetch.
3. **Facet-filtered search on term-bodied splits.** `splitset.rs` currently returns
   `Unsupported("facet-filtered search is not yet supported on term-bodied splits")`.
   Implement the facet-aware path for term bodies (the trigram path already exists).
4. **Go RRTI term-bodied split builder.** Go builds the split set + per-split `RRSF` facet
   sidecars but not RRTI term bodies (`README.md` support-matrix note). This is a larger
   port (needs the Go RRTI `.rrt` writer — overlaps task 011's Go term-index wiring).

## Acceptance

Each item is independent and take-or-leave. Any that ships keeps `RRSS` manifest + split
output byte-identical across Rust/Go where both emit it (extend the existing golden
conformance).
