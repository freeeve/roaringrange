# Task 005 — FST term-level inverted index (RRTI)

**Status:** scoping (pending). Planned 2026-06-04.

A **new, additive** index format in the roaringrange family — a word/term-level
inverted index with an **FST term dictionary** — sitting *alongside* the trigram
index (RRS), vector index (RRVI), facet sidecar (RRSF), record store (RRSR), and
identifier lookup (RRIL). **It replaces nothing.** Apps pick the index(es) that fit
and may compose them, because every index in the library shares one doc-ID space.

Same ethos as the rest of roaringrange: a static artifact on S3, booted with a few
small range reads, with the bulk range-fetched only as a query needs it.

---

## 0. Library model — an à la carte menu of indexes

roaringrange is a **toolkit of independent, composable static index formats**, not a
single monolithic index. Users **pick and choose** which index they want — and may use
**any combination** of them:

| Format | File | Answers | Status |
|---|---|---|---|
| Trigram text index | `.rrs` (RRSI) | substring / fuzzy / no-space-script text | shipped |
| **FST term index** | **`.rrt` (RRTI)** | **whole-word / prefix / fuzzy text** | **this task** |
| Vector index | `.rrvi` (RRVI) | semantic / similarity | shipped |
| Facet sidecar | `.rrf` (RRSF) | filtering / counts | shipped |
| Record store | `.idx`/`.bin` (RRSR) | render hits → fields | shipped |
| Identifier lookup | `.rril` (RRIL) | exact ID → doc | shipped |

The unifying contract is **one shared doc-ID space**: every format keys into the same
doc IDs (assigned once, in global rank order at build time). That is what makes the
menu composable — an app can ship only a term index; or term + facets + records; or
term + trigram + vector + facets, fusing their results — with no remapping between
them. The term index is simply **one more item on the menu**; adopting it neither
requires nor excludes any other. Composition specifics in §8.

---

## 1. Why a term index (the case, quantified)

A word-level inverted index is strictly fewer S3 GETs than trigrams along two axes:

- **Count axis.** A query word of length L fans out into ~(L−2) trigrams, each its
  own posting GET; a term index fetches *one* posting for the whole word. `posthuman
  became` ≈ 7 + 5 = **12 trigram-posting GETs vs 2 term-posting GETs**. You can't
  batch it away: S3 serves a single range per GET (no multipart-range), so 12
  postings is 12 requests. HTTP/2 hides the *latency* (one wall-clock wave) but not
  the request count or the bytes. ⇒ ~**6× leaner** on typical words.
- **Selectivity-per-byte.** Individual trigrams are common even when the word is
  rare: `posthuman` is rare, but `pos`,`ost`,`hum` each hit millions of docs, so you
  fetch several large, low-selectivity bitmaps and AND them down. A term posting
  carries the word's *actual* rarity — rare word → small, selective posting in one
  shot. Fetching common substrings to find rare words is the intrinsic tax of n-gram
  indexes, and it's why Lucene, Tantivy, and Pagefind are all term-level.

roaringrange's differentiation was never the lookup layer — it's the rank-ordered
doc IDs, the head/tail split, roaring, and facets. Those sit on the **postings** and
don't care whether the key is a trigram or a term. So a term dictionary slots in and
keeps every innovation that makes roaringrange itself.

**The product boundary (what trigrams still own).** Trigrams buy four things a bare
term index loses: infix/substring (`human` hitting `posthuman`), graceful typo/fuzzy
degradation, prefix-without-extra-structure, and tokenizer-free multilingual / no-space
scripts. The term index targets the common case (whole-word + prefix + fuzzy); the
**trigram index remains the right tool** for substring/infix and no-space scripts. The
two coexist in the library; an app composes them as it sees fit (see §8).

---

## 2. What is reused unchanged (the shared substrate)

From the architecture map — all of this is keyed by **doc ID** and is independent of
the dictionary key type, so the term index inherits it for free:

