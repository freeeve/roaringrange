# Task 039 — expose BM25 scores at the wasm boundary

## ✅ DONE (2026-06-18)

`searchBm25` / `searchBm25MinMatch` now resolve to an `RrtHits { ids, scores }`
struct (mirroring the vector reader's `RrviHits`), exposing the per-hit
`ScoredDoc.score` aligned with `ids`, best-first. Chose the in-place change
(breaking the two bindings) over a parallel `*Scored` method for a cleaner API,
since nothing is released yet. Verified live on real data: `searchBm25('health
research')` → 7 aligned ids+scores, descending (top 10.82 / 10.16 / 9.27);
min-match aligned too. The OpenAlex demo's BM25 arms read `.ids`. Trigram
(`RrsCatalog::search`) left id-only (no relevance score); the native API is
unchanged (already returns `Vec<ScoredDoc>`). The qllpoc consumer updates its
`searchBm25` call to read `.ids` / `.scores` when it adopts linear fusion.

(Original spec below.)

---

`searchBm25` / `searchBm25MinMatch` already compute a BM25 score per hit
(`bm25::ScoredDoc { doc, score: f32 }`, src/bm25.rs:90) but the wasm bindings
**discard it** and resolve to a bare `Uint32Array` of ids (wasm.rs:1634, 1653).
Expose the scores too, aligned with the ids — mirroring how `RrviIndex::search`
already returns `RrviHits { ids, scores }`.

## Why

The qllpoc consumer wants **score-based (linear) fusion** — normalize each arm's
scores and combine as a weighted sum — as an alternative to rank-based RRF (038). It's
the ES `linear`-retriever approach and can beat RRF when score distributions are
informative, and it composes naturally with per-arm weights. Right now it's impossible
from JS for the lexical arms: semantic search returns `scores`, but BM25 returns ids
only, so the consumer can only ever do rank fusion on the lexical side.

The scores already exist in core — this is purely threading them through the binding.

## Current

```rust
// rust/src/wasm.rs:1634 / 1653  (RrtIndex)
#[wasm_bindgen(js_name = searchBm25)]        // -> Uint32Array   (scores dropped)
#[wasm_bindgen(js_name = searchBm25MinMatch)] // -> Uint32Array   (scores dropped)
// core already returns Vec<ScoredDoc { doc: u32, score: f32 }>  (bm25.rs:90, 226)
```

## Proposed

Return an ids+scores struct, parallel to the existing `RrviHits`:

```rust
/// Aligned local doc IDs + BM25 scores, best-first. JS: `ids` Uint32Array, `scores` Float32Array.
#[wasm_bindgen]
pub struct RrtHits { /* ids: Uint32Array, scores: Float32Array via getters */ }
```
- `searchBm25` / `searchBm25MinMatch` resolve to `RrtHits` instead of `Uint32Array`.
- Match `RrviHits`' shape and getter names exactly (`ids`, `scores`) for consistency
  (cf. 033–035 API-consistency work).

**Note:** this is a breaking shape change for the two BM25 bindings (id-array →
hits-struct). Either bump accordingly, or keep `searchBm25` id-only and add
`searchBm25Scored` — your call; the consumer only needs *some* path that returns
aligned ids+scores. Pick whichever keeps the API cleanest.

## Acceptance

1. `searchBm25` / `searchBm25MinMatch` expose per-hit BM25 scores aligned with ids,
   in the same best-first order as today; ids order unchanged.
2. Scores equal the core `ScoredDoc.score` values (no requantization/rounding loss
   beyond f32).
3. Shape/getters match `RrviHits` (`ids`: Uint32Array, `scores`: Float32Array).
4. Trigram (`RrsCatalog::search`) is out of scope — it's static-rank substring matching
   with no relevance score; leave it id-only.

## Consumer usage (qllpoc)

```js
const en = await rt.searchBm25(rb, q, 600, 200);   // { ids, scores }
// linear fusion: minmax-normalize each arm's scores, weighted-sum across arms
// score(doc) = Σ_arm w_arm * norm(arm.scores[doc]);  semantic already gives scores
```

## Notes
- Lower priority than 038 (weighted RRF) — RRF + 038 already gets most of the win; this
  is to test whether *score-based* linear fusion beats rank fusion.
- Native API can stay as-is (it already returns `Vec<ScoredDoc>`); this is wasm-only.
