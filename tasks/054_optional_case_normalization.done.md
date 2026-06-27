# Task 054 — optional case normalization at index creation

Make case normalization (lowercasing / case folding) an **optional** index-creation
setting, `case_normalization: bool`, **default `true`** (= current behavior). When
`false`, the index is case-sensitive: no lowercasing at build **or** query time, and
the header records the choice so the reader reproduces it (the build/query symmetry
invariant). Default builds stay **byte-identical** to today, so every existing
artifact and golden vector is unaffected.

Scope (chosen): **all text surfaces** — term `RRTI`, trigram `RRSI`, facet `RRSF`,
and the split-set `RRSS` manifest — with **Go-port conformance** (terms.go, ngram.go,
facets.go, splitset.go) and new golden/differential-fuzz vectors for the
case-sensitive path. BM25 `.rrb` rides on the term tokenizer (no own folding).

## Design — one flag per surface, default-off bit so defaults stay byte-identical

### Term `RRTI` (clean — spare flag bits)
- `tokenize(text, case_fold)`: fold only when `case_fold`.
- `Tokenizer` gains `case_fold: bool`. `new(language, stopwords, case_fold)`,
  `plain()` = `(None,false,true)`, `from_header` derives `case_fold = flags &
  FLAG_CASE_SENSITIVE == 0`.
- Header `flags` bit 2: `FLAG_CASE_SENSITIVE = 4` — set iff `case_normalization == false`.
- `TermIndexConfig` + `TermSplitBuildConfig` gain `case_normalization: bool`.
  Thread through `TermIndexBuilder`, `write_term_index_from_postings`,
  `TermIndexStreamWriter` (writes the flag), and `TermSplitSetBuilder::seal`.
- Reader `search_prefix` / `complete` lowercase the prefix directly — gate on
  the tokenizer's `case_fold`.

### Trigram `RRSI` (no spare header field — append-only v4)
- `ngram_keys(query, gram_size, case_fold)` + `normalize(s, case_fold)`: keep the
  letter/digit filter, fold only when `case_fold`. Update all 6 build/query sites.
- Header: keep v3 = 16 B (default, byte-identical). Add **v4** = v3 layout **plus a
  2-byte `flags` field appended at offset 16** (no existing field shifts; sparse keys
  follow at the version-dependent `HEADER_SIZE`). `flags & 1 = case_sensitive`.
  Default builds (`case_normalization=true`) emit v3; case-sensitive builds emit v4.
  Reader accepts v3 (fold) and v4 (read flag).
- `SplitBuildConfig` gains `case_normalization`; threaded into per-split RRS headers.

### Facet `RRSF` (spare `reserved u16` at offset 6)
- `facet_key(field, category, case_fold)`: fold only when `case_fold`.
- `write_facets` takes the flag; writes `reserved`=1 iff case-sensitive. Reader
  (`FacetIndex`) reads it and uses it for filter-side `facet_key` recomputation.
- Split-set presence-Bloom keys (`facet_presence_keys`) use the same flag.

### Split-set manifest `RRSS`
- Store the index-wide case flag once (spare bit) so the split-set query path —
  which computes `ngram_keys` / `facet_key` once across all splits — stays symmetric.
  Each split artifact also self-describes (above), so a standalone split open is correct.

## Surfaces / ports
- Rust core: terms.rs, terms_build.rs, ngram.rs, index.rs, build.rs (RRS+RRSF),
  splitset_build.rs, splitset.rs (manifest + readers), facet.rs.
- wasm (wasm.rs) + Python (python/src/lib.rs): expose the new config field.
- Go: terms.go, ngram.go, facets.go, splitset.go, reader.go + builders.
- Docs: TERMS.md, FORMAT.md, FACETS.md, SPLITSET.md.
- Tests: unit (fold on/off symmetry per surface), golden + cross-lang differential
  fuzz vectors for a `case_normalization=false` index of each body kind.

## Invariants
- `case_normalization=true` ⇒ every output byte identical to today (golden tests green).
- Build folding == query folding for every surface (the correctness invariant).
- Rust and Go byte-conformant on the case-sensitive path.

## Status
DONE. Implemented across every surface and port, defaults byte-identical.

- **Rust**: `tokenize_with`/`Tokenizer.case_fold` + `FLAG_CASE_SENSITIVE` (RRTI); `ngram_keys_with`
  + RRSI **v4** (16→18 B header, trailing `flags`) in `write_index_with`/reader/`rrs_boot_len`;
  `facet_key(.., case_fold)` + `write_facets_with` + RRSF reserved-flag; manifest `FLAG_CASE_SENSITIVE`
  + split-set reader threads it to ngram/facet pruning; delta `SplitSetWriter` inherits it. Config
  field `case_normalization: bool` (default true) on `TermIndexConfig`/`TermSplitBuildConfig`/
  `SplitBuildConfig`/`WriterConfig`.
- **Go** (zero-value-safe inverse `CaseSensitive bool` on config structs; `...With` fns elsewhere):
  ngram.go, terms.go, facets.go, monolithbuild.go, splitsetbuild.go, termsplitsetbuild.go,
  splitset.go (flag), reader.go (`CaseFold`, v4), transcode.go (v4 writer). gofmt -s clean.
- **wasm**: reader-only, reads v4/flags via the Rust reader (no change). **Python**: `case_normalization`
  param on TermBuilder / SplitSetBuilder / TermSplitSetBuilder.
- **Conformance vectors** (Rust generates golden, both Rust + Go assert byte-for-byte):
  `testdata/rrs_monolith_cs_build_golden.txt` (v4 RRSI) and `testdata/rrti_term_split_cs_golden.txt`
  (RRTI + RRSF + manifest case flags). Go `FuzzNgramKeys` extended with the case-sensitive property.
- **Docs**: FORMAT.md (v3/v4), TERMS.md (bit2), FACETS.md (reserved bit0), SPLITSET.md (bit4).
- Verified: Rust `cargo test --all-features` (186 lib + integration) + clippy clean; Go `go test ./...`
  (root + conformance module) + 435k-exec fuzz; python `cargo check`/clippy.

## Final API note (config field flip)
The public **config** field shipped as `case_sensitive: bool` (**default `false` = case-fold**), not the
originally-specced `case_normalization: bool` (default true). Rust was flipped to match Go's
`CaseSensitive` convention and to make `false` the natural default value. Applies to `TermIndexConfig`,
`SplitBuildConfig`, `TermSplitBuildConfig`, `WriterConfig` (Rust) and the Python
TermBuilder/SplitSetBuilder/TermSplitSetBuilder constructors (`case_sensitive=False`). Builders translate
at construction to the internal `case_normalization` (true = fold) via `!config.case_sensitive`; the
low-level/operation fns (`tokenize_with`, `ngram_keys_with`, `write_index_with`, `write_facets_with`,
`write_term_index_from_postings`, `WriteTermIndexWith`, …) keep their `case_fold`/`case_normalization`
back-compat params unchanged. The flip is a config rename only — byte-neutral, all goldens stayed green
(`cargo test --all-features`: 199 pass / 0 fail).

Optional follow-up: a dedicated cross-language (Rust-generates / Go-verifies) fuzz harness for the
case-sensitive path (the golden vectors + the Go property fuzz already cover it).
