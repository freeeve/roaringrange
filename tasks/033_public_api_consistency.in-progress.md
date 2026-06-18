# Task 033: public-API Rust consistency pass

A cross-module review of the full public API (15 `src/` modules + the wasm and
python binding layers) surfaced a set of naming/typing drifts from the otherwise
strong baseline. This task tracks fixing them.

**Baseline that must NOT regress** (these are already consistent — keep them):
reader ctor = `async open(fetch) -> Result<Self, IndexError>`; builders =
`sync new()` + `write_*` free fns; one read error `IndexError` / one fetch error
`FetchError`; `u32` doc IDs, `u64` byte offsets; bare-name getters (no `get_`);
`async fn` everywhere except `RangeFetch::read`; regular wasm snake→camel mapping.

Crate is at **0.1.0**, so breaking changes are acceptable, but every in-repo
consumer (wasm `src/wasm.rs`, `python/`, `rust/examples/`, the build tooling)
MUST be updated in the same change so the workspace always compiles. The external
QLL repo consumes the wasm surface — call out any wasm-visible rename in the
commit body so it can follow.

Legend: 🟢 additive/non-breaking · ⚠️ breaking · markers are per-item.

---

## Findings (grouped; implement in the Order below)

### Typing

- **T1 ⚠️ `len()` returns three different integer types.** `usize`
  (`terms.rs:254` `TermIndex::len`, `fetch.rs:80` `MemoryFetch::len`), `u32`
  (`lookup.rs:88`, `records.rs:105`), `u64` (`vector.rs:252` `VectorIndex::len`,
  `vector.rs:565` `RerankStore::len`). Same concept is also spelled
  `SortCols::rows() -> u32` (`sortcols.rs:198`) and `ImpactIndex::doc_count() ->
  u64` (`bm25.rs:154`). Decide ONE rule (std convention is `len() -> usize`; or
  `u64` for all on-disk entity counts) and apply; keep `rows()`/`doc_count()` as
  domain aliases returning that type.

- **T2 🟢 Conversions are ad-hoc inherent methods, not `From`/`TryFrom`.**
  `Language::to_u8` (`terms.rs:79`), `Policy::to_u8` (`splitset.rs:100`),
  `MemberTag::to_u16` (`hotcache.rs:89`), `Value::as_f64` (`sortcols.rs:97`),
  `ValueType::width`. Add `From`/`TryFrom` impls (Rust API Guideline C-CONV) for
  the on-disk code↔int conversions; may keep inherent methods as thin shims.

- **T3 🟢 `Language::from_u8` is private while `to_u8` is public**
  (`terms.rs:79` vs `:87`) — can serialize but not parse back. Make the reverse
  public (ideally the `TryFrom<u8>` from T2).

- **T4 ⚠️ On-disk code sets: enum in some modules, bare `const` in others.**
  Enums: `Policy`, `MemberTag`, `Language`, `ValueType`/`Value`, `ColumnValues`.
  Bare consts: `BODY_KIND_TRIGRAM/TERM: u8` (`splitset.rs:71,75`, surfaced raw by
  `body_kind() -> u8`), `METRIC_IP/METRIC_L2: u8` (`vector.rs:40,42`, stored as
  `IvfpqParams.metric: u8`). Promote `BodyKind` and `Metric` to enums (with the
  `TryFrom<u8>` from T2). Model the `FLAG_*`/`SPLIT_FLAG_*`/`SORTCOL_FLAG_*` bit
  sets as a bitflags-style newtype instead of loose `u16`/`u8` + manual `&`.

### Naming

- **N1 ⚠️ "construct from in-memory bytes" has three names.** `from_boot`
  (`index.rs:179`, `lookup.rs:70`, `records.rs:65`), `from_bytes`
  (`splitset.rs:317`, `model2vec.rs:44`), `FacetMeta::parse`+`attach`
  (`facet.rs:103,109`). Settle the convention: `from_boot(header, fetch)` for the
  fetch-pairing variants; reserve `from_bytes`/`parse` for genuinely
  self-contained parses. The facet two-step is the outlier — add a
  `FacetIndex::from_boot(meta, fetch)` matching the others.

- **N2 ⚠️ top-N param is `limit` in half the searches, `k` in the other half.**
  `limit`: `Index::search`, `TermIndex::search`, `SplitSet::search`,
  `Cursor::page`. `k`: `Index::search_candidates`, `VectorIndex::search`,
  `search_bm25(…, m, k)`, `SortCols::topk`, `ImpactIndex::rerank`. Page size is
  `len` in `Catalog::search(offset, len, …)` but `limit` in
  `Cursor::page(offset, limit)`. Use `limit` for rank/cursor result caps; keep
  `k` only for genuine algorithmic-k (vector ANN, BM25 rerank) and document the
  split. Rename `Catalog::search`'s `len` → `limit`.

- **N3 🟢 wasm `Rrs*` prefix is overloaded across formats.** Named by format:
  `RrtIndex`(RRTI), `RrbIndex`(RRSB), `RrviIndex`(RRVI), `RrssIndex`(RRSS),
  `RrhcBundle`(RRHC). Lumped under `Rrs*`: `RrsLookup`(RRIL), `RrsSortCols`(RRSC),
  `RrsRecords`(RRSR), `RrsSecondaryIndex`, `RrsCatalog`, `RrsCursor`. Cosmetic and
  wasm-visible (QLL/JS) — decide whether to rename for format accuracy or accept
  `Rrs` = "roaringrange". **Likely defer / lowest priority** (breaks JS names).

