# 063: perf(terms): cap + batch search_prefix; avoid full-posting clones in and_locs

**Severity: MED.** `rust/src/terms.rs`, wasm-exposed (browser pays these). Line refs @ 849f9c2.

## Findings

1. **MED -- `terms.rs:736-772, 779-824` (`search_prefix`): unbounded fan-out and serial block scan.** `prefix_locs` scans with `max = usize::MAX`, fetching candidate dict blocks ONE AT A TIME sequentially (`scan_prefix`, :800), then `union_locs` issues one `head_block` fetch per matching term in a single UNCAPPED `join_all`. A short prefix over the 187M-term corpus = thousands of sequential block reads followed by an unbounded concurrent head fan-out in the browser. Fix: (a) cap matched locs like `complete`'s `max_terms` does (surface "N+ truncated" the way the demo already handles depth-capped totals, cf. task 052); (b) batch/prefetch candidate dict blocks in waves instead of one-by-one.
2. **LOW -- `terms.rs:567, 587` (`and_locs`): `blocks[0].head.clone()` / `fulls[0].clone()`** -- the full-posting clone can be multi-MB for a common term; `swap_remove(0)`/`into_iter` avoids it. Same shape as the (correct) `intersect()` in `index.rs:461`.

## Acceptance

- A 1-2 char prefix against a large fixture completes with a bounded number of block fetches and a bounded head fan-out (assert via counting fetcher).
- Results for non-truncated prefixes unchanged; truncated case clearly signaled to callers (wasm surface included).
