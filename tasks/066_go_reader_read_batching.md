# 066: perf(go): batch/reuse ranged reads in the Go reader (dict blocks, records)

**Severity: MED (read count == S3 round trips).** Reader-side only; no output-byte changes. Line refs @ 849f9c2.

## Findings

1. **MED -- `reader.go:94-126`: no dict-block reuse across the keys of one query.** Every `Posting(key)` re-runs `lookup`, which does its own ranged dict-block read; a strict-AND of N trigram keys (see the intended pattern in `conformance/conformance_test.go:105-123`) costs up to 2N ranged reads even when several keys land in the same sparse block. Fix options: sort keys and group by sparse block (mirror Rust `read_dict_blocks` in `rust/src/index.rs:313`), or a one-entry last-block memo on `Index`, or a `Postings(keys []uint64)` batch API.
2. **MED -- `records.go:150-170`: no batched record fetch.** Rendering a top-k page costs 2 reads per doc (16-byte offset pair + blob). Doc IDs are rank-ordered by construction, so top-k hits are frequently near-contiguous: the k+1 offsets for an id range are ONE contiguous idx range read, and adjacent blobs coalesce. Add `GetMany(ids []uint32)` (or `GetRange(lo, hi)`) that reads the offset span in one ReadAt and coalesces adjacent blob ranges (gap threshold like Rust's 16 KB coalescer) -- ~40 RTTs for 20 hits -> 2-4.

## Acceptance

- Counting-ReaderAt test asserts read counts: N-key AND issues ~(unique dict blocks + N posting reads); 20 near-contiguous records fetch in <=4 reads.
- Results identical to per-key/per-id paths (differential test vs existing Get/Posting).
- Do AFTER task 057 (hardening) so the new batch paths inherit the validation.
