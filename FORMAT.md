# RRS — roaring range search index (`RRSI`, version 1)

Range-fetchable layout for a [roaringsearch](https://github.com/freeeve/roaringsearch)
trigram index. Designed so a browser queries a multi-million-doc index over HTTP Range
requests with no backend: ~tens of KB one-time boot, then a few small ranged reads per
query — independent of corpus size. Measured on a 9.6M-doc corpus: 51 KB boot, <200 KB
worst query, 2–14 µs compute.

All integers little-endian. Postings are standard **portable** RoaringBitmaps
(Go `bm.ToBytes()` ⇄ Rust `RoaringBitmap::deserialize_from` — same spec, zero glue).

## Layout
**Header — 16 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSI"` |
| version | u16 | 2 | `1` |
| gramSize | u16 | 2 | `3` |
| ngrams | u32 | 4 | dictionary entry count |
| stride | u32 | 4 | sparse-index stride (e.g. 512) |

**Sparse index** — `sparseCount = ceil(ngrams/stride)` entries × 8 B, at offset `16`:
each entry is `key u64` = `dict[i*stride].key`. Downloaded once (tens of KB) and cached.

**Dictionary** — `ngrams` × 24 B, **sorted by key asc**, at `dictStart = 16 + sparseCount*8`:
| field | type | bytes |
|---|---|---|
| key | u64 | 8 |
| headOffset | u64 | 8 | absolute file offset of the head posting |
| headSize | u32 | 4 |
| tailSize | u32 | 4 |

Tail is at `headOffset+headSize`, length `tailSize`. `fullSize = headSize + tailSize`.

**Postings** — at `postingsStart = dictStart + ngrams*24`; per entry in dict order:
`[ head bytes ][ tail bytes ]`, where
- `head` = the posting restricted to docs `[0,65536)` (the first roaring container)
- `tail` = the posting restricted to docs `[65536, ∞)`

Both are independently-deserializable portable RoaringBitmaps. Doc IDs are assigned at
build time in **descending rank (popularity)**, so the head holds the 65,536 most-popular
docs — i.e. the top-K for any ranked query lives in the head (≤ ~8 KB).

## Reader
- **boot:** read header (16 B) + sparse index (`sparseCount*8` B); keep keys in memory.
- **lookup(key):** `b` = largest `i` with `sparseKeys[i] <= key` (in-memory binary search) →
  read dict block `[dictStart + b*stride*24, + min(stride, ngrams-b*stride)*24)` →
  binary-search the block for `key` → `(headOffset, headSize, tailSize)`.
- **head(key):** read `[headOffset, headOffset+headSize)` (top-K candidates).
- **tail(key):** read `[headOffset+headSize, +tailSize)` (only if the head doesn't yield K).

## Query
1. `keys = NgramKeys(query, gramSize)`; empty → no results.
2. per key: `lookup` → `head()` → deserialize roaring.
3. AND heads (smallest cardinality first); iterate ascending → first K doc IDs.
4. if fewer than K, fetch `tail()`s and continue.

## n-gram key — reader must match the builder byte-for-byte (from roaringsearch `ngram.go`)
`normalize(s)`: keep Unicode letters/digits, lowercase each rune. Per `gramSize`-rune window:
- `n ≤ 2`: pack 32 bits/rune — `key = (key<<32) | rune`
- `n` in 3..8, all ASCII (≤ 0x7F): pack 8 bits/rune — `key = (key<<8) | rune`
- else: FNV-1a over each rune's 4 LE bytes (`r&0xFF, (r>>8)&0xFF, (r>>16)&0xFF, (r>>24)&0xFF`);
  offset `14695981039346656037`, prime `1099511628211`.

Dedup keys per query. **Test vectors:** `"abc"` → `[6382179]`; `"A-b!C"` → `[6382179]`;
`"aaaa"` → 1 key; `"ab"` → `[]`.
