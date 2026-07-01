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
