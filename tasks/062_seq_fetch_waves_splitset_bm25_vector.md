# 062: perf(splitset/bm25/vector): batch fetch waves + stop over-materializing

**Severity: HIGH (filtered split-set search) / MED.** Same theme as task 061 but in splitset.rs, bm25.rs, vector.rs. Line refs @ 849f9c2.

## Findings

1. **HIGH -- `splitset.rs:696-713` (`search_filtered`, stable-key/base+delta path): splits searched strictly sequentially.** Each iteration awaits `search_split_filtered` (itself several RTTs: `.rrf` meta open + split open + tail load + page) before starting the next split. The unfiltered equivalent `search_all` (:925-946) already does one `join_all` wave. A filtered query over the live ~19-split geo set pays ~20x serial S3 latency instead of ~1 wave. NOTE: the tiered short-circuit loop (:682-690) intentionally stops early once `limit` is filled -- preserve that semantics (e.g. wave per tier, or bounded-concurrency racing), don't blindly join_all every split.
2. **MED -- `splitset.rs:1058-1061` (`search_split_filtered`): always materializes the split's ENTIRE filtered result** (`cursor.page(0, usize::MAX)` after `load_tail()`), even when the tiered loop only needs `limit - out.len()` more hits. A broad query + facet filter loads and pages every match (tail postings included) per surviving split. Pass `remaining` down like the unfiltered tiered loop does.
3. **MED -- `splitset.rs:769-777` (`facet_counts`): opens each contributing split's `FacetIndex` sequentially with the eager full open** (the comment at :1036-1040 notes that open costs ~MBs per split) -- paid serially per split on every facet render. One concurrent wave over contributing splits.
4. **MED -- `bm25.rs:342-348` and `bm25.rs:451-457`: tail postings fetched one-per-term sequentially** in both `search_bm25` and `search_bm25_min_match` (`for (off, b) in heads { terms.fetch_tail(...).await? }`). M query terms = M serial RTTs on exactly the path the head-first design flags as expensive. One `join_all` wave.
5. **MED-LOW -- `vector.rs:627-647` (`RerankStore::get_many`): one ranged GET per candidate id** (e.g. 50 x ~1.5 KB) via plain `join_all` instead of `read_coalesced`. Candidates skew toward low (popular) doc ids sitting adjacent in the dense bf16 array, so the existing 16 KB-gap coalescer would merge many into a handful of GETs. The helper already exists in `fetch.rs`.
6. **LOW -- `splitset.rs:754-764` (`facet_counts`): linear scan over all splits per result id** (`splits.iter().find(|s| s.contains(gid))`), O(ids x splits). Base splits are sorted by `doc_id_lo` -> binary search or one merge pass over sorted ids.

## Acceptance

- Fetch-count instrumentation before/after: filtered geo-split query (expect ~wave-count, not ~20x serial), M-term BM25 query (1 tail wave), 50-candidate rerank (coalesced GET count).
- Filtered tiered search returns the SAME hits in the same order as before (early-stop semantics preserved) -- cover with a multi-tier fixture test.
- `splitset_bench` / live demo spot-check on the filtered path.
