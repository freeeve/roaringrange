# Task 008 — Usability & consistency review (end-user APIs across all surfaces)

A project-wide review of the public consumer surfaces — Rust crate (reader + builder),
wasm/JS, Python, Go, and the docs/examples — for **usability and consistency** (NOT
correctness or performance). Findings were gathered by six parallel surface reviewers and
synthesized below. One reported "broken build" (`gen_zstd_fixture` unregistered example) was
**verified false** — the example self-gates with a fallback `main`, and `cargo build --examples`
passes under default features.

## Cross-cutting themes (ranked by how many users they bite, and how silently)

- **A. Naming drift in core verbs/params.** Result cap is `limit` (`index.rs:373`,
  `terms.rs:279`) vs `k` (`splitset.rs:476`, `vector.rs:334`); finalizers are `build()` (3
  Python builders) vs `finish()` (`TermBuilder`); "construct from resident bytes" is `from_boot`
  (Index) vs `from_bytes` (SplitSet); `Model2vec` has no `open` at all (`model2vec.rs:44`);
  Python `add` means five different things.
- **B. JS filter/facet conventions fail silently.** Two filter encodings (`"field\tcat"` strings
  vs JSON-pair string on `RrsCatalog.search`, `wasm.rs:806`); malformed pairs silently dropped
  (`wasm.rs:1623`); two facet-count return shapes (JSON string vs structured object). Plus
  `RrssIndex.searchFiltered(query, filters, limit)` transposes args vs the `(query, maxMissing,
  filters)` family (`wasm.rs:1617`) — introduced this session.
