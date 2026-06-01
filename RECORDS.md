# RRSR — record store (`RRSR`, version 1 and 2)

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
| version | u16 | 2 | `1` = raw, `2` = framed (see below) |
| reserved | u16 | 2 | `0` |
| count | u32 | 4 | number of records `N` |
| reserved2 | u32 | 4 | `0` |

**Offsets** — `N+1` × `u64` at offset `16`: record `d` is
`bin[off[d] .. off[d+1]]`, and the pair `(off[d], off[d+1])` sits at
`idx[16 + d*8 .. 16 + (d+2)*8]`.

## Versions — raw (1) vs framed/compressed (2)
The offset index is identical in both versions; only how the **payload** in
`bin[off[d]..off[d+1]]` is interpreted differs, keyed by the header `version`:

- **version 1 (raw, the original format):** the payload *is* the record bytes,
  untagged. Reads back byte-for-byte with any build of the reader — no codec is
  involved. A currently-deployed store stays valid unchanged.
- **version 2 (framed):** each non-empty payload is `[1-byte tag][payload]`:
  - **tag `0` (raw):** `payload` is the record bytes verbatim. The reader strips
    the tag and returns them — no codec needed, so this works with the reader's
    `zstd` feature off.
  - **tag `1` (zstd + shared dict):** `payload` is a standard zstd frame
    compressed against a **shared dictionary** (see below). The reader strips the
    tag and inflates the frame with the dictionary. Requires the reader's `zstd`
    feature; a tag-1 frame read without it (or with no dictionary set) returns a
    clear error and never panics.
  - A **zero-length** record stays zero-length (no tag byte), matching the
    version-1 zero-length convention.

  The writer keeps a record raw (tag 0) whenever compression would not shrink it,
  so a record never grows.

## `*.dict` — shared zstd dictionary (sidecar, version 2 only)
A version-2 compressed store ships a trained zstd dictionary in a **sidecar**
file (`<name>.dict`) — it is *not* embedded in the `.bin`. The reader fetches it
once at boot (like the index's sparse block) and passes it to
`RecordStore::open_with_dict` / `with_dict`; the browser passes the bytes to the
wasm `RrsRecords.openWithDict` / `RrsCatalog.openRecordsWithDict`. The same
dictionary must be used to write (`build::write_records_zstd`) and to read. Train
one from representative records with `build::train_record_dict`. Records are small
and self-similar, so a trained dictionary recovers big-block compression ratio on
per-record units without the over-fetch of large blocks.

## Reader
- **lookup(d):** read 16 B at `16 + d*8` → `(start, end)` → one ranged read of
  `bin[start, end)`. Two ranged reads per record; a page of consecutive doc IDs
  coalesces to one index read + one blob read.

Written by Rust `build::write_records` / `RecordWriter` or Go
`roaringrange.WriteRecords` / `RecordWriter`, read by `RecordStore` (in both the
Rust reader crate and the Go package). The two writers emit byte-identical
output — a Go-build → Rust-read round-trip is pinned by the Go
`TestWriteRecordsGoldenLayout` golden offsets and the Rust
`reads_go_written_rrsr_golden_bytes` test, which read the same bytes. The record
*schema* is intentionally not part of the format — only the container is.

## Optional compression
Per-record **zstd with a shared dictionary** is implemented as the additive
**version 2** layout above (see [`tasks/001_record_compression.md`](tasks/001_record_compression.md)),
inflated inside the `RecordStore` reader. It is opt-in: the raw version-1 store is
unchanged and the reader reads it byte-for-byte. The index/facet formats are
unaffected; the record encoding stays the application's choice.

The reader's decode path uses the pure-Rust `ruzstd` decoder, so it compiles for
both native and `wasm32` (the C `zstd` crate, used only for the native build-side
encode and dictionary training, does not build cleanly for wasm). All zstd code is
behind the crate's `zstd` Cargo feature; the crate builds and tests with the
feature off exactly as before. Build the browser reader that must inflate records
with `wasm-pack build --target web --features "wasm zstd"`.
