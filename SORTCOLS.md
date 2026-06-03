# RRSC — sort columns (`RRSC`, version 1)

Optional, range-fetchable **dense columns indexed by doc ID** — the build-time
counterpart of an alternate sort key. A search over the [`RRS`](FORMAT.md) index
returns doc IDs in the primary static-rank order; an `RRSC` column lets the reader
fetch a stored value per doc and re-rank a *materialized* candidate set
client-side (sort by rating / publication date / any secondary metric), in the
same no-backend HTTP-Range model as the text and facet indexes.

The same container is also how a **secondary full index** maps back to the
primary doc-ID space: a one-column `u32` store where `primary[secondary_id]` is
the permutation `secondary_docid → primary_docid`. Because a result page is a
contiguous run of secondary doc IDs, that map is one ranged read per page (see
[`slice_u32`](#reader)).

All integers little-endian. Values are fixed-width and stored densely in doc-ID
order, so value `v` for doc `d` in column `c` sits at `dataOff[c] + d*width`.

## Layout

**Header — 16 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSC"` |
| version | u16 | 2 | `1` |
| colCount | u16 | 2 | number of columns |
| rows | u32 | 4 | `N` — doc count; every column has `N` values |
| strBytes | u32 | 4 | length of the name string blob |

**Column table** — `colCount` × 24 B, at offset `16`, in column order:
| field | type | bytes | notes |
|---|---|---|---|
| nameOff | u32 | 4 | offset into the string blob |
| nameLen | u16 | 2 | name length in bytes |
| valueType | u8 | 1 | `1`=u16, `2`=u32, `3`=i32, `4`=f32 |
| pad | u8 | 1 | `0` |
| dataOff | u64 | 8 | absolute file offset of this column's dense data |
| rows | u32 | 4 | `== header.rows` (redundant guard) |
| reserved | u32 | 4 | `0` |

`width(valueType)` = 2 for `u16`, else 4. A column's data spans
`rows * width` bytes.

**String blob** — `strBytes` B of UTF-8, at `strBlobOff = 16 + colCount*24`.
Column display names, sliced by the `nameOff`/`nameLen` pairs above.

**Data** — at `dataOff` per column (the writer lays them out contiguously right
after the string blob, in column order): `rows` fixed-width values in doc-ID
order. Value for doc `d` is `data[d*width .. d*width + width]`.

## Reader
- **boot:** read the 16 B header, then the meta region
  `[0, 16 + colCount*24 + strBytes)` once and keep the column table + names in
  memory. The dense data — the bulk of the file — is **never** read at boot; it is
  range-fetched per query. Boot is a few KB regardless of `N`.
- **value(d) / values(ids):** read `width` bytes at `dataOff + d*width`. A batch of
  doc IDs (a candidate set) is sorted by offset and coalesced into a few spans
  (bridging small gaps) fetched in one concurrent wave, mirroring the container
  coalescing in [`posting.rs`](rust/src/posting.rs) and `RecordStore::get_many`.
- **slice_u32(start, len):** for a `u32` column, one ranged read of the contiguous
  run `[start, start+len)` — the permutation-page fast path. Clamps to `rows`.
- **topk(candidates, k, descending):** fetch the candidates' values and partial-sort,
  tie-broken by ascending doc ID (so equal secondary values keep primary-rank
  order — e.g. "newest, then most-cited"). Returns the reordered doc IDs.

## Notes
- A column carries no null/absent marker; a doc with no value uses a sentinel the
  application chooses (e.g. `0`). The container frames fixed-width values only.
- Written by Rust `build::write_sortcols` (and `build::write_perm` for the
  one-column `u32` permutation), read by `SortCols`. Like the `RRIL` lookup, this
  is currently Rust-only; a Go writer/reader + `go/conformance/` entry can follow.