- **C. Error model inconsistent / over-broad.** `IndexError::Malformed` (doc'd "corrupt file")
  used for caller errors (`sortcols.rs`, `secondary.rs`) and capability gaps (the term-split
  facet-filter error, `splitset.rs:812`); JS errors are bare `e.toString()` with no context; Go
  stutters (`roaringrange: roaringrange: …`) and drops `%w`; vector uses a typed error enum while
  every other builder uses `io::Error::other`.
- **D. Config ergonomics.** `Config` vs `BuildConfig` naming; `SplitBuildConfig`/`WriterConfig`
  not `Clone`/`Default` (must spell every field); `0`-means-default sentinel undocumented and
  inconsistent with `Option`; `byte_cap: 0` silent footgun; `SplitSetWriter::resume` six
  positional args incl. 3 transposable ints (`splitset_write.rs:151`).
- **E. Builder/Writer taxonomy not principled.** `SplitSetBuilder` (one-shot) vs `SplitSetWriter`
  (resumable) vs `RecordWriter` (streaming) — "Writer" means three things; `finish(w)` vs
  `finish()->struct`.
- **F. Cross-language parity gaps, no published matrix.** Python: no `.pyi`/`py.typed`; split
  builders lack `add_faceted`/`add_keys`; `TermSplitSetBuilder`/sortcols/lookup/hotcache/zstd
  unexposed. Go: split builder has no facet path; no sortcols/lookup/zstd; no `BodyKind` field.
- **G. Docs have no top-level format map.** README "frozen specs" lists 4 of ~8 docs; `RRIL` and
  `RRM2` have readers/builders but no spec doc; magic↔extension mismatches undocumented
  (`RRSI`→`.rrs`, `RRSR`→`.idx/.bin`, `RRVR`→`.rrvi.rerank`, `RRTI`→`.rrt`); stale `RRSC (planned)`
  stub in FACETS.md; broken openalex README link; SPLITSET.md cites `examples/…` (really
  `rust/examples/`).
- **H. `pub` field leakage.** `Index::gram_size` bare mutable `pub` (`index.rs:110`) vs getters
  elsewhere.
- **I. Faceting/cursor wired unevenly.** JS `RrtIndex` no faceting/cursor; term-split
  facet-filtered search deferred; `Catalog` has no sortcols re-rank hook.

## What's genuinely good (do not churn)
Uniform header validation (magic/version/checked-offsets, no panics on consumer paths);
`RangeFetch`/`SplitFetcher` traits; `RrsCatalog` + the `write_*` free-fn family; the Rust example
template (`//!` purpose + exact run cmd); Python error mapping + numpy-free FAISS byte interface;
the README "Tried and shelved" section.

## Tiers

### Tier 1 — quick wins (low-risk, mechanical) — DONE (2026-06-05)
- [x] README: format-inventory table (magic → extension → spec doc → reader/builder) added under
      a new "On-disk formats" section; repo-layout table gains a `rust/examples/` row.
- [x] Added `LOOKUP.md` (spec for `RRIL`).
- [x] Added a `Model2vec embedder (RRM2)` section to `VECTORS.md`; fixed the README anchors for
      RRVR/RRM2.
- [x] `FACETS.md`: stale `## SORTCOLS (planned, Phase 2)` stub → pointer to `SORTCOLS.md`.
- [x] Fixed broken link in `examples/openalex/README.md` (`../../roaringrange` → `../..`).
- [x] `SPLITSET.md`: example paths prefixed with `rust/`.
- [x] `Index::gram_size` field → `gram_size()` getter (updated `candidates.rs`, `density.rs`, the
      two in-crate tests, and the openalex `query` example).
- [x] Added `IndexError::Unsupported(&'static str)` (+ Display arm); term-split facet-filter error
      switched off `Malformed`.
- [x] Reordered wasm `RrssIndex.searchFiltered(query, limit, filters)` to match the
      `searchCursorFiltered` family; updated `splitset.html` + the standalone `splitset-demo`
      (index.html + README) and **rebuilt both committed wasm bundles** so the JS matches.

All gates green: rust fmt/clippy(`-D warnings`)/test for default+vector+terms+hotcache+splits+
`splits hotcache`+`splits terms`; wasm32 check; openalex builder clippy/build; gofmt clean.

### Tier 2 — consistency pass (some churn, no behavior change) — PARTLY DONE (2026-06-05)
Done this pass:
- [x] Result-cap naming: `SplitSet::search`/`search_filtered` (+ the tiered/stable-key/delta/all
      helpers) param `k` → `limit`, matching `Index`/`TermIndex`. (Vector keeps `k`/`nprobe` — the
      ANN idiom, per the reviewer.) Reader-only; no wasm/Python impact.
- [x] Derived `Clone` on `SplitBuildConfig`, `TermSplitBuildConfig`, `WriterConfig`; documented the
      `byte_cap` `0`-footgun (other `0`-sentinels were already doc'd at the field).
- [x] Reclassified the caller-arg `sortcols` errors (column OOB, doc-id OOB, "not u32") from
      `Malformed` → `BadQuery`; parse/corruption sites stay `Malformed`. Updated the two tests.

Remaining tier 2 (deferred, with reasons):
- **`Default` on build configs** — skipped deliberately: `byte_cap`/`name_prefix` have no safe
  default (a `Default` with `byte_cap: 0` is the footgun above). A `::new(required_args)` would be
  better than `Default` if added.
- **`SplitSetWriter::resume` → config** — needs a dedicated `ResumeConfig` (a full `WriterConfig`
  would carry `policy`/`tier_count`/`sortcol`/`byte_cap` that `resume` ignores, reading them from
  `prev`) plus the Python `resume` binding update. Small design call; deferred.
- **`Model2vec::open(fetch)`** for constructor parity — needs a whole-file fetch (no length on
  `RangeFetch`); deferred. (`Index::from_boot` vs `SplitSet::from_bytes` are NOT the same concept —
  `from_boot` is partial-resident-then-fetch, `from_bytes` is fully resident — so no rename there.)
- **`secondary` "missing primary column"** left as `Malformed` (it's a wrong-input-file structural
  error, genuinely borderline vs `BadQuery`).
- **JS error messages prefixed with `Class.op`** — moved to the tier-3 JS pass (it needs a wasm
  rebuild, which tier 3 does anyway).

### Tier 3 — JS contract fix (BREAKING; needs a wasm rebuild) — FILTER UNIFICATION DONE (2026-06-05)
- [x] **Filter encoding unified to structured `[field, category]` pairs.** New `filter_pairs(Array)
      -> Result<Vec<(String,String)>, JsError>` (uses the existing `js-sys` dep; **throws** on a
      malformed entry instead of silently dropping). All 7 entry points converted from `Vec<String>`
      tab-strings / the `RrsCatalog` JSON-string to `Array`: `filtered_ids`, `RrsIndex`
      `searchCursorFiltered`/`filterIds`, `RrfFacets.filterIds`, `RrsCatalog.search`,
      `RrsSecondaryIndex.searchCursorFiltered`, `RrssIndex.searchFiltered`. Deleted
      `parse_tab_filters` + `parse_filters_json`. Updated every call site in the 3 demos (index.html,
      splitset.html, splitset-demo) + the demo README; rebuilt BOTH wasm bundles. Gated: wasm32
      check, `cargo fmt`, `node --check` on all three demo modules.
- [x] **Facet-count return shape + naming consistency.** All 8 `*Json` accessors now return
      structured JS objects/arrays (no more `JSON.parse` on the caller) and are renamed to a
      consistent scheme: `facets()` (RrsIndex, RrfFacets, RrsCatalog, RrsSecondaryIndex — full-corpus
      `[{field, cats:[{name,count}]}]`), `facetCounts()` (RrsCursor, RrsSecondaryCursor, FilteredIds
      — query-restricted counts), `columns()` (RrsSortCols). Consolidated the four hand-rolled JSON
      string builders + `json_escape` into one `facets_array_js` (+ `facets_meta_array`/
      `facet_counts_js`/`facets_meta_js`); `facet_counts_to_js` (catalog) routes through it too.
      Cursors now cache a `JsValue` instead of a `String`. Updated all 5 HTML consumers (dropped the
      `JSON.parse`); rebuilt both bundles. Gated: wasm32, `cargo fmt`, `node --check` ×3.
- [ ] **Deferred (per agreement):** JS error-message `Class.op` prefixing — ~40 methods, MED value,
      high churn; not worth it right now.

Original plan (for the remaining sub-items):
Concrete plan (mapped against `wasm.rs`):
- **Filter encoding (the worst footgun).** 7 entry points currently take either `Vec<String>` of
  `"field\tcategory"` (silent-drops malformed entries) or, for `RrsCatalog.search`, a JSON-string
  of pairs — two encodings for one concept. Sites: `filtered_ids` (262), `RrsIndex.searchCursorFiltered`
  (379), `RrsIndex.filterIds` (418), the `RrfFacets.filterIds` path (487), `RrsCatalog.search`
  (`filters_json`, 806), `RrsSecondaryIndex.searchCursorFiltered` (1217), `RrssIndex.searchFiltered`
  (1623); helpers `parse_tab_filters` (1131) + `parse_filters_json` (982).
  **Plan:** one helper `filter_pairs(js_sys::Array) -> Result<Vec<(String,String)>, JsError>` (uses
  the existing `js-sys` dep — no new dependency), accepting `[[field, category], …]` and **throwing**
  on a malformed entry instead of dropping it. Change every entry point's filter param to
  `js_sys::Array`; delete the two old helpers. Update all call sites in
  `examples/openalex/web/{index,splitset}.html` + `examples/splitset-demo/index.html`. Rebuild BOTH
  wasm bundles. **The one decision:** structured `[field,category]` pairs (reviewer's rec, best JS
  ergonomics, fully breaking) vs. the lighter "keep tab-strings but unify `RrsCatalog` onto them +
  throw on malformed" (smaller breakage). Recommend the structured pairs.
- **Facet-count return shape** — `facetsJson`/`facetCountsJson`/`columnsJson`/`countsJson` return
  JSON *strings*; `RrsCatalog.search().facetCounts` already returns structured objects via
  `facet_counts_to_js`. Reuse that helper so all facet-count returns are JS objects (rename the
  `*Json` accessors). Separate sub-unit; also needs the rebuild.
- **JS error messages prefixed with `Class.op`** (moved from tier 2). Mechanical; same rebuild.

### Tier 4 — parity (largest) — MOSTLY DONE (2026-06-05)
- [x] **Python `.pyi` + `py.typed`** via a maturin *mixed layout* (`python_src/roaringrange/` with
      `__init__.py` + `__init__.pyi` + `py.typed`, `python-source = "python_src"` in pyproject).
      Verified the wheel ships all three + the `.so`, import works, 19 pytest pass. Stub hand-written
      from runtime introspection (accurate signatures + return types incl. the `flush`/`compact`
      tuples).
- [x] **Python `add_faceted`** on `SplitSetBuilder` (+ `build` now writes the per-split `.rrf`
      sidecars) — matches `Builder.add`'s `dict[str, list[str]]` facet shape.
- [x] **Python `TermSplitSetBuilder`** exposed (term/FST split sets from Python; `language`/
      `stopwords`). Two new pytest cases (facet sidecars + `.rrt` bodies / `body_kind=1`).
- [x] **Go `BodyKind` field** on `SplitSetConfig` (+ `BodyKindTrigram`/`BodyKindTerm` consts),
      written to header byte 9. Zero-value `0` → golden byte-identical (go test green).
- [x] **Support matrix** ("which builder/reader in which language") added to the README.
- [x] **Go split-builder facet path DONE (2026-06-05).** `SplitSetBuilder.AddFaceted(text,
  map[string][]string)` accumulates per-split facet postings; `seal` emits the per-split `RRSF`
  sidecar (Go `WriteFacets`) + the facet-presence summary (tag 2, sorted `FacetKey`s); `BuiltSplitSet`
  gained `Facets []NamedSplit`; `Finish` sets `SplitSetFlagFacet`. **Byte-for-byte conformant with
  Rust** — extended the shared fixture (`conformance_build` now faceted) and regenerated
  `go/testdata/rrss_build_golden.txt` (manifest + 4 splits + 4 `.rrf`); both the Rust
  `full_build_matches_shared_golden` and the Go `TestSplitSetBuilderMatchesRustGolden` assert the
  facet sidecars too. Added a `#[ignore] regen_shared_golden` Rust helper to rebuild the golden.
- [x] **Go error-message cleanup DONE.** Stripped the `roaringrange:` package-name stutter from all
  9 error sites (3 `errors.New` sentinels + 6 `fmt.Errorf`); no `%w` candidates (all are leaf errors,
  nothing to wrap).
- Still deferred: **Python `add_keys`** (niche); **Go RRTI term-bodied splits** (Go has no term-index
  writer); Builder/Writer taxonomy rename (theme E) — naming churn, separate pass.

## Non-breaking cluster — DONE (2026-06-06)
- [x] **`secondary` "missing primary column" → `BadQuery`** (was `Malformed`); test updated.
- [x] **`SecondaryCursor::next(n)`** added (Rust) for parity with the primary `Cursor`, + exposed on
      the wasm `RrsSecondaryCursor.next`; both bundles rebuilt.
- [x] **`reciprocal_rank_fusion` discoverability** — cross-linked from the `VectorIndex` doc.
- [x] **Python `Builder.add_many(rows)`** batch ingestion (+ `.pyi` entry + pytest; 20 pass).

## Open question: Go RRTI term-bodied splits (#7)
NOT "just coding" — the `RRTI` dictionary is a BurntSushi **`fst` crate** FST (the reader is
`fst::Map`). Go's off-the-shelf FST lib (vellum) uses a *different* on-disk format, so a
vellum-built RRTI would be **unreadable** by the wasm/Rust reader — and certainly not byte-identical
(the Go conformance bar). Aligning Go would require reimplementing the `fst` crate's exact
builder+serialization in Go (large, byte-exact-output is the hard part) PLUS a Snowball stemmer that
matches `rust-stemmers` byte-for-byte. So it's a real porting project, not a builder tweak. Pragmatic
status: Go stays trigram-split-only (it has no term-index writer at all).

## Gates
Rust fmt/clippy(`-D warnings`)/test for default + each feature combo (`vector`, `terms`,
`hotcache`, `splits`, `splits hotcache`, `splits terms`); wasm32 check; Go gofmt/test; builder
fmt/clippy/build; Python via maturin + pytest. (See `.githooks/pre-push`.)
