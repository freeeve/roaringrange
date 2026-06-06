# Task 011 — Byte-exact Go port of `rust-stemmers` (Snowball English / Porter2)

**Status:** pending (scoping)

A Go `stem(word) -> stem` that is **byte-identical** to `rust-stemmers`' English stemmer
(Snowball "english" = Porter2, `Algorithm::English`) for every input, so the Go build side
stems terms exactly as the Rust/wasm reader stems queries. This is the term-index
correctness invariant: **build-time and query-time stemming must agree byte-for-byte**, or
every stemmed lookup silently misses.

Unlike task 010 (FST port), this one is **needed regardless** — an `RRTI` built from Go needs
matching stemming no matter the dictionary representation (009 shipped a blocked, front-coded
dict with a small FST router; stemming is orthogonal to it).

## Why byte-exact

`terms.rs` stems with `rust-stemmers` (Snowball English) and the reader stems queries the
same way (recorded via the header flags + language byte). If a Go builder stems
`"learning" → "learn"` but with any divergence from `rust-stemmers` on some inflection, the
indexed term and the queried term differ and the result is a silent miss — the same
failure mode `go/conformance/` exists to prevent for `normalize_id` / n-gram keys.

## Scope

1. **English (Porter2) stemmer in Go**, matching `rust-stemmers`' output. `rust-stemmers` is
   auto-generated from the Snowball compiler, so matching the **same Snowball algorithm
   revision** of `english` should give parity. Implement from the canonical Snowball
   `english` algorithm (the generated stemmer logic: region R1/R2, step 0–5 suffix rules,
   exceptions list, y→Y vowel handling, apostrophe handling).
2. **Stop words**: `terms.rs::STOP_WORDS` is a fixed sorted list — the Go side must use the
   **identical list + the same drop point in the pipeline** (drop after lowercase, before
   stem). Trivial but part of the invariant; include in conformance.
3. **Tokenizer parity** (shared with the term builder): the base tokenizer is Tantivy's
   `SimpleTokenizer` + `LowerCaser` (split on non-alphanumeric, `to_lowercase`). The Go side
   must reproduce that (incl. Unicode `to_lowercase` semantics) so the pre-stem tokens match.
   If task 005's filtering (min-DF / drop numeric/overlong) lands, mirror that too.

## Validation (this is the deliverable's teeth)

- **Large word-list parity**: run a big vocabulary through `rust-stemmers` and the Go port,
  diff every output. Sources: the Snowball project's `english/voc.txt` → `output.txt` test
  vectors (the canonical correctness set), plus a real OpenAlex-derived term sample (so the
  test reflects the actual corpus, including odd tokens).
- Pin the `rust-stemmers` version (match `rust/Cargo.toml`) and the Snowball revision; record
  both, since the English stemmer has had minor revisions and version skew = silent misses.
- Wire into `go/conformance/` so CI fails on any divergence.

## Tractability vs task 010

Much easier than the FST port: Snowball stemming is a well-defined deterministic algorithm
with canonical test vectors, and existing Go implementations exist to cross-check
(`kljensen/snowball`, the Snowball compiler's Go backend). The work is mostly verifying
byte-parity against `rust-stemmers` specifically and closing any edge-case gaps, not novel
algorithm design.

## Starting points

- The canonical **Snowball `english` algorithm** (snowballstem.org) — the source of truth
  both `rust-stemmers` and any Go port derive from.
- **`kljensen/snowball`** (Go) or the **Snowball compiler's Go output** — adapt and validate
  byte-parity rather than writing from scratch; do **not** assume parity without the
  word-list diff.
- `rust-stemmers` itself as the reference oracle for generating expected outputs.

## Scope boundary

Only **English** is needed now (`terms.rs` supports `Language::English`). Structure the
package so additional Snowball languages can be added later, but don't port them yet.

## Open questions

- Adapt an existing Go Snowball impl vs port the generated stemmer directly — whichever hits
  byte-parity with `rust-stemmers` fastest.
- Exact `rust-stemmers` / Snowball revision to pin.
- Unicode lowercase edge cases: does `to_lowercase` (Rust) match Go's `unicode`/`strings`
  lowering on all corpus inputs? Part of the conformance corpus.