| Layer | Where | Reuse |
|---|---|---|
| Roaring postings + head/tail split | `rust/src/build.rs::split_posting`, `posting.rs` | **identical** — RRTI's postings region is byte-for-byte the RRS postings region |
| Doc-ID global rank order (citation desc; head = top-K) | build-time assignment; `headBoundary` | **identical** — RRTI MUST be built over the same doc-ID assignment as the sibling indexes |
| Concurrent fetch waves | `index.rs::fetch_postings` (`futures::join_all`) | **shared** |
| Lazy tail + container-level intersect | `Cursor::ensure`, `posting.rs::tail_intersect_and` | **shared** |
| Cursor pagination + fuzzy `max_missing` threshold | `index.rs::search_cursor_filtered` | **adapted** (term-set AND instead of trigram-set AND) |
| Facets (RRSF) | `facet.rs`, `FACETS.md` | **unchanged** — composes via doc IDs |
| Records (RRSR) | `records.rs`, `RECORDS.md` | **unchanged** |
| Identifier lookup (RRIL) | `lookup.rs` | **unchanged** |

**Only the dictionary + key layer is new.** In RRS the dictionary is a sorted array
of 24-byte `(key u64, headOffset, headSize, tailSize)` entries plus an in-memory
sparse index, and the key is a packed/hashed trigram (`ngram.rs`). RRTI swaps that
sorted-u64 dictionary for an **FST term dictionary** (variable-length UTF-8 term →
posting metadata) and swaps trigram extraction for **term tokenization**.

---

## 3. The new format — RRTI (`.rrt`)

`RRTI = [header] [FST term dictionary] [postings region]`. The postings region is the
**same** `[head][tail]` roaring layout as RRS, so the reader's posting code is shared
verbatim. Proposed (to lock in step 1):

- **Header** (fixed): magic `"RRTI"`, version `u16`, flags `u16` (stemmed? has-positions?
  has-inline? language id), `termCount u32`, `headBoundary u32`, `fstOffset/fstLen u64/u32`,
  `inlineBlobOffset/Len`, `postingsOffset u64`.
- **FST dictionary blob**: a minimized acyclic FST mapping `term (UTF-8) → u64 output`.
  Boot-loaded once (range-fetched in one read, or a couple of chunks); thereafter an
  **in-memory automaton** — term→output costs **zero S3 GETs**. The `u64` output is
  tagged (§5): either an inline pointer into the resident inline-postings blob (rare
  terms → 0 extra GET) or an `(offset,len)` into the S3 postings region (1 GET, tail lazy).
- **Inline-postings blob** (resident, part of boot region): the tiny postings of rare
  terms, packed; referenced by inline FST outputs. (Optimization §6.1.)
- **Postings region**: per common term `[head][tail]` roaring — identical to RRS.

**Doc/format docs:** add `TERMS.md` alongside `FORMAT.md`/`FACETS.md`/`RECORDS.md`/
`VECTORS.md` (per-format doc convention), freezing the RRTI v1 layout.

---

## 4. FST mechanics (for implementers)

An **FST (finite-state transducer)** is a minimized automaton whose edges carry an
input symbol *and* an output value. Walk it spelling the term one char per edge,
summing outputs; land on a final state and the accumulated output *is* the term's
value (here: posting metadata). It is the third step of a lineage:

1. **trie** — shares common *prefixes* (`cat`,`car`,`card` share `ca`).
2. **minimized acyclic FSA (DAWG)** — also shares common *suffixes* by merging states
   with identical futures (`running`,`jumping`,`swimming` funnel into one `-ing` tail).
   Suffix sharing is what makes it tiny for natural-language vocabularies.
3. **FST** = that minimized automaton *plus outputs on edges*.

