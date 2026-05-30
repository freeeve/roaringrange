# RRSR — record store (`RRSR`, version 1)

A search over the `RRS` index (see [`FORMAT.md`](FORMAT.md)) returns ranked doc
IDs; the record store maps each doc ID back to its stored fields, so the whole
**search → details** path runs client-side over HTTP Range with no backend.

Records are **opaque** to the library — the application chooses the encoding
(JSON, msgpack, …). The store only frames them for O(1) lookup by doc ID. Two
files, both range-fetchable.

All integers little-endian.

## `*.bin` — record blob
The record bytes concatenated in **doc-ID order**, so a results page (a run of
consecutive ranks) is a contiguous slice.

## `*.idx` — offset index
**Header — 16 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSR"` |
| version | u16 | 2 | `1` |
| reserved | u16 | 2 | `0` |
| count | u32 | 4 | number of records `N` |
| reserved2 | u32 | 4 | `0` |

**Offsets** — `N+1` × `u64` at offset `16`: record `d` is
`bin[off[d] .. off[d+1]]`, and the pair `(off[d], off[d+1])` sits at
`idx[16 + d*8 .. 16 + (d+2)*8]`.

## Reader
- **lookup(d):** read 16 B at `16 + d*8` → `(start, end)` → one ranged read of
  `bin[start, end)`. Two ranged reads per record; a page of consecutive doc IDs
  coalesces to one index read + one blob read.

Written by `build::write_records`, read by `RecordStore` (the reader crate). The
record *schema* is intentionally not part of the format — only the container is.

## Future — optional compression
Records are stored uncompressed today. Optional per-page/per-record **zstd with a
shared dictionary** (inflated inside the `RecordStore` reader) is a planned,
additive option — see [`tasks/001_record_compression.md`](tasks/001_record_compression.md).
The index/facet formats are unaffected; the encoding stays the application's choice.
