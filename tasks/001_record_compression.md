# 001 — Optional record-store compression

Status: **deferred.** Records are stored uncompressed. The record store is an
opaque container, so this is purely additive — it changes neither the `RRSI`
index nor the `RRSF` facet format.

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
