# 063: perf(terms): cap + batch search_prefix; avoid full-posting clones in and_locs

**Severity: MED.** `rust/src/terms.rs`, wasm-exposed (browser pays these). Line refs @ 849f9c2.

## Findings

1. **MED -- `terms.rs:736-772, 779-824` (`search_prefix`): unbounded fan-out and serial block scan.** `prefix_locs` scans with `max = usize::MAX`, fetching candidate dict blocks ONE AT A TIME sequentially (`scan_prefix`, :800), then `union_locs` issues one `head_block` fetch per matching term in a single UNCAPPED `join_all`. A short prefix over the 187M-term corpus = thousands of sequential block reads followed by an unbounded concurrent head fan-out in the browser. Fix: (a) cap matched locs like `complete`'s `max_terms` does (surface "N+ truncated" the way the demo already handles depth-capped totals, cf. task 052); (b) batch/prefetch candidate dict blocks in waves instead of one-by-one.
2. **LOW -- `terms.rs:567, 587` (`and_locs`): `blocks[0].head.clone()` / `fulls[0].clone()`** -- the full-posting clone can be multi-MB for a common term; `swap_remove(0)`/`into_iter` avoids it. Same shape as the (correct) `intersect()` in `index.rs:461`.

## Acceptance

- A 1-2 char prefix against a large fixture completes with a bounded number of block fetches and a bounded head fan-out (assert via counting fetcher).
- Results for non-truncated prefixes unchanged; truncated case clearly signaled to callers (wasm surface included).

## Outcome (DONE)

No format/output-byte changes; Rust/Go conformance untouched.

- **Item 1(a) cap the fan-out** -- `prefix_locs` now caps matched terms at `MAX_PREFIX_TERMS` (2048) and returns `(locs, truncated)`, scanning one past the cap to distinguish "exactly the cap" from "more". This bounds the concurrent head fan-out to at most the cap (was one head fetch per matching term -- effectively unbounded for a 1-char prefix over a 187M-term vocabulary).
- **Item 1(a) signal truncation** -- new public `TermIndex::search_prefix_capped(prefix, limit) -> (Vec<u32>, bool)`; `search_prefix` is now a thin wrapper returning `.0` (bounded, but unchanged results for any prefix within the cap). New wasm `RrtIndex::searchPrefixCapped` returns a `PrefixSearch { ids, truncated }` struct mirroring the existing `CountEstimate` pattern, so a UI can flag a bounded result as "showing partial matches". `searchPrefix` is unchanged (back-compat).
- **Item 1(b) batch dict blocks** -- `scan_prefix` fetches candidate dict blocks in concurrent waves of `PREFIX_BLOCK_WAVE` (8) rather than one blocking round trip per block; the sorted early-stop and the `max` cap still end the scan after at most one over-fetched wave. `complete` (autocomplete) shares `scan_prefix`, so it gets the same RTT reduction with identical results.
- **Item 2 avoid the multi-MB clone** -- `and_locs`'s tail path now takes ownership of the smallest full posting via `into_iter().next()` instead of `fulls[0].clone()` (the full posting can be multi-MB for a common term; the vec is not reused). The head-path `blocks[0].head.clone()` is left as-is: heads are bounded by the head boundary (not multi-MB) and `blocks` is reused on the tail path, so it cannot be cheaply consumed.

### Tests

- `terms::tests::search_prefix_caps_fanout_and_reports_truncation` -- new. A vocabulary just over the cap (all sharing prefix "aa", doc id == rank) over a `CountingFetch`; asserts `prefix_locs` caps at `MAX_PREFIX_TERMS` and flags `truncated`, the `union_locs` head fan-out is `<= MAX_PREFIX_TERMS` reads (not the full vocabulary), and the ranked top-10 docs are correct.
- `terms::tests::search_prefix_within_cap_is_exact_and_untruncated` -- new. A within-cap prefix returns the exact union and `truncated == false`.
- Batching correctness (item 1b) is covered by the existing `search_prefix_unions_matching_terms` / `search_prefix_spans_head_and_tail` / `complete` tests (unchanged results) plus the terms fuzz harness (`fuzz_rrti_terms_no_panic`), all green.

### Verification

- `cargo test --lib` matrix green: default (94), terms (139), vector (109), splits+terms (176), terms+hotcache (148); terms fuzz suite green.
- `cargo clippy --all-targets --features "splits terms vector hotcache" -- -D warnings` clean; wasm surface compiles and lints clean under `--features "wasm terms vector"` (the pre-existing unrelated `Float64Array` unused-import warning is not from this task); `cargo fmt --check` clean.

### Note

`MAX_PREFIX_TERMS` (2048) bounds the fan-out but is not a concurrency throttle -- the union still issues up to that many head fetches in one wave. If browser connection-pool pressure proves it too wide, a bounded-concurrency chunking of `union_locs`'s head wave is the follow-up (the prefix-adjacent heads are contiguous in the file -- dictionary-order layout -- so a `read_coalesced`-style merge is also possible). Deferred as it is beyond the stated finding.
