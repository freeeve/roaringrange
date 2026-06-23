# Task 049 — Go records zstd encode (`WriteRecordsZstd`) — DONE

Go `WriteRecords` wrote only **raw** (tag-0) records; the Rust `write_records_zstd`
(`rust/src/build.rs`) writes tag-1 zstd frames against a shared trained dictionary
(the chunked-zstd full-corpus format — why the 115 GB store is Rust-built today).
This adds the zstd encode + dictionary-backed decode path to `go/`.

## Decision: round-trip conformance, not byte-identical

Byte-identical zstd is a clone-the-implementation problem (libzstd's match-finder /
optimal-parser / entropy-table / dict-training heuristics, undocumented and
version-drifting — klauspost/compress does not reproduce them). But the record
store only needs **decodability**: valid zstd frames + correct `.idx` offsets + the
shared dict. So the Go builder uses **klauspost/compress (pure Go — module stays
cgo-free)** and conformance is round-trip across encoders, not golden bytes.

## What shipped

- `go/records_zstd.go`:
  - `WriteRecordsZstd(bin, idx, records, dict)` — version-2 framed store; each
    record framed `[tag][payload]`, smaller of `[1][zstd frame]` / `[0][raw]`
    (klauspost `WithEncoderDict`, `SpeedBestCompression`); zero-length stays
    zero-length (no tag). Mirrors the Rust `RecordWriter::frame_zstd` decision.
  - `OpenRecordStoreWithDict(idx, bin, dict)` — attaches a klauspost
    dictionary-backed decoder so tag-1 frames inflate at `Get` (mirror of Rust
    `RecordStore::open_with_dict`); plain `OpenRecordStore` still errors
    `ErrCompressedRecord` on tag-1. The decoder is wired through a small
    `recordDecompressor` interface in `records.go`, keeping the klauspost import
    isolated to `records_zstd.go`.

- Conformance (both cross-encoder directions proven against committed fixtures):
  - `gen_records_zstd_fixture` example writes the shared `records_corpus.bin`,
    trained `records.dict`, and the libzstd-built `records_rust_zstd.{bin,idx}`.
  - Go `TestWriteRecordsZstdRoundTrip` — klauspost encode → Go decode (+ asserts
    version-2 and a real tag-1 frame); `RR_UPDATE_FIXTURES=1` writes
    `records_go_zstd.{bin,idx}`.
  - Go `TestOpenRecordStoreWithDictReadsRustStore` — **libzstd encode → klauspost
    decode**.
  - Rust `go_built_zstd_store_reads_back_through_ruzstd` — **klauspost encode →
    production ruzstd decode** (the integration risk that was flagged: dictID /
    frame-header compatibility — confirmed working).
  - Go `TestOpenRecordStoreCompressedWithoutDict` — tag-1 errors cleanly with no
    dict.

## Not done (out of scope, intentionally)

- No Go **dictionary trainer**. ZDICT/COVER training is not in klauspost; the dict
  is produced once by the Rust `train_record_dict` and shipped as the `*.dict`
  sidecar (both encoders/decoders consume it). A full-corpus Go *builder* that
  needs to train its own dict would still call out to libzstd — but nothing in the
  pipeline requires that today.
