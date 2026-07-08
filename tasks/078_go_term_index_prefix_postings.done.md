# 078: Go TermIndex reader -- prefix/autocomplete postings

Filed from libcat (its tasks/167: SKOS vocabulary typeahead over an RRTI in
blob storage). TERMS.md documents prefix/autocomplete as a v2 query path
(router finds the first dict block; scan forward across blocks until a term
sorts past the prefix), but the Go reader (`terms_read.go`) exposes only
exact `find` (`LookupTerm`/`Posting`); `Terms()` iterates the whole
dictionary.

Ask: a `PrefixPostings(prefix string, limit int)` (or equivalent iterator)
on `*TermIndex` that range-fetches only the blocks spanning the prefix and
returns the matched terms + OR'd (or per-term) postings, mirroring the
documented format capability and whatever the rust/wasm reader does.

Context numbers from libcat's LCSH measurement (178,788 terms, RRTI 10.9MB,
router 0.9MB resident): exact Posting is 3 fetches / ~9.5KB per query --
prefix should be similar plus the forward block scan. Until this lands,
libcat holds the whole RRTI resident (10.9MB), so no urgency; it matters
once vocabularies or corpora outgrow that comfortably.

## Outcome (2026-07-08)

Shipped in `terms_read.go`, mirroring `rust/src/terms.rs`:

- `PrefixPostings(prefix, limit) ([]TermPosting, truncated, error)` -- the named
  ask: prefix-matched terms in dictionary order, each with its full posting.
- `SearchPrefix(prefix, limit) ([]uint32, truncated, error)` -- union of the
  matching postings, ascending doc IDs (= descending rank), head bitmaps read
  first and tails only on underflow (mirrors `search_prefix_capped`/`union_locs`,
  incl. the 2048-term `maxPrefixTerms` cap + truncated flag).
- `Complete(prefix, maxTerms) ([]string, error)` -- autocomplete (mirrors
  `complete`).
- Prefix case-folding matches the tokenizer (`fold_prefix` parity; verbatim on a
  case-sensitive index; skips stemming/stop words by design).

Fetch pattern: fst-go's `Ge` is itself a full in-order walk, so instead of
successive `Ge` calls the scan is ONE resident `router.Iter` pass that skips
blocks whose last term sorts before the prefix (no fetch), range-reads only the
blocks spanning it, and stops at the first term past the prefix -- verified by
`TestTermIndexPrefixReadScope` (26-term range over a ~80-block dict = <=8 reads).

Also: `roaringrange get <file> --prefix P [--terms N]` in the CLI, differential
fuzz (`FuzzTermPrefixDifferential`) vs a naive full-dictionary filter.
Released as v0.30.0.
