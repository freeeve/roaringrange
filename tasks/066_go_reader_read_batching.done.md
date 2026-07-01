# 066: perf(go): batch/reuse ranged reads in the Go reader (dict blocks, records)

**Severity: MED (read count == S3 round trips).** Reader-side only; no output-byte changes. Line refs @ 849f9c2.

## Findings

1. **MED -- `reader.go:94-126`: no dict-block reuse across the keys of one query.** Every `Posting(key)` re-runs `lookup`, which does its own ranged dict-block read; a strict-AND of N trigram keys (see the intended pattern in `conformance/conformance_test.go:105-123`) costs up to 2N ranged reads even when several keys land in the same sparse block. Fix options: sort keys and group by sparse block (mirror Rust `read_dict_blocks` in `rust/src/index.rs:313`), or a one-entry last-block memo on `Index`, or a `Postings(keys []uint64)` batch API.
2. **MED -- `records.go:150-170`: no batched record fetch.** Rendering a top-k page costs 2 reads per doc (16-byte offset pair + blob). Doc IDs are rank-ordered by construction, so top-k hits are frequently near-contiguous: the k+1 offsets for an id range are ONE contiguous idx range read, and adjacent blobs coalesce. Add `GetMany(ids []uint32)` (or `GetRange(lo, hi)`) that reads the offset span in one ReadAt and coalesces adjacent blob ranges (gap threshold like Rust's 16 KB coalescer) -- ~40 RTTs for 20 hits -> 2-4.

## Acceptance

- Counting-ReaderAt test asserts read counts: N-key AND issues ~(unique dict blocks + N posting reads); 20 near-contiguous records fetch in <=4 reads.
- Results identical to per-key/per-id paths (differential test vs existing Get/Posting).
- Do AFTER task 057 (hardening) so the new batch paths inherit the validation.

## Outcome (DONE)

Reader-side only; no output-byte changes, so Rust/Go conformance is untouched.

- **Item 1 (`reader.go`)** -- refactored `lookup` into reusable helpers (`dictBlockFor`, `readDictBlock`, `findInBlock`) and added `Index.Postings(keys []uint64) map[uint64][]byte`, which groups distinct keys by their sparse block and reads each block once (deduping keys that share a block, mirroring the Rust `read_dict_blocks` dedup), then one ranged read per present posting. An n-key AND that shares a dict block now costs `(distinct blocks + present postings)` reads instead of up to `2n`. All the untrusted-size guards (`maxReadBytes`, `blockLen >= 1`, posting-size cap) carry through the shared helpers, so the batch path inherits task 057's hardening.
- **Item 2 (`records.go`)** -- added `RecordStore.GetMany(ids []uint32) map[uint32][]byte` plus a shared `readCoalesced` byte-range coalescer (16 KiB gap, mirroring the Rust `read_coalesced`). It reads the offset-table pairs in a coalesced wave (consecutive ids' pairs overlap, so a run is one read), then the record blobs in a coalesced wave (adjacent blobs merge). A 20-record rank-adjacent page drops from ~40 reads to 2. Out-of-range ids are omitted; each returned record is `decode`d exactly as `Get` does; the offset-pair span guards match `Get`.

### Tests (`batch_test.go`, new)

- `TestPostingsDedupsSharedDictBlock` -- a `countReaderAt` (reset after `Open`) asserts a 3-key query sharing one sparse block issues exactly 4 reads (1 dict block + 3 postings), that each posting byte-matches the single-key `Posting`, and that an absent key is omitted.
- `TestGetManyCoalescesNearContiguousRecords` -- asserts a 20-id contiguous page fetches in <= 4 total reads (idx + bin), byte-matches per-id `Get`, and omits out-of-range ids.

### Verification

- `gofmt -s` clean; `go vet ./...` clean; `go test ./` (root module) and `go test ./...` (conformance module) both green.

### Note

`readCoalesced` currently lives in `records.go` (its only consumer). `reader.go`'s `Postings` reads whole postings (large, scattered) so it deliberately does not coalesce blobs -- only the dict-block reads are deduped, per the finding. If a future batch posting-fetch wants coalescing, `readCoalesced` can be promoted to a shared internal helper.
