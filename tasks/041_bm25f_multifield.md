# Task 041 — native BM25F (multi-field BM25)

True fielded BM25: index title / subtitle / subjects / Homosaurus / description as
separate fields with per-field weights and per-field length normalization, scored in one
pass (BM25F), instead of one flat concatenated `text` blob.

**Status:** exploratory — **parked behind 038 + 039** and behind the consumer-side
approximation below. Pull only if field weighting clearly helps in measurement.

## Why

A title match should outweigh a body match. Today everything is one concatenated field,
so noisy body / keyphrase text can outrank a clean title hit. qllpoc's "the body is
load-bearing but is often marketing noise" finding traces directly to the lack of field
structure — there's no way to say "title > subjects > description."

## Consumer approximation that already works (test first)

Build one `.rrt` per field (`roarterms` over field-split text) and fuse the per-field
arms with **weighted RRF (038)**. That's enough to *measure* whether field weighting
helps. If it clearly does, native BM25F is the clean single-index version — avoids N
index opens + N fused arms per query and gives proper cross-field length normalization.

## Scope (if pulled)

- Index format carries per-field length/stats; builder (`roarterms`) splits text per field.
- Query API takes per-field weights; one BM25F scoring pass.
- Effort: larger (format + builder + scorer). Justified only if the per-field-index
  approximation proves the value.

## Acceptance

1. Per-field weights change ranking as expected (title-weighted run ranks title matches up).
2. Single-pass BM25F ≈ the per-field-index + weighted-RRF approximation, but one index/open.
3. Backward compatible with single-field `.rrt` usage.
