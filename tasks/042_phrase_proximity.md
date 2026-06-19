# Task 042 — phrase / proximity queries

Match `"tennis romance"` as a phrase, or boost docs where query terms appear near each
other, instead of pure bag-of-words.

**Status:** exploratory, **lowest priority / biggest lift** — parked behind everything
else. Likely **not worth it for this catalog**; listed for completeness.

## Why

Bag-of-words BM25 can't distinguish "tennis romance" (one work) from a doc that mentions
tennis in one place and romance in another — part of qllpoc's reject problem
(tennis-nonfiction, M/M tennis-romance scoring like the real thing).

## Cost

Requires **positional postings** (term positions in the index) — a significant
index-format + query-engine change and noticeably larger artifacts.

## Assessment

Probably **not worth it here**: short descriptions, and mostly thematic/identity queries
where the semantic arm already captures phrase-ish intent. Revisit only if a concrete
phrase-precision failure shows up that weighted fusion (038), score-based fusion (039),
field weighting (041), and per-term boosts (040) all fail to fix.

## If ever pulled

- Add positional postings to the term index; phrase/proximity query mode with a slop param.
- Acceptance: exact-phrase matches outrank scattered-term matches; opt-in (no regression
  for bag-of-words `searchBm25`).
