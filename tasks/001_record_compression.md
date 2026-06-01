# 001 — Optional record-store compression

Status: **implemented** (behind the `zstd` Cargo feature). Records can be stored
zstd-compressed against a shared dictionary in the additive **version-2** record
layout (`[1-byte tag][payload]` per record, dictionary shipped as a `*.dict`
sidecar). The raw **version-1** store is unchanged and reads byte-for-byte. This
changes neither the `RRSI` index nor the `RRSF` facet format. See
[`RECORDS.md`](../RECORDS.md) for the on-disk format. Build APIs:
`build::write_records_zstd`, `build::RecordWriter::new_zstd`,
`build::train_record_dict`; reader APIs: `RecordStore::open_with_dict` /
`with_dict` and the wasm `RrsRecords.openWithDict` /
`RrsCatalog.openRecordsWithDict`. The decode path uses pure-Rust `ruzstd`
(wasm-compatible); the native encode/train path uses the C `zstd` crate.

## Why deferred
At current scales the savings are a few GB → a few cents/month, and the `.rrs`
index dominates total storage. Revisit if record-store size or egress becomes a
real number (e.g. the full ~250M-work corpus).

## Design (decided)
- **Shared zstd dictionary, not big blocks.** Records are small (~165 B) and
  similar (repeated JSON keys, common venues/authors). A trained dictionary
  (`zstd --train`) recovers big-block ratio on small units, sidestepping fetch
  amplification.
- **Keep units small** — per-page (~16–64 records) or per-record. Large blocks
  compress marginally better but over-fetch on sparse AND-query results (scattered
  doc IDs land in many blocks). Page-aligned blocks fit the common
  consecutive-rank page; the dictionary covers the sparse case.
- **Decompress in the wasm `RecordStore`,** not JS: the codec compiles into the
  reader (pure-Rust `ruzstd` if it supports dictionary decode, else a small
  zstd-wasm), so it doesn't depend on browser codec support and the app still just
  gets plain bytes back.
- Ship the dictionary once at boot, like the sparse index. Measured gzip ratio on
  real records ≈ 2.4×; dict-zstd should match or beat it on small units.

## Touches
- `build::write_records` → an optional compressed variant (dictionary + blocks).
- `RecordStore::get`/`get_many` → inflate the block, slice out the record.
- `RECORDS.md` → document the compressed layout + dictionary.
- Encoding stays the app's choice; the library only frames and (optionally) inflates.
