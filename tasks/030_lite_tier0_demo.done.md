# 030 — "Lite" tier-0 demo from existing artifacts

**Status:** pending

Because doc ID == citation rank, the top-cited 1/10th of the corpus already exists inside
the deployed artifacts: a split-set manifest listing **only the tier-0 splits** is a ~48M-doc
search over files already on S3. Zero new builds.

## Why

The economics section (how-it-works §05) shows the 1/10th-scale numbers: ~0.4 MB average
queries, mobile transfer under ~200 ms, millions of queries/month inside the CDN free tier.
This makes that mode real and lets the demo show both scales side by side.

## Design

1. A tiny tool (or a flag on `splitset_strip_summaries`) that re-emits the 29 KB manifest
   keeping only tier-0 splits → `openalex-lite.rrss` (KBs). Upload next to the full one.
2. Demo: a `?ds=lite` dataset entry pointing `split` at the lite manifest, same
   records-full store (doc IDs are the same), same `.rrf` (counts will reflect the full
   corpus — either accept with a caption or scope counts to tier-0 doc range).
3. Optionally surface as a "fast mode" toggle with the byte/latency comparison in the
   perf bar — it's a live demonstration of the cost-scaling argument.

## Open questions

- Facet counts: full-corpus counts over a lite result set may confuse; cheapest honest fix
  is labeling ("counts span the full corpus").
- Term/semantic lite variants need either tier-0 term splits (exist: the term split set is
  also tiered) and a vector subset (the 10M Gemma RRVI is effectively this already).

## Acceptance

- `?ds=lite` searches the top ~10% corpus with visibly smaller per-query bytes in the
  perf bar; works on the live deployment with only a manifest upload + demo wiring.
