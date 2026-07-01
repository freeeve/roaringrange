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
