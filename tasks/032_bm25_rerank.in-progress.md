# 032 — Pragmatic BM25: impact-sidecar rerank for term + hybrid search

Add BM25-style lexical relevance as a **purely additive** sidecar — zero
modification to any existing file or format. Existing `.rrt`/`.rrs`/`.rrf`/
`.rrvi`/records artifacts stay byte-identical; readers that don't opt in never
fetch the new bytes. This is the Weaviate-hybrid shape: BM25 lexical arm +
vector arm fused with the crate's existing `reciprocal_rank_fusion`.

## Design (Tier 1 — rerank, not a WAND engine)

The intersection already returns candidates in citation-rank order (doc ID ==
rank). Take the first M hits (1k–5k), fetch per-term impact values for just
those docs plus their length norms, score, reorder, return top K. Per-query
cost: a few KB–MB, consistent with the existing budget.

New artifacts (one `.rrb` impact sidecar per term index + one norms file):

- **Impacts**: for each term posting, a quantized impact byte (u8, Lucene-style)
  per (term, doc) pair, laid out in the posting's iteration order and
  **container-aligned** so the existing container-granular subset reads fetch
  only the candidate docs' blocks. Field weighting (title vs abstract, BM25F)
  is folded into the byte **at build time** — no multi-field tf streams.
- **Norms**: 1 byte/doc quantized doc-length, 484 MB flat, range-fetched per
  candidate window (or folded into the impact byte entirely — decide during
  format freeze; folding makes the sidecar self-contained and skips the norms
  fetch at the cost of pure-BM25 fidelity).
- **IDF**: free — `posting_cardinality` already reads df from the roaring
  descriptive header. No new storage.

Sizing (484M docs, title+abstract, ~80–120 unique terms/doc → ~40–60 B pairs):
~50 GB at u8, ~15–20 GB bit-packed + per-block zstd. Same order as the 53.8 GB
`.rrt` itself.

## Steps

- [ ] Freeze `.rrb` format (`RRSB` magic): header + per-term offset table keyed
      by the `.rrt` dictionary's term ordinals + container-aligned impact blocks.
- [ ] Builder: one more pass over records-full emitting impacts + norms
      (embarrassingly parallel like the other builds; reuse the tokenizer +
      stemmer from `terms_build`).
- [ ] Reader: `TermIndex::score_candidates(ids, terms, k)` — fetch impact
      subsets + norms for the candidate window, BM25 score, reorder. Wasm
      binding + a "relevance rerank" toggle in the demo.
- [ ] Hybrid: fuse the reranked lexical list with the semantic list via
      `reciprocal_rank_fusion` (exists) — this is the Weaviate-parity mode.
- [ ] Bench row in `live_bench`: bytes/latency of rerank vs rank-order-only,
      plus a relevance spot-check (the "roaring bitmaps" seminal-paper test).

## Non-goals (Tier 2, separate decision)

Global top-k BM25 (Block-Max WAND) — per-container max-impact metadata + a
skip-scan evaluator. Only worth it if rerank's top-M window demonstrably misses
relevant low-rank docs that hybrid's semantic arm doesn't recover.

Known limitation to document: a low-citation doc with high lexical relevance
outside the top-M candidate window is invisible to the reranker.
