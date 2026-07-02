# 073: feat(build/go): expose BM25 sidecar head-offsets + wire non-English stemmers Go-side

**Severity: MED (a pure-Go build cannot produce BM25 ranking or non-English
stemmed indexes; it silently degrades to boolean/word-level).** Surfaced by
libcatalog wiring `WriteTermIndexFull` as its only term-index path. No frozen
byte layouts change -- both items are Go build-side API gaps, not format changes.

## Findings

1. **BM25 sidecar is unbuildable from Go: `WriteTermIndexFull` computes per-term
   `head_off` but never returns it.** `WriteImpacts` (`bm25.go:114`) requires
   `[]DictEntry{Term, HeadOff}` whose `HeadOff` must be the *exact* posting head
   offset written into the paired `.rrt`. `WriteTermIndexFull`/`WriteTermIndex`
   (`terms.go:382,364`) compute `headOff := uint64(region.Len())` at `terms.go:399`
   and front-code it into the dict block, but return only `error`. No exported
   reader recovers it either: `Open`->`*Index` (`reader.go:47`) exposes only
   `Posting(key uint64)`/`Postings`/`NgramCount` -- lookup by hashed key, no
   dictionary enumeration and no (term -> head_off) accessor. The BM25 golden
   (`bm25_test.go:41-46`) sidesteps this by *fabricating* offsets
   (`uint64(i)*16 + 100`), so the two writers have never been exercised against a
   real paired `.rrt`. Net: a Go builder can write `.rrt` postings or synthetic
   `.rrb`, but cannot write a `.rrb` that addresses a real `.rrt`.

   **Fix (pick one, prefer the first):**
   - Have `WriteTermIndexFull` return `([]DictEntry, error)` (terms in dict order,
     with real `HeadOff`) -- callers feed it straight to `WriteImpacts`. Keep the
     `error`-only signatures as thin wrappers for source compatibility.
   - Or add `WriteTermIndexWithImpacts(dst, sidecar, postings, acc, ...)` that
     writes `.rrt` + `.rrb` in one pass, sharing the head-offsets internally (also
     removes the "tokenizer must match" footgun since one call owns both).
   - Or expose dictionary enumeration on `*Index` (e.g. `func (s *Index) Dict()
     iter.Seq[DictEntry]`) so a builder can reopen a finished `.rrt` and recover
     offsets. Weakest option -- an extra read pass -- but unblocks without touching
     the writer.
   - Add a golden that round-trips: build a real `.rrt`, derive the dict via the
     chosen path, `WriteImpacts`, and assert the sidecar addresses it (the current
     fabricated-offset golden does not cover this seam).

2. **Only English stemming is wired Go-side, though go-stemmers ships all 18.**
   `NewTermTokenizerFull` (`terms.go:229`) hardcodes
   `if stem && lang == TermLanguageEnglish { st = stemmers.New(stemmers.English) }`,
   leaving the stemmer nil for every other language -- so a Spanish/French/German
   corpus indexes word-level even when `stem` is requested. The dependency is
   already present and complete: `github.com/freeeve/go-stemmers` exports
   `Arabic/English/French/German/Spanish/...` (`stemmers.go:42-78`).

   **Fix:** map `TermLanguage` -> `stemmers.Algorithm` and build the stemmer for any
   supported language (nil only for `TermLanguageNone`/unsupported). This is the Go
   twin of the Rust `Tokenizer::with` language coverage; pairs with the stop-word
   coverage from task 055. Add a tokenizer test per language asserting a known
   stem, byte-checked against the Rust tokenizer as the existing English case is.

## Acceptance

- A pure-Go build can emit a BM25 `.rrb` that correctly addresses its paired
  `.rrt`, proven by a round-trip golden (not fabricated offsets).
- `NewTermTokenizerFull` stems any go-stemmers-supported language when `stem` is
  set; per-language test asserts a known stem, byte-exact vs. the Rust tokenizer.
- No changes to frozen on-disk layouts (RRTI/RRSB); these are build-API additions.

## Consumer note (libcatalog)

libcatalog `tasks/010` ships a v1 lexical index over `WriteTermIndexFull`:
boolean whole-word (presence, not tf) and English-only stemming, precisely because
of the two gaps above. Once (1) lands it can switch to BM25 impacts; once (2) lands
its `iso639` map (already covering all 18) stems non-English corpora with no
libcatalog-side change beyond flipping the `stem` flag.
