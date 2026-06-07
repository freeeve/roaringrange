# 003 — Secondary index (sort columns + second-order index)

Status: **done** — `RRSC` format, reader, and the OpenAlex builder integration
are complete. Surfacing it in the live demo (Relevance/Newest toggle) + the
Lambda newest+filters path was spun off to **task 017**.

Search/paginate the corpus in a rank order other than the primary static rank,
without losing the constant-cost-per-query property. Two mechanisms sharing one
substrate (`RRSC` sort columns):

1. **Bounded re-rank (SORTCOLS):** a dense value-per-doc column; fetch values for a
   materialized candidate set and heap-sort top-K client-side.
2. **Full second index:** a second `.rrs` reindexed in S-order + a permutation column
   `secondary_docid → primary_docid` (records/facets stay keyed by primary ID).

Design + rationale: `/Users/efreeman/.claude/plans/happy-launching-beacon.md`.

## Stage 1 — `RRSC` SORTCOLS format  (done)
- [x] `SORTCOLS.md` spec
- [x] `rust/src/sortcols.rs` reader (`SortCols`: open, values, values_u32, slice_u32, topk)
- [x] `write_sortcols`/`write_perm` + `ColumnValues`/`SortColumn` in `rust/src/build.rs`
- [x] `lib.rs` wiring
- [x] `RrsSortCols` wasm binding
- [x] tests (round-trip all 4 types, coalescing, topk ordering, truncation, bad magic)

## Stage 2 — full second index + perm map  (reader done)
- [x] `SecondaryIndex` + `SecondaryCursor` reader (`rust/src/secondary.rs`)
- [x] wasm mirror (`RrsSecondaryIndex`/`RrsSecondaryCursor`, pages → primary IDs)
- [x] tests + `rust/examples/secondary.rs` (build→read on a tiny 2-order corpus)
- [x] **filtered** secondary search reader: `SecondaryIndex` carries an optional
      secondary-space `FacetIndex`, resolves filters → secondary postings via the
      space-agnostic `Index::search_cursor_filtered`, exposes `fields()`/`counts()`;
      cursor exposes `head_bitmap()`. wasm: `openFacets`/`facetsJson`/
      `searchCursorFiltered` + `RrsSecondaryCursor::facetCountsJson`. Tested.
- [x] builder: secondary doc-ID assignment + second `.rrs` + perm column + secondary
      `.rrf` remap — implemented in `examples/openalex/builder/src/secondary.rs`
      (`build_secondary` / `order_perm` / `write_perm` / `write_secondary_facets`,
      date-desc perm, test `perm_is_date_desc_and_consistent`)
- [ ] (optional) `Catalog::with_secondary` convenience — standalone module chosen to
      match the demo's separate-objects wiring; add if a one-shot facade is wanted

## Stage 3 — OpenAlex demo + Lambda → spun off to task 017
The library + builder are complete; only the user-facing surfacing remains, tracked
in `tasks/017_secondary_index_demo_lambda.md`:
- date-desc secondary artifact (`.rrs` + perm + `.rrf`) + Relevance/Newest toggle
- server-mode (Lambda) wiring for newest+filters

## Deferred
- Go writer/reader + `go/conformance/` for `RRSC` (precedent: `.rril` is Rust-only).

## Verification (all green)
- `cargo test` → 53 passed; `cargo fmt --check` clean; `cargo clippy --lib` clean.
- `cargo run --example secondary` → newest-first "alpha" maps to primary [1,2,0].
- `wasm-pack build --target web --features wasm` → exports `RrsSortCols`,
  `RrsSecondaryIndex`, `RrsSecondaryCursor`.
