# Task 037 — add min-should-match BM25 search (`searchBm25MinMatch`)

## ✅ DONE (2026-06-18) — shipped v0.18.0 / v0.19.0

`bm25::search_bm25_min_match` (k-way merge over the M head bitmaps, lenient term
resolution) plus the `searchBm25MinMatch` wasm binding on `RrtIndex`, both purely
additive — `search_bm25` (strict AND) is untouched. Unit tests cover the
clamp/superset/subset invariants; `rust/examples/bm25_minmatch_eval.rs` is a
known-item eval (strict vs min2, exact + typo regimes). The consumer (QLL
`roaring-search.js`) and the OpenAlex demo's hybrid arms both use it
(`min_match=2`), made data-driven in the production fusion config.

(Filed from the root `TASK_bm25_minmatch.md`; original spec preserved below.)

---

## Why

`bm25::search_bm25` (src/bm25.rs:297) is strict **AND** — it intersects every
query term's postings, so multi-word queries return ~1–2 docs. That's great
precision for exact matches but contributes almost nothing to multi-word
ranking. A consumer (the QLL catalog) wants a **min-should-match** variant:
keep docs present in **≥ N of the M** query terms (e.g. ≥2 of 4), reranked by
BM25. Offline eval on the consumer shows that, fused as an arm alongside the
trigram-fuzzy arm, ≥2-match beats the current fusion on **every** metric
(recall, R@5, MRR, and false positives) — see "Acceptance" below.

## Scope — additive, do NOT change existing behavior

- **Add** a new method; leave `search_bm25` (strict AND) untouched — OpenAlex and
  other callers depend on it.
- New core fn in `src/bm25.rs`, mirroring `search_bm25`:

  ```rust
  pub async fn search_bm25_min_match<F: RangeFetch, G: RangeFetch>(
      terms: &TermIndex<F>,
      impacts: &ImpactIndex<G>,
      query: &str,
      m: usize,           // candidate window (same role as search_bm25's `m`)
      k: usize,           // top-k to return after rerank
      min_match: usize,   // require ≥ this many distinct query terms
  ) -> Result<Vec<ScoredDoc>, IndexError>
  ```

- New wasm binding on `RrtIndex` mirroring `searchBm25` (src/wasm.rs:1625):

  ```rust
  #[wasm_bindgen(js_name = searchBm25MinMatch)]
  pub async fn search_bm25_min_match(
      &self, impacts: &RrbIndex, query: &str, m: usize, k: usize, min_match: usize,
  ) -> Result<Vec<u32>, JsError>
  ```
  Returns `Vec<u32>` of **local doc ids**, identical contract to `searchBm25`
  (caller maps local→global via the `*.map.json`).

## Behavior

- Resolve the query to per-term head postings (reuse `query_head_postings`, as
  `search_bm25` does). Let M = number of resolved terms.
- **Clamp** `min_match` to `[1, M]`. Then:
  - `min_match == M` ≡ current strict AND.
  - `min_match == 1` ≡ OR (union).
  - `1 < min_match < M` ≡ the new behavior: keep docs present in ≥ `min_match`
    of the term postings.
- Take the first `m` qualifying candidates in **static-rank order** (ascending
  doc id, same as `search_bm25`'s `acc.iter().take(m)`), then rerank the top `k`
  by BM25 via the existing `impacts.rerank(&postings, &candidates, k)`
  (src/bm25.rs:220) — reuse it unchanged.
- Terms missing from the dictionary: skip them (they don't count toward M),
  matching how the rest of the crate treats unresolved terms. If M == 0, return
  empty (like `search_bm25`'s `None` head path).

## Algorithm note (≥N of M roaring bitmaps)

A k-way merge over the M head-bitmap iterators in ascending doc order, tallying
runs of equal doc ids and emitting when the tally reaches `min_match`, yields the
qualifying docs already in static-rank order and allows early-stop once `m` are
collected. (A `HashMap<u32, u16>` count then filter+sort is the simpler
correct fallback; M is tiny — queries are short — so either is fine.) Keep the
head-first / tail-upgrade two-wave structure from `search_bm25`: fetch tails only
if the ≥N head candidate set underfills `m`.

## Acceptance

- Unit: for a 4-term query, `min_match=4` matches `search_bm25`'s result set;
  `min_match=1` equals the union; `min_match=2` is a strict superset of AND and
  subset of OR; single-term query with `min_match=2` clamps to 1.
- Build cleanly under the `terms` feature; `wasm-pack build --target web` exports
  `searchBm25MinMatch` (QLL builds it via its `make roaring-wasm`).
- Consumer (QLL) re-runs its eval and expects, for the fusion arm at
  `min_match=2`: **lexR@10 ≈ 88.9%, R@5 ≈ 86.7%, MRR ≈ 0.742, FP@10 ≈ 0.95,
  FP@20 ≈ 1.55** vs current AND+trigram baseline (87.8% / 84.4% / 0.697 / 1.05 /
  1.70). Exact numbers will shift slightly (the prototype approximated coverage
  from reconstructed index text); the win is the all-metric improvement.

## Reference points

- `src/bm25.rs:297` — `search_bm25` (mirror this; AND-intersection is the part to
  generalize).
- `src/bm25.rs:220` — `ImpactIndex::rerank` (reuse as-is for the BM25 rerank).
- `src/wasm.rs:1625` — `searchBm25` binding (mirror for the new binding).
- `src/terms.rs` — `query_head_postings`, `fetch_tail` (head/tail waves).

## Consumer integration (QLL side, after the wasm ships)

QLL's `site/assets/js/roaring-search.js` will add the new arm to its fusion:
`RRF[ searchBm25MinMatch(en, m=600, k=60, min=2), searchBm25MinMatch(es, …, min=2),
trigram@fuzz2 ]` (likely keeping the existing strict-AND arm too), then re-eval
via `tools/semantic`. No engine knowledge needed there beyond the new method.
