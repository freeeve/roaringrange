# RRS faceting — optional filter + sort sections

Faceting lets a query be narrowed by categorical attributes (`language`, `format`,
owning `library`, …) and sorted by stored values, **without a backend** — the same
range-fetch model as the trigram index in [`FORMAT.md`](FORMAT.md).

A facet field maps each category value to a RoaringBitmap of doc IDs, exactly like
roaringsearch's [`BitmapFilter`](https://github.com/freeeve/roaringsearch). Those
doc IDs live in the **same popularity-ordered space** as the text postings, so facet
postings split into head/tail and range-fetch identically.

This is shipped in two phases:

- **Phase 1 (implemented): sidecar `RRSF` file.** A companion `*.rrf` file fetched
  alongside the `RRS2` text index. The text index is untouched; the reader opens two
  resources. Smallest end-to-end slice, no change to the frozen `RRS2` contract.
- **Phase 2 (planned): unified `RRS3` section container.** One file with a tiny
  section directory (`TEXT` / `FACETS` / `SORTCOLS` / `RECORDS`), so a single atomic,
  versioned object holds everything. The `FACETS` section body equals the `RRSF`
  body below; only the framing changes.

All integers little-endian. Postings are standard **portable** RoaringBitmaps
(Go `bm.ToBytes()` ⇄ Rust `RoaringBitmap::deserialize_from`).

## `RRSF` sidecar layout

**Header — 24 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSF"` |
| version | u16 | 2 | `1` |
| reserved | u16 | 2 | `0` |
| fields | u32 | 4 | facet field count |
| cats | u32 | 4 | total category count across all fields |
| strBytes | u32 | 4 | length of the string blob |
| reserved2 | u32 | 4 | `0` |

**Field table** — `fields` × 16 B, at offset `24`, in field order:
| field | type | bytes | notes |
|---|---|---|---|
| nameOff | u32 | 4 | offset into string blob |
| nameLen | u16 | 2 | name length in bytes |
| pad | u16 | 2 | `0` |
| catStart | u32 | 4 | index of this field's first category in the category table |
| catCount | u32 | 4 | number of categories in this field |

**Category table** — `cats` × 36 B, grouped by field; within a field **sorted by key asc**:
| field | type | bytes | notes |
|---|---|---|---|
| key | u64 | 8 | `hash(field, category)` (see below) |
| headOff | u64 | 8 | absolute file offset of the head posting |
| headSize | u32 | 4 | |
| tailSize | u32 | 4 | tail at `headOff+headSize`, length `tailSize` |
| cardinality | u32 | 4 | full-corpus doc count for this category (free facet count) |
| nameOff | u32 | 4 | offset into string blob |
| nameLen | u16 | 2 | |
| pad | u16 | 2 | `0` |

**String blob** — `strBytes` B of UTF-8, at `strBlobOff = 24 + fields*16 + cats*36`.
Field and category display names; sliced by the `nameOff`/`nameLen` pairs above.

**Postings** — at `postingsStart = strBlobOff + strBytes`; per category in table
order: `[ head bytes ][ tail bytes ]`, where
- `head` = the category posting restricted to docs `[0, 65536)`,
- `tail` = the category posting restricted to docs `[65536, ∞)`.

Identical split to the text index (`FORMAT.md`). A category covering millions of
docs still has a ≤ ~8 KB head, so filtering the visible top-K is cheap.

## key — `hash(field, category)`

FNV-1a 64-bit over the bytes of `lower(field)`, then `0x1f`, then `lower(category)`
(`lower` = Unicode lowercase). Offset `14695981039346656037`, prime
`1099511628211`. Forward-compatible with the Phase-2 sparse-index-by-key; the
Phase-1 sidecar reader resolves a selection by **name** (it holds the whole category
table in memory), so the key is informational there.

## Reader

- **boot:** read the 24 B header → know `fields`, `cats`, `strBytes` →
  `metaLen = 24 + fields*16 + cats*36 + strBytes` → read `[0, metaLen)` once and keep
  the field table, category table, and string blob in memory. For low-cardinality
  fields (`format` ~5, `language` ~80) this is a few KB. (High-cardinality fields like
  `library` get a sparse-index-by-key in Phase 2 so boot stays small.)
- **list / counts:** `categories(field)` and unfiltered `cardinality` come straight
  from memory — **zero postings fetched.**
- **filter:** resolve each selected `(field, category)` to its category entry → fetch
  the **head** posting → combine. One ranged read per selected category.
- **filtered facet counts (live):** `count[cat] = (resultBitmap AND catHead).len()` —
  one head fetch per category. Gate by cardinality: auto-count low-card fields only;
  high-card fields are filter-only.

## Filter semantics — mirrors `BitmapFilter`

Within a field, selected categories are **OR**ed (`GetAny`); fields are **AND**ed:

```
filter = AND over selected fields ( OR over that field's selected categories )
result = textMatch AND filter
```

The reader applies this to the head first (cheap), then — only if pagination crosses
into the tail — fetches the selected categories' tails and applies the same
OR/AND to the tail before extending the cursor. A field with a selection that
resolves to no docs makes the whole result empty, which is correct.

## `SORTCOLS` (planned, Phase 2) — mirrors roaringsearch `SortColumn[T]`

A dense, doc-ID-indexed columnar array for an alternate sort key (rating, pub-date):

```
header   magic "RRSC"; version u16; colCount u16
columns  colCount × { nameOff u32; nameLen u16; valueType u8 (u16/u32/i32/f32); dataOff u64; count u32 }
data     per column: dense values[docID], fixed width, length = count × width
```

A candidate doc's value sits at `dataOff + docID*width`, so the reader coalesce-fetches
runs for a result set (like the record store) and heap-sorts top-K. The primary
popularity order is already baked into doc-ID assignment; this is for secondary keys.
