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

## Outcome (DONE -- items 1-5; item 6 deferred)

No format/output-byte changes; Rust/Go conformance untouched.

- **Item 1 (`search_filtered`, stable-key/base+delta path)** -- Bloom/facet-prune the splits in memory (unchanged), then search all surviving base splits in one `join_all` wave and all surviving delta splits in a second wave, instead of one serial split open+load per survivor. The tiered short-circuit (base-only Tiered) keeps its early-stop loop -- ranking there is id order, so it can stop opening splits once the page fills, which a blind `join_all` would defeat.
- **Item 2 (`search_split_filtered`)** -- takes a `limit`; the tiered short-circuit passes `limit - out.len()` so a surviving split pages its lazy tail only until it has `remaining` hits rather than materializing its whole (tail-heavy) filtered set with `page(0, usize::MAX)`. The merge/rank callers pass `usize::MAX` (they need the complete set) and keep the explicit `load_tail()`.
- **Item 3 (`facet_counts`)** -- opens every contributing split's `.rrf` in one `join_all` wave (each open is ~MBs of head region), then aggregates in split order so field/category first-seen order stays deterministic.
- **Item 4 (`bm25.rs`)** -- both `search_bm25` and `search_bm25_min_match` fetch every query term's tail in one `join_all` wave on the head-underfill upgrade, instead of one serial RTT per term.
- **Item 5 (`vector.rs::RerankStore::get_many`)** -- replaced the per-id `join_all` with `read_coalesced` (16 KB gap), so the low-doc-id (popular) candidates whose bf16 vectors sit adjacent in the dense array merge into a handful of GETs.

### Item 6 -- DEFERRED (with rationale)

`facet_counts`'s `splits.iter().find(|s| s.contains(gid))` per result id is O(ids x splits). Left as-is: (a) it is a purely in-memory scan off the network-bound critical path -- for a result page (~tens of ids) over even ~780 splits it is microseconds, dwarfed by the `.rrf` opens item 3 now waves; and (b) a `doc_id_lo` binary search is only correct when split ranges are disjoint and sorted, which base splits are but **delta splits (absolute ids) can overlap base ranges** -- a binary search would silently pick the wrong split under supersession. Not worth the correctness risk for microseconds. Revisit only if profiling shows it matters (e.g. huge result pages).

### Tests

- `splitset::tests::tiered_filtered_search_bounded_page_matches_full_and_stops_early` -- new. A multi-split faceted fixture; asserts a small page equals the full page truncated (item 2 bounding drops/reorders nothing) and opens strictly fewer files than the full page (tiered early-stop preserved).
- `splitset::tests::stable_key_filtered_search_wave_returns_all_matches` -- new. Covers the item-1 non-tiered (StableKey) wave path -- previously untested for facet filtering -- asserting the wave returns every match, prunes the category-less split, and searches both splits under an empty filter.
- Items 3/4/5 are mechanical sequential->wave swaps covered by the existing order-sensitive `facet_counts` tests, the bm25 search tests, and the vector rerank tests (all green across features).

### Verification

- Full per-feature `cargo test --lib` matrix green: default (94), vector (109), terms (137), hotcache (103), splits (128), splits+hotcache (140), splits+terms (174).
- `cargo clippy --all-targets --features "splits terms vector hotcache" -- -D warnings` clean; `cargo fmt --check` clean.

### Note

Item 2's intra-split tail bandwidth win needs a split with matches past doc id 65 536 to exercise the tail bound, which a unit fixture can't size to; its correctness reduces to "`page(0, limit)` == `page(0, MAX)` truncated" (guarded by the equivalence assertion above and the existing cursor pagination tests). The `splitset_bench`/live filtered-path latency spot-check is deferred to the live redeploy (tasks 058/059).
