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

- [x] Freeze `.rrb` format (`RRSB` magic) — `src/bm25.rs` module doc. Design
      deltas from the original sketch: keyed by each term's posting-region
      `head_off` (the blocked dictionary has no cheap ordinals; head_off is
      unique, ascending in dict order, and the term search already resolves it),
      with an RRS-style resident sparse index (stride 512) over a sorted 20-byte
      entry table. Norms FOLDED into the impact byte at build time — no separate
      norms file, no extra per-query fetch. A candidate's byte address is
      `impacts_rel + posting.rank(doc) − 1`: the posting bitmap the term search
      already fetched IS the addressing structure.
- [x] Core reader: `ImpactIndex` (open = header + resident sparse, wasm-safe) +
      `rerank(postings, candidates, k)` (one coalesced entry-stripe wave + one
      coalesced impact-byte wave) + `search_bm25(terms, impacts, q, m, k)`;
      `TermIndex::query_postings` / `dict_terms` added. 6 tests incl.
      brute-force BM25 equivalence on a 200-doc corpus; clippy/fmt/wasm-check
      clean.
- [x] Core builder: `ImpactsAccumulator` (same `Tokenizer` as the `.rrt` build)
      + `write_impacts` joined against `dict_terms()` of the FINISHED index so
      head_off keys are byte-true; loud error on tokenizer mismatch.
- [ ] Full-corpus builder example over records-full: the in-RAM accumulator
      does not scale to 484M docs — needs chunked spill-and-merge (sorted
      (term, doc, tf) runs per chunk, k-way merge against the dict scan).
      Estimated artifact ~15–50 GB (~$0.35–1.15/mo S3).
- [ ] Wasm binding (`RrbIndex` or fold into `RrtIndex.searchScored`) + a
      "relevance rerank" toggle in the demo's term mode.
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
