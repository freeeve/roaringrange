# 026 — Cheap exact counts from container headers

**Status:** pending

The demo's "exact count" link warns it may scan hundreds of MB (the full tail
intersection). For a large class of queries the answer is available from KB-scale reads:

- Roaring's NO_RUNCONTAINER descriptive header stores **per-container cardinalities**.
  A single term's exact hit count = Σ cardinalities = one header read (8 + 8·size bytes).
- A multi-term AND gets an instant **upper bound**: min over terms of the per-term counts
  (and a lower bound of 0); display "≤ N" instead of "N+".

## Design

1. `Index::term_count(key) -> u64` reading just the posting header (reuses
   `needed_header_len`/`parse_dir`).
2. Single-trigram-set queries (a query whose ngram_keys has 1 key — short queries) get an
   exact count for free; multi-key queries get "≤ min" immediately, refining to exact only
   on the explicit full scan as today.
3. Demo: the result-count header shows `N` (exact, single key), `≤ N` (bound), or `N+`
   (loaded so far) — the "exact count" scan link remains for the bound case.

## Acceptance

- Single-key query shows an exact total with KBs of fetch (verify via perf bar).
- Bounds are correct (≥ true count) on a differential test vs full intersection.