The one tension (the whole trick): suffix sharing merges distinct terms' final states,
so values can't hang off leaves. Resolution — spread outputs along **edges**, summing
along each path, pushed as far toward the start as possible (at each state, shove the
min outgoing value back onto the incoming edge, leaving only differentials ahead). Two
edges may point at the *same* shared state with *different* outputs: output on the
edge, shared future in the state.

Worked example, dictionary `{star→3, stop→4, top→5}`:
- `star`: s/**3** → t/0 → a/0 → r/0 = 3
- `stop`: s/**3** → t/0 → o/**1** → p/0 = 4
- `top`:  t/**5** → o/0 → p/0 = 5
The `s` edge carries 3 (the min of its subtree {3,4}) pushed to the root, leaving the
lone differential `1` on `stop`'s `o` edge. The state after `o` is shared by both
`stop` (entered with output 1) and `top` (entered with output 0) because the future
(`p`→final) is identical.

**Construction:** a single **sorted pass** with incremental minimization — as each new
key diverges from the previous, freeze the diverging tail; before registering a new
state, check a registry of already-built states and reuse an equivalent one (same
transitions, outputs, finality). roaringrange already sorts terms, so this fits.
**Lookup** is O(term length), no vocabulary-wide binary search.

**Two-level option (Lucene/Tantivy `.tip`/`.tim`).** To keep the resident FST small at
huge vocab, map a term *prefix* → on-disk *block*, then range-fetch that one block and
scan it for the exact term + metadata. v1 recommendation: **one-level** (full term →
output, whole FST resident) — a scholarly title/abstract vocabulary fits resident;
revisit two-level if the FST outgrows the boot budget (§10 risk).

---

## 5. Capabilities the FST hands back

- **Whole-word lookup:** walk → output → posting. **0–1 GET** (0 if inline).
- **Prefix scan / autocomplete:** walk to the state after the prefix, traverse what's
  reachable. **Native, free** — the term-index answer to "prefix without extra structure."
- **Fuzzy:** build a **Levenshtein automaton** accepting everything within edit distance
  k of the query and walk it in lockstep with the FST, emitting only terms both accept
  (Lucene's exact mechanism). In-memory, **vocab-scan-free** — recovers typo tolerance
  after dropping trigrams.

The only thing not cheaply recoverable is true **mid-word infix** — the trigram-shaped
hole, and exactly why the trigram index stays in the library.

---

## 6. The real wins: fewer fetches, not a cleverer index

There's no magic posting structure that beats term-keyed roaring for this access
pattern (§9). The genuine gains move the *common case toward zero fetches* — the same
"spend on the tail, give the head away" move as head/tail postings:

1. **Inline rare-term postings.** A rare term's posting is a handful of doc IDs —
   smaller than the `(offset,len)` pointer to go fetch it. Store it in the FST output /
   resident inline blob (Lucene does this for low-freq terms): the dictionary lookup
   returns the posting directly, **0 extra GET**. Rare terms are the selective, valuable
   ones — `posthuman` resolves for free; you only pay a fetch when a term's list
   outgrows the inline threshold. (head/tail instinct at the dict↔posting boundary.)
2. **Materialize hot multi-term/phrase postings.** A term index floors at 1 GET/term
   because each posting lives at its own offset. Break the floor by precomputing a
   *combined* posting for frequent pairs/phrases (`machine learning`, `climate change`)
   as a single key ⇒ the two-term query becomes **1 GET**. A static result cache in
   posting form; uncommon combinations fall back to per-term intersection. Pick the top
   few thousand from the corpus / query log.
3. **Push the residency boundary.** Boot-load the FST (already resident), the inline
   rare postings (free, they're in the FST), and head-presence bitmaps / head postings
   for the few thousand most common terms. A large fraction of queries (common terms,
   top-K only, no deep pagination) then resolve **entirely from resident data at zero
   incremental GET**. This is Pagefind's real insight: the win is the dictionary that
   loads once, not the posting format.

---

## 7. Deliberately NOT building (and why — name the frontier)

- **FM-index / BWT / suffix automaton.** Textbook succinct substring search; gorgeous in
  RAM. But backward search is one rank query *per pattern character* — m sequential
  random accesses ⇒ m serial round-trips over S3 per query. Latency-poison; the HNSW
  failure mode. Skip.
- **BlockMax-WAND / per-term impact ordering.** IR state-of-the-art for fewest-bytes-to-
  top-K, but it wants each term's posting in a *different* order — colliding head-on with
  the single global rank order that serves intersection, ranking, and head/tail at once.
  The global rank head is already a coarse impact pre-order shared across all terms. Not
  a trade worth making for bytes we're not short on.
- **Learned dictionary indexes (RMI/PGM).** The FST already resolves in 0–1 GET once
  resident; nothing left to win.
- **Partitioned Elias-Fano.** Marginal byte win over battle-tested roaring in wasm; not
  worth the reimplementation.

**Phrase/positions footnote.** Real phrase/proximity search needs *positional* postings
(bigger). v1 is bag-of-words; phrases come via materialized hot-phrase postings (§6.2)
or a later positional phase. Trigrams' weak boundary-spanning adjacency signal is not a
substitute for positions.

---

## 8. Composition (library feature, app-level — not mandated)

Because every index shares the doc-ID space, an app can wire **any subset** together
and fuse their doc-ID outputs however it likes — the demo already fuses the trigram and
vector indexes via reciprocal-rank fusion (`hybrid` mode), and a term index drops into
that same fusion unchanged. The pattern below is **one worked example**, not the only
one:

- **Term index primary, trigram index as lazy fallback.** Whole-word/prefix queries —
  the overwhelming majority — go through the FST at ~1 GET/term. The trigram postings
  are touched only when the user types a wildcard, an explicit substring, or a fuzzy
  match the term path missed. You pay the 12-GET trigram cost *only* on queries that
  demand substring semantics. (Same "serve the common case cheap" logic as head/tail,
  one layer up.)
- This belongs in `Catalog` (`rust/src/catalog.rs`) as an optional wiring, mirroring how
  it already composes index + facets + records. The library *enables* it; it does not
  force any index on anyone.

---

## 9. Dependency decisions

- **FST + Levenshtein automaton — DECIDED (2026-06-04): the BurntSushi `fst` crate.**
  Pure-Rust, wasm-safe, battle-tested as the term-dictionary layer of **Tantivy** and
  **Meilisearch** (our exact use case), `Map<term → u64>`, native prefix streams, and a
  **built-in `Levenshtein` automaton** — the single genuinely hard part of §5, already
  done and validated at scale. Maintenance profile is "finished software," not abandoned:
  latest release 0.4.7 (2021), last commit 2024, not archived, ~4.3M recent downloads / 20M
  total — a stable, frozen-API foundational crate, not active churn. Low-churn risk is
  bounded because it's **permissively licensed (Unlicense/MIT)** and small/self-contained,
  so we keep a **vendoring escape hatch**: fork a trimmed copy if upstream ever stalls on a
  bug we hit. Lives behind a non-default **`terms`** Cargo feature (exactly like `vector`) —
  reader stays wasm-safe, builder native-only. Chosen over hand-rolling an FST + automaton
  from scratch to de-risk steps 1–3; revisit vendoring/minimizing once the format is proven.
- **Stemming.** `rust-stemmers` (Snowball/Porter, pure-Rust) behind the `terms` feature,
  off by default — or defer stemming to step 5 and ship v1 unstemmed. (Still open.)

---

## 10. API surface & integration (mirror the RRVI pattern)

- **`TERMS.md`** — frozen RRTI v1 layout (per-format doc convention).
- **Rust `rust/src/terms.rs`** — `TermIndex<F: RangeFetch>` reader: `open` (boot FST +
  inline blob), `search(query)` / `prefix(p)` / `fuzzy(term, k)` → posting fetch → reuse
  the shared AND / head-tail / Cursor machinery. Behind a **non-default `terms` feature**.
- **Rust `rust/src/terms_build.rs`** — builder: tokenize corpus (reuse `ngram.rs::normalize`
  for the char rules; add optional stemming/stopwords) → per-term roaring postings (reuse
  `split_posting`) → build the FST (sorted pass) → emit RRTI. Native-only (like the IVFPQ
  trainer); the reader stays wasm-safe.
- **Tokenizer symmetry invariant** (#1 correctness risk): the build-time and query-time
  tokenizers (normalize + optional stem + stopwords + language) must be **byte-identical**
  — same discipline as the vector model invariant. Centralize in one module used by both.
- **Python (PyO3)** builder binding; **wasm** `TermIndex.search/prefix/fuzzy` binding;
  **Go** reader/transcode parity (later, like the RRS Go path).
- **Demo**: add a 4th search mode, or wire the term-primary + trigram-fallback composition
  behind the existing toggle.

---

## 11. Phasing (steps)

1. **Format + reader.** Freeze RRTI (`TERMS.md`); `terms.rs` reader (boot FST → term→posting
   → reuse head/tail AND); `terms` feature; tests + a tiny hand-built fixture. Decide
   `fst`-crate vs hand-roll, one-level vs two-level.
2. **Builder.** `terms_build.rs` tokenize → postings → FST → RRTI; PyO3 binding;
   cross-validate reader vs builder.
3. **Prefix + fuzzy.** Prefix streams (autocomplete) + Levenshtein-automaton fuzzy.
4. **Inline rare postings (§6.1) + residency (§6.3)** — head-presence/head postings for hot terms.
5. **Optional stemming + stopwords (§9)**, symmetric build/query, language-tagged in the header.
6. **Hot-phrase materialization (§6.2).**
7. **wasm binding + demo wiring** (term mode / term-primary+trigram-fallback in `Catalog`); Go parity.
8. **Positional postings / phrase search (optional, later).**

---

## 12. Open decisions to lock

- Format name/magic/extension: **RRTI / `.rrt`** (proposed).
- ~~`fst` crate vs hand-rolled FST + Levenshtein~~ → **DECIDED: `fst` crate** (§9).
- One-level (whole FST resident) vs two-level `.tip`/`.tim`. (Proposed: one-level v1.)
- Stemming default on/off + language set; stopwords on/off.
- Inline threshold (posting byte size below which a term is inlined).
- Positions in v1? (Proposed: no.)
- **Doc-ID alignment:** RRTI must be built over the *same* doc-ID assignment (citation
  rank) as the sibling indexes so facets/records/vector compose. The term builder must
  consume the shared rank ordering, not invent its own.

## 13. Risks

- **Resident FST size at scale.** 484M-doc scholarly vocabulary (incl. author names,
  numbers, identifiers) could be millions of unique terms → tens–hundreds of MB FST →
  boot cost. Mitigate: min-frequency term pruning, two-level `.tip`/`.tim`, or a separate
  names index. Measure unique-term count early.
- **Tokenizer drift** between build and query (the symmetry invariant).
- **Fuzzy automaton cost in wasm** for large k / long terms — cap k, gate behind explicit
  fuzzy intent.
- **Multilingual / no-space scripts** — term tokenization is weak there; that's the
  trigram index's domain. Document the boundary; let apps route by script.

---

*Net:* a term-keyed FST inverted index is the fewer-lookups answer (~6× on typical
words, more selective per byte), it bolts the existing ranking/head-tail/facets/records
substrate on unchanged, and the real wins are **residency + materialization** (inline
rare postings, bake hot phrases, load enough that the common case never hits S3) — the
same "spend on the tail, give away the head" principle the whole library already runs on.
It lives **next to** the trigram index, which keeps owning substring/fuzzy/no-space.
