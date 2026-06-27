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
**Header — 16 B (v3) / 18 B (v4)**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSI"` |
| version | u16 | 2 | `3` (case-folding) or `4` (case-sensitive) |
| gramSize | u16 | 2 | `3` |
| ngrams | u32 | 4 | dictionary entry count |
| stride | u32 | 4 | sparse-index stride (e.g. 512) |
| flags | u16 | 2 | **v4 only**, at offset 16: `bit0` = case-sensitive; rest reserved (`0`) |

**v3 vs v4 (case normalization).** A default index lowercases each n-gram rune (`normalize`)
and is **v3**, byte-identical to before this flag existed. Building with case normalization OFF
keys on the original case and emits **v4**: the v3 fields plus a trailing 2-byte `flags` field at
offset 16 (no v3 field shifts; the sparse index then starts at offset 18). The reader accepts both
and derives `caseFold = !(flags & 1)`; queries must derive keys the same way
(`NgramKeysWith(query, gramSize, caseFold)`). `headerSize` below is `16` for v3, `18` for v4.

**Sparse index** — `sparseCount = ceil(ngrams/stride)` entries × 8 B, at offset `headerSize`:
each entry is `key u64` = `dict[i*stride].key`. Downloaded once (tens of KB) and cached.

**Dictionary** — `ngrams` × 20 B, **sorted by key asc**, at `dictStart = headerSize + sparseCount*8`:
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
- **boot:** read header (`headerSize` B; for v4 also read the 2-byte `flags`) + sparse index
  (`sparseCount*8` B); keep keys in memory.
- **lookup(key):** `b` = largest `i` with `sparseKeys[i] <= key` (in-memory binary search) →
  read dict block `[dictStart + b*stride*20, + min(stride, ngrams-b*stride)*20)` →
  binary-search the block for `key` → `(offset, size)`.
- **posting(key):** read `[offset, offset+size)` and deserialize.
- **eager prefix:** the cursor fetches the first container bucket (docs `[0, 65536)` — the
  top-ranked candidates, matching the `RRSF` facet head boundary) of each query term for the
  instant first page + facet counts, then pages the buckets at or above it via container-level
  ranged reads (`TailScan`), fetching only the buckets a page spans — never the whole posting.

## Query
1. `keys = NgramKeysWith(query, gramSize, caseFold)` (v3 / non-flagged ⇒ `caseFold = true`); empty → no results.
2. per key: `lookup` → fetch the eager prefix → deserialize roaring.
3. AND the eager prefixes (smallest cardinality first); iterate ascending → first K doc IDs.
4. if fewer than K, page the postings' higher buckets in rank order and continue (a strict AND
   reads only the containers each page spans; a facet filter is applied per bucket).

## n-gram key — reader must match the builder byte-for-byte (from roaringsearch `ngram.go`)
`normalize(s, caseFold)`: keep Unicode letters/digits; lowercase each rune **only when
`caseFold`** (a v3 index, or v4 with the case-sensitive bit clear — the default). A v4
case-sensitive index keeps the original case. Per `gramSize`-rune window:
- `n ≤ 2`: pack 32 bits/rune — `key = (key<<32) | rune`
- `n` in 3..8, all ASCII (≤ 0x7F): pack 8 bits/rune — `key = (key<<8) | rune`
- else: FNV-1a over each rune's 4 LE bytes (`r&0xFF, (r>>8)&0xFF, (r>>16)&0xFF, (r>>24)&0xFF`);
  offset `14695981039346656037`, prime `1099511628211`.

Dedup keys per query. **Test vectors:** `"abc"` → `[6382179]`; `"A-b!C"` → `[6382179]`;
`"aaaa"` → 1 key; `"ab"` → `[]`.
