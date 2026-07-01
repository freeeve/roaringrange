# 072: fix(splitset): distinguish absent vs unreadable facet sidecar in facet_counts

**Severity: MED-LOW.** Spun off from task 060 item 6 (deferred there because it is an
error-taxonomy change, not a malicious-bytes hardening fix). Line refs @ 849f9c2.

## Problem

`rust/src/splitset.rs` `facet_counts` (the `Err(_) => continue` at ~line 776) treats
EVERY `FacetIndex::open` failure as "this split has no facet sidecar, contributes
nothing." A transient transport error (S3 500, network blip) is therefore silently
swallowed and the split's facet counts are dropped -- wrong totals returned as `Ok`,
with no signal to the caller. "Sidecar absent by design" and "sidecar unreadable
right now" are indistinguishable at this layer.

## Why it is not a one-liner

`FetchError` (rust/src/fetch.rs) has only `OutOfRange` and `Transport`. A legitimately
absent per-split sidecar (a 404) surfaces as `Transport`, exactly like a transient
500 -- so the reader cannot tell them apart today. A correct fix needs one of:

1. **A `FetchError::NotFound` variant** -- the honest signal for "this object does not
   exist." Touches every `RangeFetch` impl (`WasmFetch`, `FileFetch`, `MemoryFetch`,
   `CachedFetch`, the split-set resolver, the Lambda `CachedFetch`), each mapping its
   transport's not-found (HTTP 404, `io::ErrorKind::NotFound`, out-of-range on an
   empty object) to it. Then `facet_counts` does `Err(e) if e.is_not_found() =>
   continue, Err(e) => return Err(e)`.
2. **A manifest presence flag** -- record per split whether a facet sidecar was
   written (the builder knows), so `facet_counts` only attempts `open` when present
   and propagates any error. Needs a manifest field (check whether an unused reserved
   bit/field exists before adding one; avoid a format break if possible).

Option 1 is more broadly useful (other readers currently conflate not-found with
transient too) and is the recommended direction.

## Acceptance

- A split whose facet sidecar genuinely does not exist still contributes nothing
  (no error) -- the legitimate absent-sidecar path is preserved.
- A transient/unreadable sidecar error propagates out of `facet_counts` instead of
  silently dropping that split's counts. Cover with a fetcher that returns a
  transport error (not not-found) for one split's `.rrf` and assert `facet_counts`
  errors rather than returning short counts.
- If option 1: `NotFound` mapped in every `RangeFetch` impl; existing tests/goldens
  unchanged. No byte-output changes either way.
