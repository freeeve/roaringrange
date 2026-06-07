# RRS — roaring range search index (`RRSI`, version 3)

Range-fetchable layout for a [roaringsearch](https://github.com/freeeve/roaringsearch)
trigram index. Designed so a browser queries a multi-million-doc index over HTTP Range
requests with no backend: ~tens of KB one-time boot, then a few small ranged reads per
query — independent of corpus size.

All integers little-endian. Postings are standard **portable** RoaringBitmaps
(Go `bm.ToBytes()` ⇄ Rust `RoaringBitmap::deserialize_from` — same spec, zero glue).

**v3 vs v2.** v3 collapses each term's separate `[head][tail]` blobs into **one posting per
term** and drops the header's `headBoundary`, shrinking the dict entry 24 → 20 B. The head was a
build-time `[0, headBoundary)` prefix stored separately and directly addressed for the instant
first page; v3 derives that prefix from the single posting's container directory instead (the
reader's *eager prefix*), since container-level ranged reads (`TailScan`) already page any bucket.
Doc IDs are still assigned in **descending rank (popularity)**, so a posting's leading container
buckets are the most-popular docs.

## Layout
**Header — 16 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSI"` |
| version | u16 | 2 | `3` |
| gramSize | u16 | 2 | `3` |
| ngrams | u32 | 4 | dictionary entry count |
| stride | u32 | 4 | sparse-index stride (e.g. 512) |

**Sparse index** — `sparseCount = ceil(ngrams/stride)` entries × 8 B, at offset `16`:
each entry is `key u64` = `dict[i*stride].key`. Downloaded once (tens of KB) and cached.

**Dictionary** — `ngrams` × 20 B, **sorted by key asc**, at `dictStart = 16 + sparseCount*8`:
| field | type | bytes | notes |
|---|---|---|---|
| key | u64 | 8 | |
| offset | u64 | 8 | absolute file offset of the posting |
| size | u32 | 4 | posting length in bytes |

**Postings** — at `postingsStart = dictStart + ngrams*20`; per entry in dict order: one
independently-deserializable portable RoaringBitmap of the term's docs at `[offset, offset+size)`.
A portable bitmap is a sorted directory of containers keyed by `doc >> 16` (a 64K-doc "bucket")
with an offset table, so a reader can range-read individual buckets without the whole posting.

## Reader
- **boot:** read header (16 B) + sparse index (`sparseCount*8` B); keep keys in memory.
- **lookup(key):** `b` = largest `i` with `sparseKeys[i] <= key` (in-memory binary search) →
  read dict block `[dictStart + b*stride*20, + min(stride, ngrams-b*stride)*20)` →
  binary-search the block for `key` → `(offset, size)`.
- **posting(key):** read `[offset, offset+size)` and deserialize.
- **eager prefix:** the cursor fetches the first container bucket (docs `[0, 65536)` — the
  top-ranked candidates, matching the `RRSF` facet head boundary) of each query term for the
  instant first page + facet counts, then pages the buckets at or above it via container-level
  ranged reads (`TailScan`), fetching only the buckets a page spans — never the whole posting.

## Query
1. `keys = NgramKeys(query, gramSize)`; empty → no results.
2. per key: `lookup` → fetch the eager prefix → deserialize roaring.
3. AND the eager prefixes (smallest cardinality first); iterate ascending → first K doc IDs.
4. if fewer than K, page the postings' higher buckets in rank order and continue (a strict AND
   reads only the containers each page spans; a facet filter is applied per bucket).

## n-gram key — reader must match the builder byte-for-byte (from roaringsearch `ngram.go`)
`normalize(s)`: keep Unicode letters/digits, lowercase each rune. Per `gramSize`-rune window:
- `n ≤ 2`: pack 32 bits/rune — `key = (key<<32) | rune`
- `n` in 3..8, all ASCII (≤ 0x7F): pack 8 bits/rune — `key = (key<<8) | rune`
- else: FNV-1a over each rune's 4 LE bytes (`r&0xFF, (r>>8)&0xFF, (r>>16)&0xFF, (r>>24)&0xFF`);
  offset `14695981039346656037`, prime `1099511628211`.

Dedup keys per query. **Test vectors:** `"abc"` → `[6382179]`; `"A-b!C"` → `[6382179]`;
`"aaaa"` → 1 key; `"ab"` → `[]`.
