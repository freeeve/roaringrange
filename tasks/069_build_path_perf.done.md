# 069: perf(build): build-path costs at 484M-doc scale

**Severity: LOW-MED (multi-hour builds; GC/syscall pressure).** No output-byte changes. Line refs @ 849f9c2.

## Findings

1. **`ngram.go:31, 51` -- per-call allocations in doc-ingest.** `NgramKeysWith` allocates a fresh `map[uint64]struct{}` per call and `normalize` a fresh `[]rune` per word; runs once per document via `SplitSetBuilder.AddText` (`splitsetbuild.go:122`) / `TrigramMonolithBuilder.AddText` (`monolithbuild.go:89`) -- hundreds of millions of map+slice allocations per full build. Add a reusable-state variant (or slice-based dedupe for the typical few-dozen keys); keep the existing API as a wrapper.
2. **Unbuffered tiny writes in library writers.** `records.go:64-76` (`RecordWriter.Write`): one `bin.Write` + one 8-byte `idx.Write` per record ~= 2 syscalls/record when handed `*os.File`s (~1B syscalls at corpus scale). Same class: `lookup.go:101-109` (16 B/entry), `bm25.go:182-190` (20 B/term), `splitset.go:184-203` (56 B/split). The examples wrap bufio (`main.go:296-297`) but the library neither buffers internally nor documents the expectation. Either wrap internally (flush on Close -- pairs with task 067 item 4) or document "pass buffered writers" loudly on every writer.
3. **`rust/src/splitset_build.rs:938-961` (`TermSplitSetBuilder::add_faceted`) walks the postings BTreeMap 3x per term** (`contains_key` marginal check, `contains_key` as `is_new`, then `entry`); the trigram twin (:604-617) does two. Restructure around a single `entry` per (doc, term).
4. **`bm25.rs:483-597` (`ImpactsAccumulator`/`write_impacts`) is whole-corpus-resident** (`BTreeMap<String, Vec<(u32,u32)>>` + full impacts Vec = tens of GB at 484M docs). KNOWN -- this is the task-032 remainder ("full-corpus chunked builder"); listed for completeness, do it under 032's plan.

## Acceptance

- Byte-identical artifacts (goldens + conformance) -- these are pure alloc/syscall reductions.
- Ingest micro-bench (b.ReportAllocs) before/after for NgramKeysWith path; note numbers in the commit.

## Outcome (DONE -- items 1-3; item 4 stays under task 032)

No format/output-byte changes; goldens + conformance unchanged.

- **Item 1 (`ngram.go`)** -- added `NgramKeyer`, a reusable-scratch keyer (dedup map + rune buffer + key buffer), used per-document by `TrigramMonolithBuilder.AddText` and `SplitSetBuilder.AddText`/`AddFaceted` (both consume the keys immediately, so the aliased buffer is safe). `NgramKeysWith` is now a thin wrapper over a fresh keyer (identical behavior/output). Also switched the hot loop from `strings.Fields` to `strings.FieldsSeq` (no `[]string` alloc) and removed the now-dead `normalize`. **Micro-bench (`BenchmarkNgramKeys`, 60-key doc): fresh path 1688 B/op, 16 allocs/op -> reused keyer 0 B/op, 0 allocs/op** -- every per-document ingest allocation eliminated.
- **Item 2 (`records.go`, `lookup.go`, `bm25.go`, `splitset.go`)** -- documented the buffered-writer expectation loudly on `NewRecordWriter`, `WriteLookup`, `WriteImpacts`, `WriteSplitSet` (each emits many small per-element writes; pass a `bufio.Writer` for a file/socket and flush after). Chose documentation over internal buffering deliberately: the example builders already wrap `bufio`, so internal buffering would double-buffer, and it would force a flush-on-close API change.
- **Item 3 (`rust/src/splitset_build.rs` `add_faceted`)** -- the per-term insert loop did `contains_key` (is_new) then `entry` (2 walks); collapsed to a single `entry` match that yields both the posting and the vacancy flag, so `add_faceted` now walks the postings map twice per term (marginal estimate + insert), matching the trigram twin. The pre-seal marginal `contains_key` is intrinsic (it decides sealing before `seal()` clears the map), so it stays.
- **Item 4** -- `bm25.rs` whole-corpus-resident accumulator is the task-032 "full-corpus chunked builder" remainder; left for 032's plan as noted.

### Tests

- `TestNgramKeyerMatchesWrapper` (new) -- the keyer matches `NgramKeysWith` byte-for-byte across a mix of queries and across successive calls (buffer-reset correctness), for both case-fold modes.
- Item 3 covered by the existing splitset build goldens/tests (byte-identical).

### Verification

- Go: `gofmt -s` clean, `go vet` clean, root + conformance + examples/openalex modules green.
- Rust: `cargo test --lib --features "splits terms"` green (176); `cargo fmt --check` clean; `cargo clippy --all-targets --features "splits terms"` clean.
