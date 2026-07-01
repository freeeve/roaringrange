# 057: fix(go): harden Go readers against malicious index bytes (mirror 45fa50f)

**Severity: HIGH.** The Rust readers were hardened against malicious inputs in 45fa50f (+ mutation fuzz harness); the Go reference readers never got the same pass. Readers parse remote/untrusted data. Line refs @ 849f9c2.

## Findings

1. `records.go:150-169` (`RecordStore.Get`) -- `start`/`end` come straight from remote idx bytes; only `end < start` is checked (`records.go:160-161`), then `make([]byte, end-start)` runs with an attacker-controlled u64. `start=0, end=2^63` panics with `makeslice: len out of range` instead of returning `ErrTruncated`. Cap `end-start` against a sane max (or against the bin file size if known) and return `ErrTruncated`/`ErrMalformed`.
2. `reader.go:68-78` (`Open`) -- `ngrams` (u32) from the 16-byte header drives `make([]byte, sparseCount*8)`: ngrams=2^32-1 with stride=1 attempts ~34 GB -> unrecoverable OOM abort, not an error.
3. `reader.go:107` (`lookup`) -- `make([]byte, blockLen*dictEntry)` with stride=2^32-1 attempts ~80 GB.
4. `reader.go:135` (`Posting`) -- `make([]byte, rec.size)` up to 4 GB from one dict record.
5. 32-bit builds: `int(uint32)` conversions in the above can go negative -> panic.

## Fix direction

- Add plausibility caps/clamps at parse time (e.g. validate header-derived sizes against the file size where a size is available; otherwise a hard cap constant) and return typed errors, matching what the Rust side does post-45fa50f -- read `rust/src/index.rs` / `fetch.rs` hardening for the exact conventions (checked/saturating math, `Malformed` errors).
- No output-byte changes anywhere (writers untouched); this is reader-side validation only.

## Acceptance

- A Go mutation-fuzz harness over the reader parse paths (mirror the Rust harness added in 45fa50f): mutated headers/dict records/offset pairs must produce errors, never panics or multi-GB allocations. Wire into `go test` (fuzz corpus checked in).
- Existing goldens and conformance tests still pass byte-identical.

## Outcome (DONE)

Reader-side only; no format or output-byte changes (writers untouched, conformance
byte-identical).

- `reader.go`: added `maxReadBytes` (1 GiB) cap. `Open` computes the sparse-index
  layout in uint64 (no overflow / no negative length on 32-bit) and rejects a
  layout past the cap; `lookup` rejects a non-positive or over-cap dict block;
  `Posting` bounds the untrusted u32 `rec.size`. The findings' `end < start` /
  stride==0 guards were already present.
- `records.go`: added `maxRecordBytes` (64 MiB) + `maxDecompressedRecord` (64 MiB);
  `Get` bounds the offset-pair span before `make`.
- `records_zstd.go`: `OpenRecordStoreWithDict` sets `WithDecoderMaxMemory` so a
  zstd bomb errors instead of inflating unbounded (mirrors the Rust 64 MiB cap).
- `reader_fuzz_test.go` (new): targeted hostile-header regressions (sparse-index,
  dict-block, posting-size, record-span -- each proven to reject rather than
  allocate), deterministic single-mutation sweeps over valid RRS/RRSR fixtures
  asserting no panic, and native `FuzzRRSReader` / `FuzzRRSRecordStore` targets.
  Native fuzzing ran clean (349K RRS execs, 3.8M RRSR execs, no crashers).

Note: `RecordWriter` count-enforcement (finding A5 / the writer footgun) is tracked
separately in task 067 item 4; not part of this reader-hardening pass.
