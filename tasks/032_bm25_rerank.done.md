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
- [x] Full-corpus builder (`build_impacts`, chunked spill + dict-lockstep
      merge) RAN 2026-06-11: 484,369,476 docs / 186,934,488 terms, all in
      lockstep; 15.3 h (14.8 h spill at ~350–650 s per 4M-doc chunk, ~32 min
      merge). Artifact `/tmp/oa-out/openalex-484m-stem.rrb` = **24.3 GB**
      (~$0.56/mo S3) — ~20.4B (term, doc) pairs; the 87M-term hapax tail is
      ~1 byte/term. **Relevance spot-check PASSED**: "roaring bitmaps" m=2000
      rerank = all top-5 explicitly Roaring papers (the generic bitmap-
      compression survey demoted from #2), 57 ms local incl. process spawn.
      Needs S3 upload. (Cosmetic: the merge checkpoint log format prints
      "NM-term" wrong — fix on next builder touch.)
- [x] **LIVE 2026-06-12**: `.rrb` uploaded (24.3 GB, immutable); wasm
      `RrbIndex` + `RrtIndex.searchBm25/rerankIds` deployed; demo "BM25
      relevance" toggle default-ON under term + hybrid-term (`?bm25=0`
      opt-out). Verified live: "roaring bitmaps" → `term · bm25 · 27
      matches · 60 ms`, all top hits Roaring-specific; `bm25=0` falls back
      to citation order. search_bm25 is HEAD-FIRST (reranks head candidates
      from the bytes plain search already moves; tails only when the head
      can't fill the window) so common queries pay ~KBs extra.
      Hybrid-term's text arm feeds BM25-ranked ids into the RRF fusion.
- [x] Cross-mode rerank: score TRIGRAM-mode (and hybrid-tri) candidates with
      the same term `.rrb` — the shared doc-ID space makes one sidecar serve
      every mode. Glue only: trigram search yields candidate ids; resolve the
      query's words via `TermIndex::query_postings`; feed both straight into
      `ImpactIndex::rerank(postings, candidates, k)`. Query terms that don't
      resolve in the `.rrt` (typos/partial words — trigram's specialty) just
      contribute no BM25 component; zero resolvable terms degrades to static
      rank. Decision record: trigram-level impacts were REJECTED — character
      n-gram tf/idf is noise as a relevance signal, and the artifact would be
      ~150–190 GB (every doc posts to ~300–500 distinct trigrams) vs the term
      sidecar's ~35 GB.
      **DONE 2026-07-02** (demo glue; core `rerankIds` binding already shipped):
      `index.html` reranks trigram + hybrid-tri candidates through the shared
      `.rrb` via `rterm.rerankIds` (resolves `query_postings`, then
      `ImpactIndex::rerank` — unmatched/unresolvable candidates fall back to
      static rank, never error). UI: the BM25 toggle now also shows under
      trigram as **opt-IN** (default OFF there to preserve the cursor's exact
      count + tail paging; default ON stays for the word/term-hybrid modes).
      `bm25Off` became a tri-state (null = per-mode default / true = opted out /
      false = opted in); the URL encodes only the exception vs the mode default
      (`?bm25=0` opts a default-on mode out, `?bm25=1` opts trigram in). The
      trigram rerank path runs before the server branch and engages whenever a
      client candidate source (`idx` or the split set `rrss`) is booted; a
      server-only boot falls through to the normal Lambda path unchanged.
      **Deploy pending** (wasm bundle already carries `rerankIds`; needs a
      `deploy.sh` push of the updated `index.html`).
- [x] Bench row in `live_bench`: bytes/latency of rerank vs rank-order-only,
      plus a relevance spot-check (the "roaring bitmaps" seminal-paper test).
      **DONE 2026-07-02** (live over CloudFront): added `term mono +bm25`
      (`search_bm25`, M=2000) and `trigram mono +bm25` (trigram candidates at
      SEM_K=250 reranked through the SAME term `.rrb`), plus a `roaring bitmaps`
      spot-check printing top-5 rank-order vs bm25 (term) and trigram→bm25.
      Live K=25, "machine learning": term mono 15.5 KB / 662 ms → +bm25
      24.29 MB / 6.3 s; trigram mono 860 KB / 7.6 s → +bm25 25.12 MB / 15.9 s.
      The rerank cost is the impact-byte candidate-window fetch — largest on
      mega-common terms (both query words span most containers); rarer queries
      (e.g. the spot-check) are KB-scale. Relevance spot-check (live) — "roaring
      bitmaps", top-25 rank-order vs bm25 differ in **24 of 25 slots**:
      - rank-order top-5: `[2722648, 7776809, 11137692, 14211300, 36283355]`
        (generic bitmap-compression papers; the real Roaring paper demoted to #5)
      - term bm25 top-5: `[36283355, 314306731, 11137692, 84865397, 278229021]`
      - trigram→bm25 top-5: `[36283355, 314306731, 11137692, 84865397, 278229021]`
      The trigram→bm25 top-5 is **byte-identical** to the term bm25 top-5: one
      `.rrb`, fed candidates from a different backend (trigram), yields the same
      relevance order — the cross-mode thesis (one sidecar, shared doc-ID space)
      proven live. Both surface the real Roaring paper at #1.

## Non-goals (Tier 2, separate decision)

Global top-k BM25 (Block-Max WAND) — per-container max-impact metadata + a
skip-scan evaluator. Only worth it if rerank's top-M window demonstrably misses
relevant low-rank docs that hybrid's semantic arm doesn't recover.

Known limitation to document: a low-citation doc with high lexical relevance
outside the top-M candidate window is invisible to the reranker.
