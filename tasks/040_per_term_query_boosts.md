# Task 040 — per-term query boosts in `searchBm25`

Let the caller weight individual query terms, so an identity/genre term can outrank an
incidental one — e.g. boost "lesbian" in *"lesbian tennis romance"* so identity-bearing
matches beat tennis-only or romance-only ones.

**Status:** exploratory — **parked behind 038 (weighted RRF) + 039 (BM25 scores)** and
behind consumer-side measurement. Pull only if the qllpoc experiments show per-*term*
control closes a gap that arm-level weighting (038) + per-field indexes don't.

## Why

qllpoc's precision problem on multi-term queries is partly that all query terms are
interchangeable in BM25 (a 2-of-3 partial match scores like another 2-of-3). Per-term
boosts let the consumer encode "this term matters more" — or, combined with
`min_match`, lean toward "this term should be present."

## API sketch

Optional parallel per-term weights on `searchBm25` / `searchBm25MinMatch`, defaulting to
all-`1.0` (backward compatible). Each query term's `idf × impact` contribution scales by
its weight.

## Notes / partial coverage

- Per-field term indexes + weighted RRF (038) approximate *field-level* emphasis; this is
  finer-grained (per query *term*).
- Effort: moderate — scoring loop + wasm binding.

## Acceptance

1. Omitting weights → identical to today.
2. A boosted term measurably lifts docs strong on that term; a `0` weight neutralizes it.
3. Backward-compatible wasm signature (new arg optional).
