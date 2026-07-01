# Task 055: language-keyed stop words + TERMS.md language-byte fix

Stop-word removal is currently **English-only**: a single flat list (`STOP_WORDS`
in `rust/src/terms.rs:185`, mirrored by `termStopWords` in `terms.go:62`) is
applied whenever the optional stop-word filter (`flags` bit1) is on, regardless of
the index's language. The header already carries an 18-language stemmer byte
(`terms.rs:162` `languages!`: en es ar da nl fi fr de el hu it no pt ro ru sv ta
tr); stop-word removal should key on that same language rather than assume English.

This is **not** "strip more aggressively." For catalog-style corpora stop-word
removal is frequently net-negative (titles are mostly stop words, e.g. "The Who",
"To Be or Not to Be"), and BM25's IDF already down-weights common terms. So the
filter stays **optional and off by default**; the goal is only that, *when
enabled*, it is correct for the index's language instead of silently applying
English stops to a French corpus.

## Scope
- Replace the flat list with a per-language selection keyed on `Language`:
  `stop_words(lang) -> &'static [&'static str]` (Rust) and the Go equivalent.
- Curate a static per-language list for each of the 18 Snowball languages (from
  the Snowball / NLTK / spaCy stopword sets), embedded as sorted static tables,
  with **no new dependency** (matches the current English approach).
- Cross-language parity: the Rust and Go tables must be byte-identical per
  language (as the English list already is), asserted by a conformance test.

## Design
- **Which language does the stop filter use?** Today the `language` byte is
  "meaningful only when bit0 (stemmed) is set" (TERMS.md). To allow, e.g., French
  stop words without French stemming, make the byte meaningful when **bit0 OR
  bit1** is set. Simpler alternative: stop-word removal follows the stemmer
  language, so enabling stops requires setting a language. Pick one and document
  it in the header spec.
- Rust: `is_stop_word(t, lang)` and the `Tokenizer` (`terms.rs:201-265`) select
  the list from `self.language`; `Tokenizer::new` already receives the language.
- Go: mirror in `terms.go` (`isTermStopWord` + `TermTokenizer`), keyed on
  `TermLanguage`.
- Default builds (stops off) emit **byte-identical** output to today.

## Also: fix TERMS.md staleness
The `TERMS.md` header table (the `language` row, ~line 42) still reads "stemmer
language when `bit0` set (`1` = English); `0` otherwise." In fact `terms.rs:162`
defines all 18 languages (bytes 1-18). Sync the doc: give the 18 byte assignments
(or point at the `languages!` macro as the source of truth), and update the
`## Tokenizer` section, which likewise mentions only English.

## Related (out of scope here)
The Go builder wires only `TermLanguageEnglish` for **stemming** (`terms.go`:
`None` / `English` only), while the Rust builder covers all 18. Wiring the
remaining 17 Go-side is the larger multilingual gap (libcatalog `tasks/005`),
noted here because it shares the "one place to add a language" refactor.

## Invariants
- Stop-word filter off => every output byte identical to today (goldens green).
- English stop list unchanged => existing English-stopword indexes unaffected.
- Rust and Go per-language lists byte-identical (conformance test).
- Build folding == query folding preserved (the header carries the language).

## Status
DONE. Implemented across Rust, Go, and Python; defaults byte-identical (all goldens green).

**Design chosen: Option A (language byte meaningful when `bit0` OR `bit1`), no grandfather.**
Stemming and stop-word removal are independent filters over one shared language; enabling
either **requires** a language (no `language==0 ⇒ English` fallback — verified no shipped
index has `bit1` set, so the clean rule is safe). `stem` is decoupled from `language`: the
reader builds the stemmer only under `bit0` but reads the language under `bit0 | bit1`, so an
index can strip a language's stop words *without* stemming.

- **Per-language lists:** 18 sorted `stopwords/<lang>.txt` files at the repo root (English =
  the fixed 31-word list; the other 17 from NLTK, Tamil from spaCy). Rust embeds them with
  `include_str!`, Go with `//go:embed` — the *same physical files*, so the lists are
  byte-identical by construction (no second copy to drift).
- **Rust:** `stop_words(lang)` / `is_stop_word(t, lang)`; `Tokenizer::with(language, stem,
  stopwords, case_fold)` (old `new` kept as a `stem = language.is_some()` shim); `spec()` now
  4-tuple; `from_header` reads the language under `bit0 | bit1`; `TermIndexConfig` /
  `TermSplitBuildConfig` gain `stem: bool`; the stream writer sets `FLAG_STEMMED` from `stem`
  and errors when a filter is on with no language.
- **Go:** all 18 `TermLanguage` constants + `stopwordFile` map; `termStopWordList` /
  `isTermStopWord(t, lang)`; `TermTokenizer.language`; `NewTermTokenizerFull` +
  `WriteTermIndexFull` (old `*With` funcs kept as shims); `TermSplitBuildConfig.Stem`.
- **Python:** `TermBuilder` / `TermSplitSetBuilder` gain `stem=None` (defaults to
  `language is not None`); a `ValueError` when a filter is on with no language.
- **Tests:** Rust `stopword_lists_wellformed`, `stop_words_key_on_language`,
  `stopwords_without_stemming`, `stopwords_without_language_errors`; Go mirrors in
  `stopwords_test.go`. TERMS.md updated (header + Tokenizer sections).

**Still out of scope (unchanged):** Go multilingual **stemming** — only English stems on the
Go side (a non-English `stem` leaves the stemmer nil). Go stop-word *lists* cover all 18.
