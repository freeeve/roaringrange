# 056: fix(index): facet excludes silently dropped during tail pagination

**Severity: HIGH (correctness, live in demo).** Line refs @ 849f9c2.

## Problem

`Cursor::ensure` in `rust/src/index.rs:996-1088` has two tail paths that disagree on excludes:

- **Incremental `TailScan` path** (`index.rs:1012-1059`) -- the *default* path (strict AND, `min_match == recs.len()`, seekable postings). It builds the facet constraint from `filter.fields` (includes) ONLY (`index.rs:1014-1025`), and appends each `next_window` result straight into `self.results` (`index.rs:1046`) with no `apply_tail`. `ResolvedFilter.excludes` (`index.rs:686`) is never consulted.
- **Whole-tail fallback** (`index.rs:1064-1087`) -- correctly calls `f.apply_tail(&mut tail_and)` (`index.rs:1081-1083`), which subtracts excludes (`index.rs:744-764`).

## Failure scenario

- A user excluding a category (demo: `searchCursorFiltered` with `[field, category, true]` -> `wasm.rs:610-628` -> `resolve_sels` -> `ResolvedFilter.excludes`) sees excluded docs reappear on every page past the head boundary (doc >= 65536).
- An excludes-ONLY filter (`fields` empty, `excludes` non-empty) produces an empty `facet_fields` vec, so the tail scan runs completely unfiltered.

## Fix direction

Either:
1. Pass excludes into `TailScan` and subtract per window at the same container granularity the includes use (preferred -- keeps incremental paging for exclude filters), or
2. Force the whole-tail path when `!f.excludes.is_empty()` (simple, but loses incremental paging for exclude queries).

Check `posting.rs` `TailScan::open`/`next_window` signatures -- includes are already threaded through as `facet_fields: &[Vec<(u64, usize)>]`; excludes need a parallel flat list, subtracted after the include-AND per window.

## Acceptance

- Test: strict-AND query with an exclude filter, corpus with excluded docs above doc 65536; page past the head; excluded docs must NOT appear. Cover excludes-only filter too.
- Both tail paths (seekable and whole-tail fallback) return identical result sets for the same query+filter.
- Wasm demo path (`searchCursorFiltered` with exclude flag) verified.
- Go reader: check whether the Go cursor (if/where it pages tails with filters) has the same divergence; port the fix + test if so.

## Outcome (DONE)

Fixed via option 1 (thread excludes through the incremental scan, keeps paging):
- `posting.rs`: `TailScan` gained an `excludes: Vec<TailDir>` field; `TailScan::open`
  gained an `exclude_ranges: &[(u64, usize)]` param (opened with the facet fetcher,
  falls back to whole-tail if non-seekable, and NEVER prunes candidate keys —
  an excluded doc removes docs from a bucket, not the whole bucket); `next_window`
  subtracts the exclude union over each window after the include-AND.
- `index.rs`: `Cursor::ensure` builds `exclude_ranges` from `filter.excludes` tails
  and passes them to `TailScan::open`.
- Tests: `posting::tests::tail_scan_with_excludes_matches_filtered_andnot` +
  `tail_scan_excludes_only` (unit, direct on TailScan), and
  `build_tests::cursor_tail_pagination_applies_excludes` (end-to-end through
  `Cursor::ensure`). All three proven to FAIL without the fix.

⚠ NOTE surfaced during the fix: two catalog tests
(`catalog::tests::include_and_exclude_combine`, `multiple_excludes_union`) were
**asserting the buggy behavior** — their fixture's `es` category = `{4,5,tail}`
(includes the tail doc) but they expected the excluded tail doc to survive, with
comments abbreviating `es` as `{4,5}` to rationalize it. Corrected both to
`vec![1, 3]` with accurate comments. These tests were a second witness to the bug.

Go reader has no filtered/cursor/exclude path (writer + minimal `Posting`/`Get`
reader only), so no Go port needed. Reader-side only; no format/output-byte
changes; all 91 lib tests + integration + clippy clean.