- **N4 🟢 `with_*` mixes sync-infallible and async-fallible.** Sync `-> Self`:
  `RecordStore::with_dict` (`records.rs:99`), `Ivfpq::with_opq`
  (`vector_build.rs:109`). Async `-> Result<Self>`: `Catalog::with_facets/
  with_records` (`catalog.rs:61,69`), `SecondaryIndex::with_facets`
  (`secondary.rs:69`). Keep `with_*` for cheap chainable setters; rename the
  fetching ones (`load_*`/`attach_*`) so I/O+fallibility is visible at the call
  site.

- **N5 🟢 batch accessors: `get_many` vs `values`.** `get_many`
  (`records.rs:225`, `vector.rs:582` `RerankStore`) vs `values`/`values_u32`
  (`sortcols.rs:217,272`). Single is `get`/`value`. Prefer the `get`/`get_many`
  pair; keep `values_u32` as a typed specialization if needed.

- **N6 🟢 `Language` string-parsing is duplicated and English-only.** Copy-pasted
  `"english" => Language::English` in `python/src/lib.rs:551`, `:801`,
  `rust/examples/build_term_splitset.rs:72`; no `Language::from_code`/`FromStr`.
  Add `Language::from_code(&str) -> Option<Self>` (or `FromStr`) + an `as_code`/
  `code() -> &str`, route all three call sites through it, and add the
  `"spanish"|"es"` arm. **This lands the already-present-but-unreachable `Spanish`
  enum variant** (the uncommitted `terms.rs` change) properly instead of reverting
  it. Pairs with T2/T3.

### Smaller notes

- **S1 🟢** `Cursor` (`index.rs`) and `SecondaryCursor` (`secondary.rs`) have
  byte-identical surfaces (`next`/`page`/`head_bitmap`/`loaded`/`head_count`/
  `pending_tail`/`load_tail`) — extract a shared `Cursor` trait so generic code
  can page either.
- **S2 🟢** `FacetIndex` exposes `pub fields: Vec<Field>` (`facet.rs:84`) while
  every other reader encapsulates state behind getters. Make it a `fields()`
  getter.
- **S3 🟢** `FileFetch::open` is sync and named `open` (`fetch.rs:180`), which
  everywhere else signals the async fetch-ctor. Idiomatic for a file handle —
  just add a doc note.
- **S4 🟢** `VectorBuildError` (`vector_build.rs:63`) is the only build-side
  domain error enum; other writers funnel into `io::Error::other("string")`.
  Consider whether `terms_build`/`splitset_build` validation deserves the same.
  **Optional / judgement call.**

---

## Order of work (dependency- and risk-ordered)

1. **N6 + T2 + T3** — `Language::from_code`/`as_code` + `From<Language> for u8` +
   `TryFrom<u8> for Language`; route the 3 parse sites; wire `Spanish`. Additive.
2. **T2 (rest) + T4** — `From`/`TryFrom` for `Policy`/`MemberTag`; promote
   `BodyKind` + `Metric` enums; bitflags newtype for the flag sets. (Touches
   on-disk readers/writers — keep byte layout identical, verify with tests.)
3. **N1** — unify byte-construction naming (`FacetIndex::from_boot`, audit
   `from_bytes`).
4. **N2** — `k`/`limit` parameter naming sweep (+ `Catalog::search` `len`→`limit`).
5. **N4, N5** — `with_*` → `load_*` for fetching variants; `values`→`get_many`.
6. **T1** — settle and apply the `len()` integer-type rule.
7. **S1, S2, S3** — shared `Cursor` trait; `FacetIndex::fields()` getter; doc note.
8. **N3, S4** — deferred (wasm-name churn / optional) — do last or split out.

## Acceptance (per step and overall)

- `cargo test --all-features` green; `cargo clippy --all-features --all-targets`
  clean; `cargo fmt --check` clean.
- `wasm-pack build --target web --features "wasm terms vector hotcache splits"`
  succeeds; any renamed JS export is intentional and noted.
- `python/` still builds (it consumes `Language`, `Index`, etc.).
- On-disk formats unchanged (these are API/naming changes, not format changes):
  existing `.rr*` fixtures/tests must pass byte-for-byte.
- Each step is its own semantic commit; wasm-visible breaks flagged in the body.

---

## Progress

- **Step 1 — N6 + T2 + T3 — DONE (uncommitted).** `terms.rs`: replaced the
  hand-rolled 2-language enum with a single source-of-truth `languages!` table macro
  expanding to the enum + `from_u8`/`from_code`/`as_code`/`to_u8`/`algorithm` +
  `impl From<Language> for u8`. **Expanded English+Spanish → the full 18-language
  Snowball set** `rust-stemmers` ships (bytes 1–2 locked historical, 3–18 appended;
  byte = stable header encoding). `from_u8` now `pub` (T3); `From<Language> for u8`
  added (T2). Routed the 3 duplicated English-only parse sites
  (`python/src/lib.rs` ×2, `rust/examples/build_term_splitset.rs`) through
  `Language::from_code` (N6), each now accepting all 18 + ISO aliases. Added
  `language_code_and_byte_roundtrip` test (byte/name/ISO round-trip, dense-1..=18
  byte guard). Verified: `cargo test --features "terms splits"` 132 pass, clippy
  clean, fmt clean, example builds, **`python/` cargo check passes**. On-disk format
  unchanged (English still byte 1). TryFrom for the strict enums folded into Step 2.
- Steps 2–8: pending.
