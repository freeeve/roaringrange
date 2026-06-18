# Task 034: non-core deeper consistency pass

A second consistency review (after task 033's public-reader-API pass), going into
the **non-core** modules' build-side writers and internal seams — error handling,
feature-gating, naming, duplication. Non-core = everything additive (terms/bm25,
vector/model2vec, splits, hotcache, facet, records, sortcols, secondary, lookup,
catalog + their build sides); core = index.rs/fetch.rs/posting.rs.

Driven by a 4-way parallel review. **Verified clean axes:** feature-gating has zero
bugs; every reader `open`/`from_boot` checks both magic AND version; the term writer
is single-source; quantization is pure/deterministic; every build records its params
in the header.

**Refuted false positives (verified):**
- "`vector_build.rs` missing wasm cfg" — it's gated at its `pub mod` in `lib.rs:97`
  (`#[cfg(all(feature="vector", not(target_arch="wasm32")))]`).
- "`head_boundary` seam silently corrupts `.rrb`" — `ImpactsAccumulator::new` takes
  no `head_boundary`; `write_impacts` keys impacts by the real `head_off` from
  `dict_terms()`, ordered by ascending doc id (accumulator + posting both follow).
  `head_boundary` never enters `.rrb` addressing; a tokenizer mismatch (the real
  seam) is already caught loudly.

## Findings

- **F1 — error-message format-prefix drift `[HIGH confidence, low risk]` — DONE.**
  ~28 `IndexError`/`io::Error` strings didn't name their format. Prefixed them with
  the 4-char code (RRSF/RRSC/RRSR/RRTI/RRSS): `facet.rs`, `sortcols.rs`,
  `records.rs`, `terms.rs`, `terms_build.rs`, `splitset.rs`, `splitset_write.rs`
  (the `compact:` paths). Mechanical, no behavior change.
- **F2 — inconsistent `usize→u32` truncation guards `[MEDIUM]` — TODO.** On-disk
  size fields are u32; some sites `try_into()`-guard → clean error
  (`splitset_build.rs:109`), many bare-`as u32` silently truncate (`build.rs:102,
  156,179,197,499`; `splitset_write.rs:428,492`; `splitset_build.rs:351,371`).
  Make guarding uniform (error, not silent truncation).
- **F3 — error-TYPE convention `[MEDIUM, DEFERRED]`.** `VectorBuildError` enum
  (vector_build only) vs `io::Error::other("string")` (everyone else) vs
  `IndexError` (readers); `vector_build` is inconsistent with itself
  (`write_rerank` uses `io::Error::new(InvalidInput)` vs `build_ivfpq`'s enum).
  **Verdict (what's rusty):** the idiomatic ideal is one shared build-side
  `BuildError` enum with an `Io(io::Error)` variant + domain variants
  (`From<io::Error>`/`Display`/`Error`); the pragmatic-good for this no-deps,
  internal-tooling crate is format-prefixed `io::Error::other` (which F1 delivers).
  Deferred by decision — revisit if the build side grows a public, matched-on API.
- **F4 — build-side duplication `[MEDIUM]` — partial.** (a) `build.rs` sparse-index
  emission is **byte-identical** in `write_index` (93–95) and
  `merge_partials_to_rrs` (813–815) — extract a shared helper. **TODO.** (b) splitset
  `SplitSetBuilder` vs `TermSplitSetBuilder` share ~70–80% of `add_*`/`seal`/
  `drain_sealed` — a shared token-extractor/encoder seam would dedup it. **DEFERRED**
  (larger refactor, real risk; do as its own task).
- **F5 — smaller `[LOW]`.** `SORTCOLS_`-prefix const drift (`build.rs:663–665`); no
  `SplitSpec` consistency validation (`doc_id_lo ≤ doc_id_hi`); implicit BM25 scale
  (`k1+1` rediscovered — a `IMPACT_SUPREMUM` const + assert); vague overflow messages
  (no actual count); `.expect()` on infallible Vec serialize; redundant
  `super::quantize_impact`.

## Plan (decided)

Do **F1 + F2 + F4(a)** now (commit per fix); F3 deferred (verdict recorded);
F4(b) and F5 left as backlog.

## Acceptance

- `cargo test --all-features` green; pre-push gate (fmt + clippy `-D warnings`
  across the feature matrix on rust + openalex-builder, gofmt) green; wasm + python
  compile. No on-disk format change (error strings + internal refactor only).
