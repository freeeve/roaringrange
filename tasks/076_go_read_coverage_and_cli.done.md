# 076 — Full Go read coverage + `roaringrange` CLI inspector

## Goal

Add a Go CLI to inspect any roaringrange file (dump / convert to JSON). Before the
CLI, the Go side could only *read* two formats (RRSI via `Open`, RRSR via
`OpenRecordStore`); every other format had a writer but no reader. This task
completes the Go read side first, then adds a thin dependency-free CLI over it.

## Part A — library readers (DONE)

One `<format>_read.go` beside each writer, each mirroring `Open` (io.ReaderAt,
validate header, keep the small directory/router resident, range-read bodies,
bound untrusted lengths via `boundedRead`). Each parses back into the SAME structs
its writer consumes, so `Write*(Open*(x).ReadAll()) == x`.

- `format.go` — magic→format registry (self-registration via init), `FileInfo`
  header view, `boundedRead`/`readHeader`/`u16/u32/u64` shared helpers,
  `DetectFormat`/`OpenHeader`.
- `lookup_read.go` (RRIL), `sortcols_read.go` (RRSC), `hotcache_read.go` (RRHC),
  `splitset_read.go` (RRSS), `facets_read.go` (RRSF), `bm25_read.go` (RRSB),
  `vector_read.go` (RRVI + RRVR), `terms_read.go` (RRTI: router FST + front-code
  decode + posting split; tokenizer rebuilt from header flags).

Testing per reader: Tier-1 round-trip against the writer + Tier-2 decode of the
Rust-authored `testdata/*_build_golden.txt` where one exists (rril/rrsc/rrhc/rrss/
rrsb/rrvi). All green (`go test ./...`).

## Part B — CLI `cmd/roaringrange/` (DONE)

Plain package under the root module, stdlib `flag` (with an interleaving parser so
flags may follow positionals), subcommand dispatch. `info_builtin.go` registers the
pre-existing RRSI/RRSR readers so all 12 formats auto-detect.
- `info <file>` — auto-detect magic, print `FileInfo` (text/`--json`).
- `dump <file>` — full structural dump as JSON, `--limit/--offset` paging,
  `--postings` to include bitmap/vector contents.
- `records <idx> <bin> [--dict d] [--id N | --range a-b]` — decode record store.
- `get <file>` — single-key lookup (`--key`/`--term`/`--id`/`--head-off`).

CLI smoke test (`main_test.go`) builds the binary and drives info/dump over the
goldens. Verified end-to-end against materialized golden fixtures for every format
(incl. stemmed-Unicode RRTI terms and zstd-dict RRSR records).

## Verification

`go test ./...`; `go build ./cmd/roaringrange`; drive `info`/`dump` over the golden
fixtures and a real downloaded index split.
