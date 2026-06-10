# 025 — Rarest-first intersection with container probing

**Status:** pending — **RE-SCOPED 2026-06-10**: the v3 reader already implements most of
this. `posting.rs::tail_intersect_and` seeds from the smallest tail posting and probes the
larger ones at container granularity (smallest-first, span-coalesced), and head reads are
bounded eager prefixes (16 buckets/term), not whole postings. What remains:

1. **Measure** the live per-query bytes for dense queries (perf bar / `live_bench`) to see
   how far the current path already is from the naive whole-posting cost — the task's
   original ≥5× target may already be met.
2. Audit the **cursor** path (`search_cursor*` / `TailScan`) for the same property — the
   one-shot `search` uses `tail_intersect_and`, but paged tail loads may differ.
3. The eager prefixes are still fetched for every term including the densest (~128 KB/term
   worst case — modest); skipping the densest term's prefix when the others' intersection
   is already tiny is the only remaining fetch to shave.
4. Fuzzy (max_missing > 0) phase as originally scoped.

The principled version of the shelved "candidates" experiment (see README §Tried and
shelved): cut a dense query's egress by never fetching the common trigrams' postings whole.

## Design

Today `Index::search`/cursor fetches every query trigram's posting (rarest and densest
alike) and ANDs. Instead:

1. Dict lookups (already done) give every posting's size. Sort keys by posting size.
2. Fetch the K smallest postings whole (K≈2–3) and AND them → a candidate bitmap.
3. For each remaining (large) posting, **probe** it at container granularity (offset table →
   fetch only the containers the surviving candidates occupy → AND within container),
   re-shrinking the candidate set after each posting, cheapest-remaining first.
4. Stop early when candidates = 0. Fall back to whole-posting fetch when the candidate
   container fan-out × ~8 KB approaches the posting size (the probe must never cost more
   than the fetch it replaces).

Unlike record-verification, this never touches the record store, so it does NOT hit the
result-set-size floor that killed the original experiment.

## Interactions

- Shares the container-probe machinery with task 024 — build that first.
- Fuzzy (max_missing > 0) needs the threshold count per doc, not pure AND: probing still
  works (each posting contributes presence per candidate) but the early-exit changes
  (a candidate survives until it can no longer reach min_match). Phase 2; strict AND first.
- The head/eager prefix optimisation stays: page-1 often needs only the eager prefix.

## Acceptance

- "machine learning" (484M): measure bytes before/after via the perf bar / `live_bench` —
  target ≥ 5× reduction on dense multi-word queries.
- Results byte-identical to the current path (differential test over synthetic corpora +
  a sample of live queries).
