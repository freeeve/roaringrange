# Task 015 — Fold the split-set into the main demo; remove splitset.html

**Status:** pending

The RRSS split-set is currently a **separate page** (`examples/openalex/web/splitset.html`,
"OpenAlex — monolith vs split set") that opens a monolith (`RrsIndex`) and a split set
(`RrssIndex`) side by side. Make the split set a **first-class option in the main demo**
(`index.html`) instead, and delete the standalone page.

The reader already supports it: `rust/src/splitset.rs` (`SplitSet::open` + `search` /
`search_filtered` with cross-split tier/base-delta/bloom pruning), exposed to the browser as
`RrssIndex` (`open(manifestUrl, baseUrl)`, `search`, `searchFiltered`, `splitCount`,
`deltaCount`, `totalBytes`) and built into the demo wasm via the `splits` feature. This task is
**demo wiring + cleanup only**, no reader change (unless facet counts are wanted — see below).

## Prerequisite (blocking the live demo, not the wiring)

No split set is deployed: `s3://openalex-eve/openalex-split/` is empty and
`openalex-split/openalex.rrss` is 403. A 484M split-set term build was in flight (see the
live-build memory); it must be uploaded with `./deploy.sh --splits <dir>` before the integrated
mode works live. The wiring can be built and unit-checked against a small local split set first.

## Design — split-set as a backend toggle in the main demo

Add a toggle next to **Server-side search** (the `.srvtoggle` pattern in `index.html`), e.g.
**"Split-set index"**, that swaps the **trigram** backend from the monolith `RrsIndex` to
`RrssIndex` for the active query. Flipping it reproduces splitset.html's monolith-vs-split
comparison on the same query, in one page. Key wiring points:

1. **Render path.** `RrssIndex.search`/`searchFiltered` return a **flat ranked `Vec<u32>`** (not a
   cursor). So route results through the existing **`rankedIds` path (`goPageRanked`)** built for
   semantic/term/hybrid — not the trigram cursor (`goPage`). Pick a ranked depth (like `SEM_K`)
   for `limit`. No incremental tail / `pendingTail` in this mode (the split reader returns a
   bounded ranked list).
2. **Records.** The split set ships its **own** record store (`openalex-split/openalex-records.{idx,bin}`).
   Doc IDs match the monolith **only when both are built over the same corpus** (both rank by
   `cited_by_count`; see the deploy.sh note). Safest: in split-set mode fetch records from the
   split set's own store. If the deployed split set is the same 484M corpus as `openalex-full`,
   `records-full` could be reused instead — decide based on how it's built/uploaded.
3. **Facets.** `RrssIndex.searchFiltered` takes `[field,category]` filters and the split carries
   per-split `.rrf` sidecars, so facet *filtering* works. But `RrssIndex` exposes **no facet-count
   method** today — the panel's counts would need either a new `RrssIndex.facetCounts()` (reader
   change) or to fall back to the monolith's counts (approximate) in split-set mode. Decide; note
   if a small reader addition is in scope.
4. **Perf panel.** Surface the split-set pruning story the old page showed: `splitCount` /
   `deltaCount` / `totalBytes`, and ideally "N of M splits read (tier/bloom pruned)". Add a
   split-set row group (or reuse the existing groups with split-aware labels).
5. **Mode interaction.** The toggle applies to trigram (and possibly term) search; hide/disable it
   for semantic (vectors aren't split here). Mirror how the server-side toggle is shown/hidden per
   mode (`index.html` mode-change handler).

## Cleanup (remove splitset.html + its references)

- Delete `examples/openalex/web/splitset.html`.
- `index.html:632` — remove the `<a class="howlink" href="splitset.html">` link.
- `deploy.sh` — drop `splitset.html` from the HTML upload loop (`for h in index.html
  how-it-works.html splitset.html`) and from the CloudFront invalidation `--paths`; update the
  comment that references "the URLs in splitset.html". **Keep** `--splits` (still uploads the split
  data the integrated mode reads).
- Leave the unrelated standalone `examples/splitset-demo/` alone — different demo.

## Acceptance

- The main demo has a working **Split-set** toggle: the same query runs against the monolith and
  the split set, results render through the ranked-list path, facets filter, and the perf panel
  shows the split count + bytes (+ pruning) — covering everything splitset.html did.
- `splitset.html` and all references to it are gone; `deploy.sh` no longer uploads/invalidates it
  but still supports `--splits`.
- Works end to end once a split set is deployed (`deploy.sh --splits`), and degrades cleanly
  (toggle hidden/disabled) when the `.rrss` manifest is absent — like the demo already does for the
  uploading text index.
