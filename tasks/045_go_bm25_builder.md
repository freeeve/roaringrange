# Task 045 — Go RRSB (`.rrb`) BM25 impact-sidecar builder

Port the Rust `bm25` builder to `go/`, byte-for-byte with `write_impacts`, so the
Go build-side can emit the BM25 sidecar that pairs with the `.rrt` term index Go
already writes (`WriteTermIndex`). First of the Go build-side gaps (see 046–050).

## Rust source of truth
- `rust/src/bm25.rs`: `quantize_impact` (the float-exact impact math), the module
  doc (frozen `RRSB` layout), and the `build` submodule's `ImpactsAccumulator` +
  `write_impacts`.

## Format (frozen, little-endian)
64-B header (magic `RRSB`, version 1, flags, `scale`=k1+1 f32, k1/b/avgdl f32,
term_count u32, sparse_stride u32, entries_off u64, impacts_off u64, doc_count u64,
8 reserved) → sparse index (every `stride`-th entry's head_off, u64) → entries
(term_count × 20 B: head_off u64, impacts_rel u64, card u32, ascending head_off) →
impacts (per term, `card` bytes in ascending posting-doc order).

## Port

`go/bm25.go`:
- `QuantizeImpact(tf uint32, dl, avgdl, k1, b float32) byte` — `s = tf·(k1+1)/(tf +
  k1·(1−b+b·dl/avgdl))`, `byte = clamp(round(s·255/(k1+1)), 1, 255)`. Do the
  arithmetic in **float32** and round half-away-from-zero to match Rust's
  `f32::round` exactly (the one conformance gotcha — same class as the fst/stemmer
  ports). Verify ties.
- `ImpactsAccumulator` over the shared `TermTokenizer`: `AddDoc(text)` →
  doc_len = len(tokens); per-term tf (sorted), append `(doc, tf)` per term; track
  `docLens`. Docs added in ascending doc-ID order (per-term lists ascending by
  construction).
- `WriteImpacts(dst io.Writer, dict []DictEntry{Term string; HeadOff uint64}, acc,
  k1, b float32) error` — mirror `write_impacts`: validate ascending head_off, each
  dict term has stats (else error), quantize per (doc,tf), emit header + sparse +
  entries + impacts. `avgdl = Σ docLens / n`, `scale = k1+1`.

## Conformance
- `go/testdata/rrsb_build_golden.txt` (`name <hex>`), generated from the Rust
  `write_impacts` over a fixed corpus + dict + (k1,b) and **asserted by both** a new
  Rust test and `go/bm25_test.go` (the shared-golden pattern used by
  `rrss_build_golden.txt`). Isolate the builder by passing an explicit dict (term →
  ascending head_off over the corpus's distinct tokenized terms), so the test does
  not depend on the `.rrt` build.

## Acceptance
- `go test ./...` passes; `go/bm25_test.go` matches the golden byte-for-byte.
- A Rust test asserts the same golden (both sides pinned).
- `gofmt -s` clean.
