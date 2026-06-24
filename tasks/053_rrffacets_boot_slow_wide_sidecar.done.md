# Task 053 — `RrfFacets` boot/`facets()` slow on a wide sidecar (~191 range reads → ~15–20 s) — DONE

## Resolution (2026-06-24, meta-only boot)

`RrfFacets::open` now boots **meta-only** (`FacetIndex::open_meta` instead of `open`): it reads
just the header + field/category tables + string blob, so `facets()` (names + full-corpus
counts) is ready in **2 ranged reads instead of ~224** (measured on the live DeepLibby
`search.rrf`, 108,126 categories, via `examples/bench_facet_counts.rs`). The head/tail postings
are no longer eagerly loaded — `filterIds` (`membership_bitmap`), `facetCounts` (`counts_full`),
and `countsFor` already range-fetch exactly the postings they touch (task 052), so a non-resident
head is just fetched on demand for the priced categories.

One required follow-on: `counts_full`'s top-N rank key moved from the resident **head count** (now
all-zero on a meta-only boot) to the **full-corpus count** (the meta — always present, and what a
facet UI shows). The displayed top categories get exact head+tail counts; the long tail stays
head-only (zero on the meta-only path — not displayed; reachable exactly via `countsFor`).

Untouched: `RrsIndex`/`RrsCatalog` facet opens still load heads (their search *cursor* uses the
fetch-free `counts()` over a query's head). Cost moved off boot: the first `filterIds` now also
fetches the priced heads (~541 → ~812 reads on the bench — bounded, concurrent, cached).

## Symptom

The DeepLibby browse facet panel takes **~15–20 s to appear**. Opening the facet sidecar and
reading the global facet list does **~191 range fetches** of `search.rrf` (the sidecar has
**108,126 categories**) before the list is ready. The page's *results* render fast; only the
facet sidebar lags. Measured headless (Chrome) on `dev.deeplibby.com`: 191 `search.rrf`
responses, global facets still not loaded at 15 s.

## Context

`RrfFacets.facets()` is the **global, unfiltered** facet list — DeepLibby uses it for the browse
facet dropdowns. The shell only **displays the top ~10 per field** (plus a type-to-filter box),
so for *display* it needs only category **names + counts** (the meta), NOT the head/tail
postings (those are for `filterIds`).

## Hypothesis

`facet.rs` boot reads the compact meta region (header + field table + category table + string
blob) **plus** the per-field top-N category **head postings** (the `// Number of highest-count
category heads loaded per field for a large sidecar` cap, ~`facet.rs:54-56`). On a wide sidecar
(108k categories) those top-N head postings are many scattered reads — ~191 total. But the
`facets()` *display* path needs only the meta; the head postings are only consumed by
`filterIds`. So boot eagerly fetches postings the facet-list display never uses.

## Proposed fix

Either (preferred) **defer the head-postings load to the first `filterIds`/filter call** so
`RrfFacets.open` reads only the meta region (a few contiguous ranges) and `facets()` is
near-instant; filtering then loads postings on demand (already the hot path). Or **batch the
boot reads into fewer, larger ranges** (the meta is contiguous; the per-field heads could be one
coalesced read). Same spirit as the task-052 `counts_full` top-N cap: don't fetch what isn't
shown.

## Impact / current state

DeepLibby's browse facet sidebar is ~15–20 s to appear and the shell can't pre-warm it cheaply
(`facets()` is all-or-nothing). A meta-only boot would make it near-instant. The shell shows
top-10/field + type-to-filter, so meta-only display is sufficient; the type-to-filter long tail
already goes through `countsFor` on demand.

## Repro

```js
// headless Chrome, fresh load of the browse page:
//   facet/ref responses: { "manifest.json":1, "search.rrf":191, "reference.json":1 }
//   global facets not loaded at 15 s; results render in <1 s
```

## Where to look

- `rust/src/wasm.rs:524` `RrfFacets::facets()`; `:620` `filter_ids` (the posting consumer).
- `rust/src/facet.rs` boot (meta region + head-postings load) and `:54-56` the per-field head cap.

## Acceptance

- `RrfFacets.open` + `facets()` over the 108k-category DeepLibby sidecar completes in a small,
  bounded number of range reads (target: single-digit / low-tens, not ~191), no functional change
  to `filterIds`/`facetCounts`/`countsFor`.
