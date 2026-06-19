# Task 043 — facet exclusion (negative) filters

## ✅ DONE (2026-06-19)

A new `FilterSel { field, category, negate }` (facet.rs) carries the flag.
`resolve_sels` splits includes (per-field OR / cross-field AND = `P`) from a flat
exclude union `X`; `ResolvedFilter` applies `P ANDNOT X` via new
`apply_head`/`apply_tail` (intersect-then-subtract, so an excludes-only filter
works with no positive set to AND) plus exclusion in `membership_bitmap` and
`full_bitmap`. `Catalog::search` takes `&[FilterSel]`. The wasm `filter_sels`
parser accepts BOTH the legacy `[field, category]` / `[field, category, true]`
**array** AND the preferred `{ field, category, exclude? }` **object**, so every
existing array call keeps working. The include-only `resolve(&[(String,String)])`
stays as a wrapper, so secondary / splitset / the lambdas are untouched. Counts
run over the post-exclusion survivors; every category still lists so the UI can
un-exclude.

Verified: 3 native exclusion tests (exact sets + counts), full suite green
(143 + 12), clippy clean across the feature matrix; runtime on real facet data —
object and array exclude forms both partition the candidates (include ⊎ exclude,
0 overlap, union = all). Out of scope (per spec): the secondary-index cursor
stays include-only. Consumer (qllpoc) wires the `{…exclude:true}` form + bumps.

(Original spec below.)

---

Support **excluding** facet categories in filtered search and facet counts, not
just including them. A selection can now be negative: remove docs in that
category. Motivating query: "speculative fiction **but not** short stories".

## Why

The qllpoc browse page wants an "(exclude)" control next to each facet value
(subject/tag/format/…). Today a filter can only *include* a category; there is no
way to say "everything in Speculative Fiction except Short Stories" without
enumerating every other format. Exclusion is a single roaring `ANDNOT`, so it
belongs in the facet engine — doing it client-side means re-deriving set algebra
in JS, an extra `filterIds` pass per exclusion, and hand-patching the facet
counts. Native and python consumers would still lack it.

## Current

The filter is a list of `(field, category)` pairs — within a field the categories
OR, across fields they AND (`catalog.rs:114`, applied in `facet.rs`). The wasm
boundary takes `[field, category]` arrays (`RrfFacets.filterIds`,
`RrsCatalog.search`, `RrsIndex.filterIds`). There is no negation.

## Proposed API

Keep the 2-element pair as **include** (fully backward compatible); add an
optional third element marking **exclude**:

```
[field, category]          // include (unchanged)
[field, category, true]    // exclude — remove docs in this category
```

Internally a `FilterSel { field: String, category: String, negate: bool }` (or a
`(String, String, bool)`); the native API gains the `negate` flag. The wasm
binding parses a 2- or 3-element array per entry.

## Semantics

- **Includes** unchanged: within-field OR, across-field AND → the positive set `P`
  (full corpus if no includes).
- **Excludes** combine as a single OR-union `X` across *all* excluded `(field,
  category)` (a doc is dropped if it matches any exclusion, regardless of field),
  then `result = P ANDNOT X`. This makes "speculative fiction AND NOT short
  stories" and stacking multiple exclusions both work.
- **Facet counts** are computed over the post-exclusion survivors (falls out if
  exclusion is applied before counting). Drill-down nuance to preserve: an
  excluded category must **still appear in its own field's count list** so the UI
  can un-exclude it (mirror the existing "count each active field with its own
  selections removed" behavior, treating a field's excludes like its includes for
  the self-removal pass). Document the chosen counts semantics in the doc comment.
- `filter_count_bound` / `filter_cost` (`facet.rs:369/389`): excludes only shrink
  the set, so the existing upper-bound stays valid; cost may ignore excludes or
  add their resident cardinality. Keep it conservative.

## Where

- `facet.rs` — build `P` (existing), union the excluded category bitmaps into `X`,
  `andnot`; thread `negate` through the pair parsing + count passes.
- `catalog.rs` `search()` — accept and forward the extended filter list.
- `wasm.rs` — `RrfFacets.filterIds`, `RrsCatalog.search`, `RrsIndex.filterIds`
  parse 2/3-element entries; update the doc comments (the `[["format","ebook"]]`
  examples) to show the exclude form.

## Tests

- exclude removes only matching docs; non-matching untouched
- include + exclude combine: `[["genre","Speculative Fiction"]]` +
  `[["format","Short Stories",true]]` → speculative minus short stories
- multiple excludes union (exclude two formats → neither appears)
- exclude with **no** positive filter (exclude over the full corpus)
- facet counts reflect exclusion; the excluded category still lists in its field
- malformed entry (bad arity / non-bool flag) throws, matching current behavior
- 2-element entries still behave exactly as before (regression)

## Consumer (qllpoc)

`facets.js`: an "(exclude)" button right of each value → an `excluded` state that
deep-links through the URL (mirror of `active`, e.g. `x<cat>` params); `browse()`
in `roaring-search.js` passes the 3-element exclude pairs straight through. A
value can be included or excluded but not both. Bundle the new wasm + bump.
