# RRTI — roaring range term index (`RRTI`, version 2)

Range-fetchable **term-level** (whole-word) inverted index — an additive member of the
roaringrange family (next to the trigram `RRS`, vector `RRVI`, facet `RRSF`, record
`RRSR`, and lookup `RRIL`). It shares the same doc-ID space as the others, so it composes
with facets/records/vector unchanged. Where `RRS` keys a sorted-`u64` dictionary by
trigram, `RRTI` keys a **whole-word term dictionary**: a query word costs **one** posting
fetch instead of ~(L−2) trigram fetches, and the posting carries the word's true rarity
(small → selective in one shot).

The dictionary is a **blocked, front-coded sorted-string table with a small resident FST
routing over block boundaries** — the same shape as Quickwit's `tantivy-sstable`. Only the
small router is held in memory (O(#blocks), **not** O(vocabulary)); the dictionary blocks
are range-fetched on demand, exactly like the trigram `RRS` range-fetches its dict blocks.
This is what lets a full-corpus vocabulary load in a browser. The postings region is the
`RRS` `[head][tail]` roaring split verbatim, so the reader's posting/AND/head-tail path is
shared and doc IDs stay globally rank-ordered (top-K lives in the head).

> **History.** v1 keyed the dictionary with a single monolithic whole-term FST held
> *entirely* in RAM. That doesn't scale (a full-corpus FST is a multi-GB resident blob), so
> v2 replaced it with the blocked dictionary below. v1 files are no longer read — rebuild
> them as v2.

All integers little-endian. Postings are standard **portable** RoaringBitmaps
(`RoaringBitmap::serialize_into` ⇄ `deserialize_from`).

## Layout

`[ header ][ router FST ][ dict blocks ][ postings region ]`

**Header — 40 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRTI"` |
| version | u16 | 2 | `2` |
| flags | u16 | 2 | `bit0` = stemmed, `bit1` = stop-words removed, `bit2` = case-sensitive (terms not lowercased, so queries skip lowercasing too); rest reserved (`0`) |
| termCount | u32 | 4 | distinct terms in the dictionary |
| headBoundary | u32 | 4 | doc-ID head/tail split — multiple of 65536, default 65536 |
| routerLen | u64 | 8 | byte length of the router FST |
| dictLen | u64 | 8 | byte length of the dict-blocks region |
| blockCap | u32 | 4 | dict block byte cap used at build (informational) |
| language | u8 | 1 | index language — `0` = none, `1`–`18` per the **Language bytes** list below. Meaningful when `bit0` (stemmed) **or** `bit1` (stop-words) is set (offset 36) |
| reserved | u8[3] | 3 | zero padding to 40 B |

**Language bytes** (offset 36): the single index language, shared by the stemmer (`bit0`) and
the stop-word list (`bit1`) — it is meaningful when **either** filter is on, and a filter on
with no language (`0`) is rejected at build. `0` = none; `1`–`18` name a Snowball language, per
the `languages!` table in `rust/src/terms.rs` (the source of truth): `1` English, `2` Spanish,
`3` Arabic, `4` Danish, `5` Dutch, `6` Finnish, `7` French, `8` German, `9` Greek,
`10` Hungarian, `11` Italian, `12` Norwegian, `13` Portuguese, `14` Romanian, `15` Russian,
`16` Swedish, `17` Tamil, `18` Turkish. The stemmer is built only for `bit0` (so an index can
strip a language's stop words without stemming); an unknown byte ⇒ no stemmer / no stop list.

**Router FST** — `routerLen` bytes at offset `40`. A minimized FST
([BurntSushi `fst`](https://docs.rs/fst)) mapping **each dict block's last term (UTF-8) →
`(blockOff << 24) | blockLen`**, where `blockOff` is the block's byte offset relative to
`dictStart` (40 bits → 1 TB) and `blockLen` its byte length (24 bits; the cap is ≪ 16 MB).
Downloaded once at boot and held in memory — O(#blocks), a few MB even for tens of millions
of terms. To find the block that could contain a term `t`, take the first router entry whose
key `>= t` (`range().ge(t)`): blocks are contiguous and sorted, so that block is the only one
that can hold `t`.

**Dict blocks** — at `dictStart = 40 + routerLen`, length `dictLen`. A contiguous sequence
of byte-capped, **front-coded** blocks. A block is a run of entries scanned to its
`blockLen`-bounded end; each entry is:
```
[ shared    : uvarint ]   bytes shared with the previous term in the block (0 for the first)
[ suffixLen : uvarint ]   length of the suffix bytes
[ suffix    : bytes   ]   term = prevTerm[..shared] + suffix
[ headOffΔ  : uvarint ]   head_off delta from the previous entry (first entry: absolute)
[ headSize  : uvarint ]   head posting byte length
```
The first entry holds the block's first term in full; the block's last term is the router
key. Comparisons and shared-prefix lengths are over UTF-8 **bytes** (the sorted `BTreeMap` /
FST order the builder drains). `head_off` is the term's posting-block offset within the
postings region; it increases in term order, so its in-block delta is non-negative.
Front-coding compresses the long shared prefixes of scholarly vocabulary.

**Postings region** — at `postingsStart = dictStart + dictLen`. One block per term (located
by `head_off`):
```
[ tail_size : u32 LE ][ head bytes ][ tail bytes ]
```
- `head` = the posting restricted to docs `[0, headBoundary)` (a portable RoaringBitmap)
- `tail` = the posting restricted to docs `[headBoundary, ∞)` (a portable RoaringBitmap)

`headSize` (from the dict entry) gives the head's length; the 4-byte `tail_size` prefix
gives the tail's, so one ranged read of `head_off .. head_off + 4 + headSize` fetches the
head **and** learns where the tail ends. Doc IDs are assigned at build time in **descending
rank (popularity)**, so the head holds the `headBoundary` most-popular docs — the top-K for
any ranked query lives in the head. This region is byte-identical to `RRS`'s postings.

## Reader
- **boot:** read header (40 B) + the router FST (`routerLen` B); keep the router +
  `headBoundary` in memory. `dictStart = 40 + routerLen`; `postingsStart = dictStart + dictLen`.
- **locate(term):** `router.range().ge(term)` → first `(lastTerm, packed)` → block byte range
  `[dictStart + blockOff, blockLen)` → range-fetch that block → front-coded scan for `term`
  → `(head_off, headSize)`. One ranged dict-block read; absent term → `None`.
- **head(term):** read `[postingsStart + head_off, + 4 + headSize)`; first 4 B = `tail_size`,
  next `headSize` B = the head posting.
- **tail(term):** read `[postingsStart + head_off + 4 + headSize, + tail_size)` (only if the
  head doesn't yield K).

## Query
1. `terms = tokenize(query)` (dedup); empty → no results.
2. **dict-block wave:** map each term to its block (resident router, no fetch), range-fetch
   the distinct blocks concurrently, scan each for its term. Any absent term ⇒ strict AND is
   empty ⇒ no results.
3. **head wave:** fetch all heads concurrently; AND smallest-cardinality-first; iterate
   ascending → first K doc IDs.
4. **tail wave (lazy):** if fewer than K and any tail is non-empty, fetch the tails and AND
   the full `(head | tail)` postings.

**Prefix / autocomplete** range-fetch the dict blocks spanning the prefix (the router finds
the first block; the scan walks forward across blocks until a term sorts past the prefix),
then union the matching terms' postings. **Fuzzy / substring** is *not* a term-index
operation — route typo-tolerant queries to the trigram `RRS` index, which composes over the
same doc-ID space.

## Tokenizer — reader and builder must match (build/query symmetry)
`tokenize(text)` mirrors Tantivy's `SimpleTokenizer` + `LowerCaser`:
- a token is a maximal run of `char::is_alphanumeric` characters;
- each character is lowercased via `char::to_lowercase`.

The same function tokenizes the indexed text (builder) and the query (reader), so a query
resolves to the same terms that were indexed. Optional filters then run **after** this base
step, recorded in the header so the reader applies them identically: **stop-word removal**
(`flags bit1`) then **Snowball stemming** (`flags bit0`). Both key on the single `language`
byte (`1`–`18`, see **Language bytes** above) and are **independent** — an index can strip a
language's stop words without stemming, stem without stripping, or both; either filter
requires a language. Stop words come from per-language sorted lists embedded from
`stopwords/<lang>.txt` (the same files the Go port embeds, so the lists are byte-identical);
stemming is via the pure-Rust `rust-stemmers`, the same crate Tantivy uses. Both are wasm-safe,
so the browser filters/stems queries too. **Test vectors:** `"Machine-Learning, FAST!"` →
`["machine","learning","fast"]`; `"GPT-4 and BERT"` → `["gpt","4","and","bert"]`.

## Status
v2: exact whole-word AND + prefix/autocomplete with the head/tail rank split, over a blocked
front-coded dictionary range-fetched on demand. Native builder `terms_build`
(`write_term_index` / `TermIndexBuilder`); reader `terms::TermIndex` (wasm-safe); shared
front-coding codec `terms_dict`. All behind the non-default `terms` Cargo feature. Fuzzy is
delegated to the trigram `RRS` index. See `tasks/009_rrti_blocked_dictionary.done.md` and
`tasks/005_fst_term_index.done.md`.
