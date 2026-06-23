# Task 051 — Go trigram `.rrs` monolith writer — DONE

**Status:** DONE (2026-06-23). Shipped `go/monolithbuild.go` (`WriteIndex` +
`TrigramMonolithBuilder`), the `gen_rrs_monolith_golden` example +
`go/testdata/rrs_monolith_build_golden.txt`, the Go conformance + readback tests
(`go/monolithbuild_test.go`), and the Rust drift-guard
(`build_tests::rrs_monolith_golden_matches`). The Go monolith is byte-for-byte with the
Rust `write_index` path and reads back through the RRS reader with correct doc-ID alignment
(the empty-doc case verified).

Split out of task 050 (it was mis-bundled with the vector trainer — unrelated). Unlike
the trainer, this is a deterministic **byte-exact** serializer in the same family as the
045–049 build-side ports, and Go already has every primitive:

- `NgramKeys` (`go/ngram.go`) — byte-exact trigram keys vs Rust `ngram_keys`;
- `roaring.Bitmap.ToBytes()` — portable posting bytes (matches Rust `serialize_posting` /
  `serialize_into`, proven transitively by the RRSS split golden);
- `writeIndex` (`go/transcode.go`) — the v3 `RRSI` layout (header / sparse index /
  20-byte dict / postings), already the byte-for-byte mirror of Rust `build::write_index`.

So a monolith = the split-set builder's `seal()` over the whole corpus with **no byte-cap
tiering** — one ordinary v3 `RRS` over all docs. The Rust counterpart is
`build::write_index` (primitive) driven by the `build_trigram_monolith` example (the
chunked partial→merge path for 100+ GB; the Go writer is the simple in-memory equivalent
for small/medium corpora).

## Scope

1. `go/monolithbuild.go`:
   - `WriteIndex(dst, gramSize, stride, []IndexEntry)` — public mirror of Rust
     `build::write_index`: sorts entries by key, guards `stride <= 0 → DefaultStride`,
     wraps the existing private `writeIndex`.
   - `TrigramMonolithBuilder` — `AddText`/`AddKeys` (ascending doc IDs; an empty doc still
     consumes an id so the doc-ID space stays dense), `DocCount`, `Write` (seal all
     postings to one RRSI via `WriteIndex`).
2. Conformance (the 045–049 pattern): `gen_rrs_monolith_golden` Rust example over a fixed
   corpus → `go/testdata/rrs_monolith_build_golden.txt`; `go/monolithbuild_test.go` builds
   the same corpus via the builder and asserts `== golden`; Rust `build_tests` drift-guard
   asserts the example still matches the committed golden.

## Out of scope

- The chunked partial→merge path (resumable 100+ GB build). The in-memory builder holds all
  postings at once — fine for small/medium corpora; for the full OpenAlex monolith the
  Rust chunked builder remains the tool.
- Facet sidecar / records / `.rril` — unchanged; the monolith only produces the `.rrs`.
