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

## Outcome (DONE)

Implemented **option 1** (the recommended `FetchError::NotFound` variant). No serialized
bytes changed -- this is purely reader-side runtime error taxonomy, so all conformance
goldens are unaffected and the Go side (which has no `facet_counts`/`FetchError` layer) is
untouched.

Changes:
- `rust/src/fetch.rs` -- added `FetchError::NotFound` (with a doc-comment distinguishing it
  from `Transport`), `FetchError::is_not_found()`, and a `Display` arm. `MemoryFetch` gained
  a `missing` field + `MemoryFetch::missing()` constructor whose `read` always returns
  `NotFound` -- so an in-memory resolver can represent a legitimately-absent object distinctly
  from an empty (present-but-truncated) one. `FileFetch` maps `io::ErrorKind::NotFound` ->
  `NotFound`.
- `rust/src/wasm.rs` -- BOTH `WasmFetch` paths (`get_all` whole-object and the ranged
  `RangeFetch::read`) map HTTP `404`/`416` -> `NotFound`. The ranged path is the one
  `FacetIndex::open` actually uses, so this is what makes the split-set demo's wasm
  `facet_counts` skip an absent sidecar instead of erroring.
- `rust/src/splitset.rs` `facet_counts` -- the aggregation loop now does
  `Err(IndexError::Fetch(e)) if e.is_not_found() => continue, Err(e) => return Err(e)`.
  An absent sidecar (404) is skipped; a transient/corrupt one propagates.
- Test resolvers (`splitset.rs` `MapResolver`/`CountingResolver`/`GlobalBloomResolver`,
  `splitset_bundle.rs`, `fuzz_tests.rs`) now return `MemoryFetch::missing()` for an absent
  name instead of an empty `MemoryFetch` -- required so an absent facetless split still
  yields `NotFound` (skip) rather than `OutOfRange` (which would now wrongly propagate).
- New test `facet_counts_skips_absent_but_propagates_unreadable_sidecar` covers all three
  cases: absent sidecar -> skipped (Ok), present-but-corrupt bytes -> propagated (Err), and a
  simulated transient `Transport` failure (S3 500 via a `FlakyResolver`) -> propagated (Err),
  satisfying the acceptance criterion exactly.

Deliberately NOT changed: the Lambda `S3Fetch` impls (`examples/*-lambda/src/main.rs`). Those
are the trigram-monolith / term-index search Lambdas; each opens a single *required* `.rrf`
from an env-configured key and should fail hard if it is absent -- they never run split-set
`facet_counts`, so a `404 -> NotFound` mapping there has zero functional effect. Adding
SDK-version-specific `SdkError` introspection to deploy-gated example code carries build-break
risk with no benefit for this fix; it is left for the deploy-gated follow-up (058/059) if ever
wanted.

Verification: `cargo fmt`; `cargo test` (default 95, splits 130, all green); `cargo clippy
--all-targets` clean across default/vector/terms/hotcache/splits/"splits hotcache"/"splits
terms"; `cargo check --target wasm32-unknown-unknown --features wasm` compiles (only
pre-existing dead-code warnings); `go build ./...` clean (no Go changes).
