# Task 052 — `FilteredIds.facet_counts()` under-counts vs `RrssIndex.facet_counts(ids)` — DONE

## Resolution (2026-06-24)

Root cause confirmed: `FacetIndex::counts()` (`rust/src/facet.rs`) intersects only each
category's **resident head posting** (docs `[0, 65536)`) — correct for the search path (a
query's head *is* its top results, fetch-free) but wrong for `FilteredIds` over an arbitrary
corpus-spanning id list, which also spans the tails. The split path is right because each split
is ≤65536 docs (head-sized), so summing heads across splits covers the corpus; the monolithic
`.rrf` sidecar has a real head+tail per category and the head-only count missed every tail doc
(per-category ratios 33–143×, not a uniform 50×, because each category's head/tail split differs).

Fix: added `FacetIndex::counts_full(result)` — async, counts each category over head **and** tail,
fetching the tail at **container granularity** (only the buckets `result` spans, via
`read_posting_subset`, like the filter path), and fetching a non-resident head if a large-sidecar
boot skipped it. `wasm.rs::filtered_ids` now calls `counts_full` instead of the head-only
`counts()`. The search-path `counts()` is unchanged (still fetch-free). Conformance test
`build_tests::filtered_counts_include_tail_not_just_head` (head+tail fixture) asserts
`counts_full` == the true intersection and that head-only `counts()` undercounts.

**Note:** this is a wasm-reader fix — consumers (DeepLibby shell, the OpenAlex demo) must rebuild
+ redeploy the wasm to pick it up.

## Perf follow-up (2026-06-24)

The first `counts_full` fetched every category's tail. On a **wide** sidecar that is ruinous:
the DeepLibby `.rrf` has **108,126 categories**, so a single drill-down did **216,068**
range-reads / 26.5 MB (~841 ms local; effectively a hang over HTTP). Fix: `counts_full` now
takes a `top_per_field` cap — it prices exactly (head+tail) only the top-N categories per field
(ranked by the free head-only count, the displayed ones), and the unshown long tail keeps its
head-only count. `wasm.rs::filtered_ids` passes `FACET_COUNTS_TOP_PER_FIELD = 64`;
`top_per_field == 0` still prices every category (the conformance test). Benchmark
(`examples/bench_facet_counts.rs`, run on the live DeepLibby sidecar): **216,068 reads → 541**
(~400×), 841 ms → 63 ms. Trade-off: the unshown long-tail categories are head-only/approximate
again — fine for a facet panel. For the exact count of a *specific* long-tail category a user
expands or searches, added `FacetIndex::counts_for(result, &pairs)` (wasm `countsFor(ids, pairs)`
on `RrfFacets`/`RrsIndex`) — exact head+tail counts for the named `[field, category]` pairs, ~one
tail fetch each, returned as a `Uint32Array`.

## Symptom

`RrfFacets.filterIds(ids, pairs)` returns the **correct** survivor `ids`, but the
`FilteredIds.facetCounts()` on that result reports per-category counts that are
**drastically low** — ~50× under in the repro below. The survivor set is right; the
facet histogram computed over it is wrong. `RrssIndex.facetCounts(ids)` over the
*identical* id set returns the correct counts.

## Repro

DeepLibby owned artifacts (`search.rrf` + `search.rrss`, 3,774,281 docs). Same id set,
two facet-count paths:

```js
import init, { RrfFacets, RrssIndex, setRangeCacheMb } from "roaringrange";
const BASE = "https://dev.deeplibby.com/artifacts"; // owned corpus, range-served
await init(/* …_bg.wasm */); setRangeCacheMb(64);

const fac = await RrfFacets.open(`${BASE}/search.rrf`);
const n = 3774281, allIds = new Uint32Array(n);
for (let i = 0; i < n; i++) allIds[i] = i;

const filtered = await fac.filterIds(allIds, [["language", "spanish"]]);
console.log(filtered.ids.length);                       // 186782  (correct survivor count)

// (A) the facet-sidecar path — WRONG
filtered.facetCounts().find(f => f.field === "subject").cats…   // fiction:88, juvenile fiction:75, nonfiction:40

// (B) the index path over the SAME ids — CORRECT
const idx = await RrssIndex.open(`${BASE}/search.rrss`, BASE);
(await idx.facetCounts(filtered.ids)).find(f => f.field === "subject").cats… // nonfiction:5737, fiction:4460, juvenile fiction:2472
```

| subject | `FilteredIds.facetCounts()` (A) | `RrssIndex.facetCounts(ids)` (B) |
|---|---|---|
| nonfiction | 40 | 5737 |
| fiction | 88 | 4460 |
| juvenile fiction | 75 | 2472 |

(B) is right: ~4,460 of 186,782 Spanish docs tagged `fiction`, consistent with ~44% merged-tag
coverage. (A) is ~1/50 of that across every category.

## Expected

`FilteredIds.facetCounts()` == `RrssIndex.facetCounts(filteredIds)` — the per-category counts
over the full survivor set.

## Where to look

- `rust/src/wasm.rs:674` — `FilteredIds::facet_counts()` (the wrong path).
- `rust/src/wasm.rs:620` — `…::filter_ids()` that builds `FilteredIds`.
- `rust/src/wasm.rs:274` — `facet_counts_js(facets, head: &RoaringBitmap)` — note it counts over a
  **`head`** bitmap; if `FilteredIds::facet_counts()` passes a head/first-split bitmap rather than
  the full survivor bitmap, that explains the undercount.
- Correct path for comparison: `rust/src/wasm.rs:2073` `facet_counts(ids)` → `rust/src/splitset.rs:737`
  `facet_counts(...)`, which **aggregates across splits** (see test
  `splitset.rs:2112 facet_counts_aggregate_across_splits_by_name`).

## Hypothesis

The undercount looks like the `FilteredIds` path counts over a single split / the head bitmap
(or otherwise fails to aggregate across all facet splits), while the index path sums across all
splits. The constant ~50× factor ≈ a per-split slice of the full posting set.

## Impact / current workaround

`filterIds().facetCounts()` is the natural API for **drill-down facets** — narrowing the other
facets to a filtered set with no text query (faceted browse). DeepLibby's shell worked around it
by opening the trigram index and calling `RrssIndex.facetCounts(survivorIds)` instead, which works
but means loading the trigram split-set purely to count facets over an already-filtered id list.
Fixing `FilteredIds::facet_counts()` lets consumers use the facet sidecar alone.

## Update — deeper: the two facet systems disagree GLOBALLY (the bigger bug)

Not just `FilteredIds` vs index. The **standalone RRSF sidecar (`search.rrf`, `RrfFacets`)**
and the **per-split embedded facet sidecars (`search.rrss`, `RrssIndex.facetCounts`)** report
**different global counts for the same value** — so a consumer can't make a facet *count* match
what *filtering* returns. Same owned corpus, global `subject=fiction`:

| source | global `fiction` |
|---|---|
| `RrfFacets.facets()` (drives `filterIds` filtering) | **776,545** |
| `RrssIndex.facetCounts(allIds)` | **144,679** |

…a ~5.4× disagreement at the corpus level, before any filtering. And the actual intersection:

- `filterIds([language=spanish]).length` = 186,782
- `filterIds([language=spanish, subject=fiction]).length` = **28,858**  ← what clicking "fiction" under Spanish actually returns
- `RrfFacets.filterIds([language=spanish]).facetCounts()` subject fiction = **88**
- `RrssIndex.facetCounts(spanishIds)` subject fiction = **4,460**

So **all three** count APIs disagree with the 28,858 the filter actually yields. Filtering uses
the `RrfFacets` bitmaps (consistent with 776,545 global), but no count API reports the
`RrfFacets`-consistent number over a subset.

### Likely cause

The RRSF sidecar and the split-embedded facet sidecars are built from / aggregate the
(multi-valued) subject set differently — `RrfFacets` looks complete (a doc tagged `fiction`
counts once under `fiction`), the split path looks like it counts a subset (single split / head,
or only one subject per doc). Reconcile so both, and `filterIds(S,[[f,v]]).length`, agree.

### Impact on DeepLibby

Drill-down facets (narrow the other facets to a filtered set) — the *values* shown are right
(clicking yields results, no dead-ends) but the *counts* can't be trusted. The shell currently
shows `idx.facetCounts` numbers, which under-state vs. the click result. Pending this fix the
shell may hide counts on filtered facets or drop to per-value `filterIds().length` (expensive).

## Acceptance

- For any id set S and facet value V (field f): `RrfFacets.filterIds(S,[[f,V]]).length` ==
  `FilteredIds.facetCounts()` over S at `[f][V]` == `RrssIndex.facetCounts(S)` at `[f][V]`.
- The global case (S = all docs) must agree across `RrfFacets.facets()` and `RrssIndex.facetCounts(allIds)`.
- Conformance test over a multi-split, multi-valued-facet fixture (mirroring
  `facet_counts_aggregate_across_splits_by_name`).
