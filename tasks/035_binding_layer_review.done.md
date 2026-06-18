# Task 035: python + wasm binding-layer review

A consistency/quality review of the binding layers (`python/src/lib.rs` via pyo3,
`rust/src/wasm.rs` via wasm-bindgen), after the library-API passes (tasks 033/034)
which didn't focus on the bindings. 3-way parallel review.

**Verified clean (headline):** every recent native change is correctly reflected in
wasm — `load_*`, `fields()` getter, `searchBm25MinMatch`, `u32` `len()`,
`Metric`/`BodyKind` correctly absent from the reader surface. Python error mapping
(`PyValueError`/`io_err`/`build_err`, no panics), lossless marshaling both sides, and
the `from_code` centralization all verified clean.

## Findings — all DONE (commit folds into pending v0.19.0)

- **B1 `[HIGH, my miss]`** — python language error at `lib.rs:553,802` still said
  `(supported: "english", "spanish")` after the 18-language change. Reworded to not
  hardcode the set (Snowball name or ISO-639-1 code, with examples).
- **B2 `[MEDIUM, real bug]`** — `RrsCatalog` `.expect("catalog present")`
  (`wasm.rs:864,897,910,932`) panicked in the browser: the consuming `load_*`
  builders drop the `Catalog` on a fetch error, leaving `inner = None`, so the next
  call crashed. `cat()` now returns `Result<&Catalog, JsError>` (transparent to the
  `.d.ts`); `facets()`/`ngramCount()`/`search()` propagate it; the builder `take()`s
  use `ok_or_else(JsError)?`. A failed sidecar load is now a catchable JS error, not
  a crash.
- **B3 `[MEDIUM, naming]`** — `TermBuilder::add_batch` → `add_many` (the convention
  the other builders use). Updated the method, doc, `.pyi` stub, and the streaming
  script.
- **B4 `[LOW, consistency]`** — `RrbIndex.doc_count()` returned `f64` while every
  `len()` is `u32`; native is now `u32` (T1) → return `u32`. Transparent to JS (both
  marshal to TS `number`; `.d.ts` still `docCount(): number`). `RrssIndex.doc_count`
  legitimately stays `f64` (it's a `u64` Σ across splits).
- **B5 `[LOW]`** — python `SplitSetWriter::resume` used an inline error-map; added an
  `index_err` helper (the read-side parallel of `io_err`/`build_err`) and routed it
  through.

## B6 — REFUTED (verification prevented an API break)

The "redundant `js_name`" finding was a **false positive**. The generated `.d.ts`
has **zero** snake_case methods and *every* multi-word wasm method carries an
explicit `js_name` (only the `#[start]` fn doesn't) — wasm-bindgen does **not**
auto-camelCase method names, so those annotations are **required**; removing them
would rename `isEmpty`→`is_empty` etc. and break QLL's JS. The `FilteredIds`
getter-vs-method and `WasmBitmap` `u64→u32` clamp items are JS-visible / near-
unreachable and not worth a break. No B6 change made.

## Verification

Python `cargo check` clean; `wasm-pack build` + wasm-target clippy clean;
`.d.ts` exports unchanged for `docCount`/`isEmpty`/`searchBm25MinMatch`; pre-push
gate green. No on-disk format change.
