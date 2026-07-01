# 061: perf(rrs): batch sequential fetch waves in the monolith reader

**Severity: HIGH (interactive query latency; each fetch is an S3/HTTP round trip).** The codebase's own doctrine is "one join_all wave per stage" (see `fetch_head_prefixes`); these paths violate it. Line refs @ 849f9c2.

## Findings

1. **HIGH -- `posting.rs:329-347` (`open_tail_dirs`): tail-posting headers fetched sequentially.** The `for` loop awaits each posting's ~4 KB header (sometimes twice) one at a time. Called by `TailScan::open` on a cursor's first tail access, for every trigram PLUS every facet category: a 10-trigram query with a 2-category filter pays ~12 serial RTTs (~1.2 s at 100 ms RTT) before the first tail window -- in the interactive demo path. Reads are fully independent; make it one `join_all` wave like `fetch_head_prefixes`.
2. **MED -- `index.rs:371-385, 407-421` (`query_cost`, `count_estimate`): per-key `lookup()` with no dict-block dedup.** Multiple n-grams routinely share a dict block; these issue duplicate CONCURRENT range reads for the same `(offset, len)` -- wasted RTTs plus the exact duplicate-in-flight-Range hazard `read_dict_blocks` (`index.rs:313-320`) exists to prevent (some HTTP caches answer truncated). Both are wasm-exposed and called per keystroke. Route through `read_dict_blocks`.
3. **MED -- `index.rs:569-573` (`search_candidates`): the k rarest postings fetched serially** (`await` in a `for` loop) -- k extra sequential RTTs. One `join_all` wave.
4. **LOW -- `index.rs:778-790` (`full_bitmap`): four sequential waves** (include-head, include-tail, exclude-head, exclude-tail) that are mutually independent -- 4 RTTs where 1 would do, on the vector-filter path.
5. **LOW -- `index.rs:521-539` (`search` wave 3): `tail_intersect_and` re-reads the rarest posting in full**, including bucket-0 bytes wave 2's eager prefix already fetched (plus re-reading posting headers). Redundant egress whenever the head under-fills `limit`; only the wasm range cache hides it -- native callers pay twice. Thread the already-fetched prefix through, or start the tail range past the eager bound.

## Acceptance

- Count fetches (instrument or a counting `RangeFetch` in tests) before/after for: (a) 10-trigram + 2-facet-category cursor first tail access, (b) `query_cost` on a query whose keys share dict blocks, (c) `search_candidates`. Expect wave counts, not per-item counts.
- No result changes: existing search/cursor tests byte-identical results.
- `live_bench` / demo spot-check for the tail-scan first-page latency win.

## Outcome (DONE)

All five findings fixed; no format/output-byte changes, so Rust/Go conformance is untouched.

- **Item 1 (`open_tail_dirs`)** -- the per-posting header loop is now a single `join_all` wave for the prefixes plus a second `join_all` wave for the (rare) oversized-header re-reads. A 12-posting filtered tail-open pays 1--2 round-trip waves, not ~12 serial RTTs.
- **Item 2 (`query_cost`, `count_estimate`)** -- both now route their keys through a new `lookup_many` helper that dedups by dict-block byte offset via `read_dict_blocks`, so keys sharing a block cost one read (not one per key) and never issue duplicate in-flight Range requests.
- **Item 3 (`search_candidates`)** -- the k-rarest posting fetch is one `join_all` wave instead of an `await`-in-`for` loop.
- **Item 4 (`full_bitmap`)** -- the four independent include/exclude head/tail sub-queries run as one `futures::future::join` instead of four sequential awaits.
- **Item 5 (`search` wave 3)** -- wave 3 now opens a seekable `TailScan` starting at `EAGER_BUCKETS` (the same proven path the cursor uses) instead of `tail_intersect_and`, which read the rarest posting in full from bucket 0. Docs `< EAGER_DOC_BOUND` were already fetched and intersected by wave 2, so this drops the duplicate head-container egress that only the wasm range cache was hiding. A non-seekable posting falls back to the old whole-posting strict AND.

### Tests

- `posting.rs::open_tail_dirs_reads_headers_concurrently` -- new. A peak-in-flight-counting `RangeFetch` (yield-once futures so concurrent reads interleave under the single-threaded test executor) proves the header prefixes overlap (`max_inflight >= 2`). Verified it fails when the wave is reverted to a sequential loop.
- `build_tests.rs::query_cost_dedups_shared_dict_block` -- new. An instrumented fetch (reset after `open`) confirms a 2-key shared-block query costs exactly one dict read.
- `build_tests.rs::search_candidates_fetches_postings_concurrently` -- new. Confirms the posting wave overlaps (`max_inflight >= 2`) and the candidate set is unchanged.
- `build_tests.rs::cursor_tail_pagination_applies_excludes` -- (from the exclude work) still green.
- Item 5 result-correctness is covered by the existing `search_and_with_tail_intersection` and `posting_spans_buckets_and_search_pages_in_order` wave-3 tests plus the independent `tail_scan_matches_full_and` (`TailScan == tail_intersect_and`) equivalence test; the change is a mechanical reuse of the cursor's already-tested `EAGER_BUCKETS` scan path.

### Verification

- Full per-feature `cargo test --lib` matrix green: default (94), vector (109), terms (137), hotcache (103), splits (126), splits+hotcache (138), splits+terms (172).
- `cargo clippy --all-targets --features "splits terms vector hotcache" -- -D warnings` clean; `cargo fmt --check` clean.

### Note

The cursor-level tail path (`Cursor::ensure` -> `TailScan::open`) was already concurrent inside `next_window`, so items 1 and 3 are isolated by dedicated posting-/candidate-level concurrency tests rather than a cursor test. `live_bench`/demo latency spot-check deferred to the live redeploy (tracked with tasks 058/059).
