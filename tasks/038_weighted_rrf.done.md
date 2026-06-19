# Task 038 — per-list weights for reciprocal rank fusion

## ✅ DONE (2026-06-18) — lands in v0.20.0 (minor bump)

`reciprocal_rank_fusion_weighted(lists, weights, k_param)` added (`vector.rs`);
`reciprocal_rank_fusion` is now a thin wrapper delegating with all-`1.0` weights,
so the unweighted result is bit-identical. Exported from `lib.rs`. The wasm
`reciprocalRankFusion` gained an optional trailing `weights?: Float64Array` — 2-arg
calls are unchanged, a length mismatch throws a clean error. Unit tests
(unit-weight equivalence, weight reorder, 0.0-drop, fractional/>1) plus a wasm
runtime smoke test all pass. qllpoc tunes the actual weight values its side.
Original spec below.

---

Add **optional per-list weights** to reciprocal rank fusion (core Rust + the wasm
binding), so a consumer can weight some ranked lists more than others. Backward
compatible: omitting weights = today's equal-vote behavior.

## Why

The qllpoc consumer fuses several arms for hybrid book search — per-language BM25
(strict AND), per-language BM25 min-should-match (037), trigram, and a model2vec
**semantic** arm — via `reciprocalRankFusion`. Today every list gets an **equal
vote**, so the single semantic arm is one vote against ~5 lexical-ish votes. For
concept/theme queries the relevant docs are semantic-found, so equal voting
**starves semantic**.

Measured on qllpoc's conceptual gold (recall@10), purely by changing fusion weights
(prototyped with a JS weighted-RRF):

| fusion weights (and / min / trigram / semantic) | recall@10 |
| --- | --- |
| equal (current shipped) | 41% |
| conjunctive ×3, min ×0.5, trigram ×0.5, semantic ×3 | **56–58%** |

≈ **+15 pts** from weighting alone — the biggest single ranking lever found. (Caveat:
that gold is somewhat model2vec-pool-biased, so the *exact* number is inflated; the
*direction* — semantic was badly under-weighted — is robust. Weights are the
consumer's to tune; the library only needs to accept them.)

The consumer wants this **in the library**, not re-implemented in JS, so the wasm RRF
stays the single source of truth for ranking (as it already is for the equal-weight case).

## Current API

```rust
// rust/src/vector.rs:638
pub fn reciprocal_rank_fusion(lists: &[&[u32]], k_param: f64) -> Vec<(u32, f64)>
```
- re-exported in `rust/src/lib.rs`
- wasm binding: `reciprocalRankFusion(lists: Uint32Array[], kParam: number) -> Uint32Array`

Scoring today: a doc at 0-based `rank` in a list contributes `1.0 / (k_param + rank + 1)`,
summed across the lists it appears in; result sorted by score desc.

## Proposed API

Keep the existing fn working; add a weighted variant + thread weights through wasm.

**Core (Rust):**
```rust
/// Weighted RRF: list `i` contributes `weights[i] / (k_param + rank + 1)` per hit.
/// `weights.len()` must equal `lists.len()`.
pub fn reciprocal_rank_fusion_weighted(
    lists: &[&[u32]], weights: &[f64], k_param: f64,
) -> Vec<(u32, f64)>
```
Make the existing `reciprocal_rank_fusion(lists, k)` a thin wrapper calling the weighted
one with `weights = vec![1.0; lists.len()]` (identical behavior, no duplicated logic).

**wasm binding** — optional trailing arg (backward compatible; qllpoc's current 2-arg
calls must keep working unchanged):
```
reciprocalRankFusion(lists: Uint32Array[], kParam: number, weights?: Float64Array | number[])
```
- `weights` omitted/undefined → equal weights (current behavior).
- `weights` present → must be parallel to `lists` (same length); clean error/throw on
  length mismatch, consistent with the module's other input-error reporting.

## Acceptance

1. `reciprocal_rank_fusion(lists, k)` output is **bit-identical** to before (regression).
2. `reciprocal_rank_fusion_weighted(lists, &[1.0; n], k)` == the unweighted result.
3. Weighting changes order as expected — unit test: two docs each alone in one list at
   the same rank; the doc in the higher-weighted list ranks first. A `0.0` weight makes a
   list contribute nothing.
4. wasm: 2-arg call unchanged; 3-arg call applies weights; length mismatch is a clean error.
5. Fractional and `> 1` weights both work (qllpoc uses e.g. `0.5` and `3.0`).

## Consumer usage (qllpoc `roaring-search.js`)

```js
// lists order: [bm25_en(AND), bm25_es(AND), min_en, min_es, trigram, semantic]
const ids = mod.reciprocalRankFusion(
  lists, 60,
  Float64Array.of(3, 3, 0.5, 0.5, 0.5, 3),   // semantic + conjunctive boosted; min/trigram trimmed
);
```
(Exact weights TBD by qllpoc after a fair-pool gold re-judge; the library only needs to
honor whatever weights are passed.)

## Notes

- Native + wasm signatures should stay consistent (cf. 033–035 API-consistency tasks).
- No change to `k_param` semantics or the rank convention — weights are a pure per-list
  multiplier on each hit's existing contribution.
- Purely additive, like 037. Ship under a minor version bump; qllpoc gates on the release.
