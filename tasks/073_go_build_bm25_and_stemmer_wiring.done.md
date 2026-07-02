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

## Outcome (DONE)

Both build-API gaps closed, no frozen on-disk layout touched; full Go suite, Go
conformance module, and Rust `terms::`/`bm25::` tests all green.

**(1) BM25 sidecar head-offsets now recoverable from a Go build.** Added
`WriteTermIndexFullDict(...) ([]DictEntry, error)` (`terms.go`) -- the same writer as
`WriteTermIndexFull`, additionally returning the dictionary in byte-lexicographic order with
the *real* posting `HeadOff`s it front-codes into the `.rrt`. `WriteTermIndexFull` is now a
thin error-only wrapper over it, so `WriteTermIndex`/`WriteTermIndexWith` and every existing
caller (`termsplitsetbuild.go`, `stopwords_test.go`, and the external libcatalog) keep
compiling unchanged -- strictly more source-compatible than mutating `WriteTermIndexFull`'s
own signature (the task's option 1), which would have force-broken libcatalog. The bytes
written to `dst` are identical to before (RRTI split-set goldens still pass).

New round-trip golden `TestBM25SidecarAddressesPairedRRT` (`bm25_test.go`): builds a real
`.rrt` via `WriteTermIndexFullDict`, feeds the returned dict straight to `WriteImpacts`, then
proves correctness *independently of the writer's offset math* -- it parses the `.rrt`
postings region and asserts each returned `HeadOff` lands exactly on that term's posting
record (head bytes + tail length), then asserts the `.rrb` entries table keys on those same
offsets in dict order with the posting's cardinality. This replaces the old
fabricated-offset (`i*16+100`) coverage gap.

**(2) All 18 Snowball languages now stem Go-side.** Added the `stemAlgorithm`
`TermLanguage -> stemmers.Algorithm` map (`terms.go`) -- an explicit by-name map (the two enums
number languages differently, so a numeric cast would silently mis-stem), mirroring the Rust
`Language::algorithm` mapping one-to-one. `NewTermTokenizerFull` builds the stemmer for any
mapped language (nil only for `TermLanguageNone`/unsupported), replacing the English-only
hardcode.

Byte-exact-vs-Rust proof: `rust/examples/gen_tokenizer_stem_golden.rs` tokenizes one
non-trivially-stemming, distinct-rooted word per language through the Rust `Tokenizer::with`
and emits `testdata/tokenizer_stem_golden.txt`. `TestTokenizerStemMatchesRustGolden`
(`terms_stem_test.go`) asserts the Go tokenizer reproduces every line, and the symmetric Rust
`terms::tests::tokenizer_stem_golden_matches` pins the Rust tokenizer to the same committed
file (guards against either port drifting). `TestStemAlgorithmCoversAllLanguages` asserts the
map is complete (18 entries), one-to-one (distinct algorithms), builds a non-nil stemmer for
every byte 1..=18, and stays nil for `TermLanguageNone`.

Files: `terms.go`, `terms_stem_test.go` (new), `bm25_test.go`,
`rust/examples/gen_tokenizer_stem_golden.rs` (new), `rust/src/terms.rs` (test only),
`testdata/tokenizer_stem_golden.txt` (new golden). libcatalog can now switch its v1 lexical
index to BM25 impacts and stem non-English corpora with no further core change.
