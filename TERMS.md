# RRTI — roaring range term index (`RRTI`, version 1)

Range-fetchable **term-level** (whole-word) inverted index — an additive member of the
roaringrange family (next to the trigram `RRS`, vector `RRVI`, facet `RRSF`, record
`RRSR`, and lookup `RRIL`). It shares the same doc-ID space as the others, so it composes
with facets/records/vector unchanged. Where `RRS` keys a sorted-`u64` dictionary by
trigram, `RRTI` keys an **FST term dictionary** by whole word: a query word costs **one**
posting fetch instead of ~(L−2) trigram fetches, and the posting carries the word's true
rarity (small → selective in one shot).

Only the **dictionary + key** layer is new. The postings region is the `RRS`
`[head][tail]` roaring split verbatim, so the reader's posting/AND/head-tail path is
shared and doc IDs stay globally rank-ordered (top-K lives in the head).

All integers little-endian. Postings are standard **portable** RoaringBitmaps
(`RoaringBitmap::serialize_into` ⇄ `deserialize_from`).

## Layout

`[ header ][ FST dictionary ][ postings region ]`

**Header — 32 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRTI"` |
| version | u16 | 2 | `1` |
| flags | u16 | 2 | `bit0` = stemmed, `bit1` = stop-words removed; rest reserved (`0`) |
| termCount | u32 | 4 | distinct terms in the dictionary |
| headBoundary | u32 | 4 | doc-ID head/tail split — multiple of 65536, default 65536 |
| fstLen | u64 | 8 | byte length of the FST dictionary blob |
| language | u8 | 1 | stemmer language when `bit0` set (`1` = English); `0` otherwise |
| reserved | u8[7] | 7 | zero padding to 32 B |

**FST dictionary** — `fstLen` bytes at offset `32`. A minimized acyclic finite-state
transducer ([BurntSushi `fst`](https://docs.rs/fst)) mapping `term (UTF-8) → u64 output`.
Downloaded once at boot and held in memory, so resolving a term to its posting location
costs **zero** further reads. The `u64` output packs the term's posting block location:

```
output = (head_off << 24) | head_size
```
- `head_off` — byte offset of the term's posting block within the postings region
  (40 bits → up to 1 TB of postings)
- `head_size` — byte length of the head posting (24 bits → up to 16 MB, ample for one
  rank-head)

**Postings region** — at `postingsStart = 32 + fstLen`. One block per term (located by
`head_off`):
```
[ tail_size : u32 LE ][ head bytes ][ tail bytes ]
```
- `head` = the posting restricted to docs `[0, headBoundary)` (a portable RoaringBitmap)
- `tail` = the posting restricted to docs `[headBoundary, ∞)` (a portable RoaringBitmap)

`head_size` (from the FST) gives the head's length; the 4-byte `tail_size` prefix gives
the tail's, so one ranged read of `head_off .. head_off + 4 + head_size` fetches the head
**and** learns where the tail ends. Doc IDs are assigned at build time in **descending
rank (popularity)**, so the head holds the `headBoundary` most-popular docs — the top-K
for any ranked query lives in the head.

## Reader
- **boot:** read header (32 B) + the FST blob (`fstLen` B); keep the FST + headBoundary in
  memory. `postingsStart = 32 + fstLen`.
- **locate(term):** `fst.get(term)` → `output` (in-memory, no fetch) →
  `head_off = output >> 24`, `head_size = output & 0xFFFFFF`. Absent term → `None`.
- **head(term):** read `[postingsStart + head_off, + 4 + head_size)`; first 4 B = `tail_size`,
  next `head_size` B = the head posting.
- **tail(term):** read `[postingsStart + head_off + 4 + head_size, + tail_size)` (only if
  the head doesn't yield K).

## Query
1. `terms = tokenize(query)` (dedup); empty → no results.
2. per term: `locate` → if any term is absent, the strict AND is empty → no results.
3. fetch all heads (one concurrent wave); AND smallest-cardinality-first; iterate ascending
   → first K doc IDs.
4. if fewer than K and any tail is non-empty, fetch the tails and AND the full
   `(head | tail)` postings.

## Tokenizer — reader and builder must match (build/query symmetry)
`tokenize(text)` mirrors Tantivy's `SimpleTokenizer` + `LowerCaser`:
- a token is a maximal run of `char::is_alphanumeric` characters;
- each character is lowercased via `char::to_lowercase`.

The same function tokenizes the indexed text (builder) and the query (reader), so a query
resolves to the same terms that were indexed. Optional filters then run **after** this
base step, recorded in the header so the reader applies them identically: **stop-word
removal** (`flags bit1`) and **Snowball stemming** (`flags bit0`, language in the
`language` byte — `1` = English, via the pure-Rust `rust-stemmers`, the same crate
Tantivy uses; wasm-safe, so the browser stems queries too). The earlier note that these
were future work no longer applies (the `flags`
field records which were applied). **Test vectors:** `"Machine-Learning, FAST!"` →
`["machine","learning","fast"]`; `"GPT-4 and BERT"` → `["gpt","4","and","bert"]`.

## Status
v1: exact whole-word AND with the head/tail rank split. Native builder
`build::write_term_index`; reader `terms::TermIndex` (wasm-safe), both behind the
non-default `terms` Cargo feature. Planned: prefix scan (autocomplete) and fuzzy
(Levenshtein automaton ∩ FST) — both native to the FST; inline rare-term postings;
hot-phrase materialization; optional stemming. See `tasks/005_fst_term_index.in-progress.md`.
