# Task 011 — Byte-exact Go port of `rust-stemmers` (Snowball English / Porter2)

**Status:** DONE — stemmer spun out to a standalone repo, and the roaringrange-side
wiring is complete and shipped (see below). File closed.

## Closed (2026-06-23)

The roaringrange-side wiring the 2026-06-07 "Still TODO" section flagged as open is
all present in `go/terms.go`:
- depends on + imports `github.com/freeeve/go-stemmers` (the stemmer) and
  `github.com/freeeve/fst-go` (the block-dict FST router);
- `WriteTermIndex` — the monolithic RRTI `.rrt` Go writer;
- `TermTokenize` / `TermTokenizer` — the ported Tantivy SimpleTokenizer + LowerCaser
  + stopwords + stemmer.

The build-time↔query-time stemming invariant (this task's whole reason to exist) is
pinned byte-for-byte to the Rust pipeline by the committed `rrti_term_split_golden.txt`
conformance golden (tokenize→stem→stopwords→postings→`.rrt`), asserted in
`go/termsplitsetbuild_test.go` and `go/bm25_test.go`. The original outcome notes below
are retained for history.

## Outcome (2026-06-06)

The stemmer port was spun out to a **standalone repo** — `github.com/freeeve/go-stemmers`
(`~/go-stemmers`, MIT) — mirroring the task-010 `~/fst-go` pattern, and the user widened
scope to a **full port of the rust-stemmers crate (all 18 Snowball algorithms)**, not just
English. Output is byte-identical to rust-stemmers 1.2.0, verified by:

- `TestVectors` — committed `voc_*`/`res_*` for all 18 languages (rust-stemmers' own 12 +
  snowball-data vocabularies with `rustgen`-generated goldens for the 6 it ships none for).
- `TestRustgenParity` — live differential vs the `rustgen` oracle over a 60k-term OpenAlex
  corpus (`cmd/oacorpus`), all 18 languages.
- `FuzzStem` — 30s, no panics / no UTF-8 corruption.

Correctness is **output-string parity**, not fst-style serialized bytes (a stemmer has no
on-disk format). Perf pass: Env pooling + in-place slice edits + skip-copy-when-unmodified →
English 4→1 allocs/op. Layout: `internal/snowball` runtime, `internal/<lang>` per algorithm,
`stemmers.go` API, `rustgen/` oracle, `cmd/oacorpus/`.

### Still TODO (roaringrange-side, here)

Verified open as of 2026-06-07: `go/go.mod` has no go-stemmers dependency, there is no
RRTI `.rrt` Go writer in `go/`, and the `terms.rs` tokenizer/`STOP_WORDS` are not ported
(the only `tokenize` in `go/` is the unrelated n-gram tokenizer). Dependency path is
**`github.com/freeeve/go-stemmers`** (plural; package `stemmers`; `Algorithm`/`New`/`Stemmer`
API mirroring rust-stemmers) — the singular `go-stemmer` does not exist.

1. Depend on `github.com/freeeve/go-stemmers` from the roaringrange `go/` module and wire it
   into the RRTI `.rrt` Go writer (task 011's original purpose: a Go builder that stems
   exactly as `terms.rs`).
2. Port `terms.rs::tokenize` (Tantivy SimpleTokenizer + LowerCaser; `char::is_alphanumeric`
   + `char::to_lowercase`) and the `STOP_WORDS` list to Go, with conformance against the
   Rust side (the original scope items 2 & 3 below). Watch the `to_lowercase` full-mapping
   vs Go `unicode.ToLower` edge cases.

---

(original scoping doc follows)

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
