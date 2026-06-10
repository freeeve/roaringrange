# 023 — Cost-based query routing (client ↔ search-lambda)

**Status:** pending

The demo's "Server-side search" toggle is a hand-operated escape valve. Make it automatic:
estimate a query's client-side egress **before fetching any posting**, and route expensive
queries to the search-lambda by default.

## Why this works with zero format changes

- The trigram dictionary lookup (sparse block + dict block, KBs, range-cached) returns each
  key's **posting byte size** — so `Σ posting sizes` is known before any posting is fetched.
- The facet sidecar's category table is **resident after boot** (head/tail sizes per
  category), so a filter's added cost is known with no fetch at all.

## Design

1. `rust/src/index.rs`: `Index::query_cost(query) -> u64` — `ngram_keys` → dict lookups →
   sum `rec.size`. Reuses the existing `locate`/dict-block machinery (cache-warm for the
   client path if we fall through to a local search).
2. `rust/src/wasm.rs`: `RrsIndex.queryCost(q)` binding (and a `RrfFacets`/cursor-side way to
   read selected categories' posting sizes — they're already resident).
3. `examples/openalex/web/index.html`: in the trigram path, when the lambda is configured
   and the user hasn't forced a mode: `cost = await idx.queryCost(q) + facetCost(filters)`;
   above `AUTO_ROUTE_MB` (default ~8 MB) call `goPageServer` instead of the local cursor,
   and say so in the perf summary ("auto-routed server-side · est. 42 MB saved").
   The server-mode toggle becomes tri-state in spirit: auto (default) / always / never —
   implement as the existing checkbox plus an "auto" note, or a small select.

## Acceptance

- A rare-term query runs client-side exactly as today (estimate ≪ threshold).
- A dense query ("machine learning", a broad facet filter) routes to the lambda
  automatically; the perf bar shows the estimate and the routed request.
- Manual server-mode toggle still forces either path.
- Unit test for `query_cost` (estimate == Σ dict sizes; absent trigram → 0 for that key).
