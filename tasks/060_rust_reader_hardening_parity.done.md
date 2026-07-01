# 060: fix(rust): reader hardening parity pass (untrusted-bytes gaps missed by 45fa50f)

**Severity: MED.** 45fa50f hardened the main parsers; these paths were missed. All are reachable from remote/corrupt index bytes on read paths. Line refs @ 849f9c2.

## Findings

1. `terms.rs:385` (`TermIndex::open`) -- untrusted `router_len` is `as usize`-cast and FETCHED before any validation. wasm32: `router_len >= 2^32` truncates silently (the FST checksum at :393-396 then rejects, so outcome is an error -- but via the exact cast pattern `fetch.rs:92-94` documents as forbidden). Native: a hostile 40-byte header drives an unbounded pre-validation allocation inside the fetcher (`FileFetch` does `vec![0u8; len]` at `fetch.rs:218`) -> OOM/abort DoS. Validate/cap before fetching; use the same saturating-add style the function already uses two lines later.
2. `sortcols.rs:252, 321` -- unchecked `info.data_off + s` on an untrusted u64 `data_off` (from the column table, :178). Debug: panic. Release: WRAPS -- and a wrapped small offset can land inside the file, so the fetch succeeds and returns wrong values as `Ok` (silent-wrong-bytes). Same pattern in `values()` and `slice_u32`. Use checked/saturating math + `Malformed`.
3. `splitset.rs:1213-1233` (`bloom_contains`) -- `k = read_u32(bloom, 0)` used directly as the hash-iteration count; a corrupt manifest summary with `k = 0xFFFF_FFFF` spins ~4.3B splitmix64 iterations per query key per split -- hangs the wasm tab. `RemoteBloom::open` (:294) already rejects `k > 64` for the same layout. One-line clamp.
4. `vector.rs:412-424` -- cluster-list size math is UNchecked: `len = count * (4 + self.m)` and the mismatch check `block.len() != ids_len + count*self.m`. On wasm32 both sides wrap identically mod 2^32, so a corrupt `count` PASSES the size check and the scan silently produces garbage hits (release; debug panics). `open` (:212-234) documents and uses checked arithmetic for exactly this risk -- apply the same to the query path.
5. `bm25.rs:130` -- `scale <= 0.0` doesn't reject NaN (NaN comparisons are false), so a corrupt header yields all-NaN scores that sort arbitrarily via `partial_cmp(...).unwrap_or(Equal)` (:273-277). Reject `!scale.is_finite() || scale <= 0.0` at open.
6. `splitset.rs:776` (`facet_counts`) -- `Err(_) => continue` swallows ALL `FacetIndex::open` errors as "no facet sidecar"; a transient S3 500 silently drops that split's counts and returns wrong totals as `Ok`. Distinguish absent (e.g. a NotFound-ish error or presence flag in the manifest) from unreadable; propagate the latter.
7. **Short-read convention** -- `Index::open` (`index.rs:153-157`), `SortCols::open` (`sortcols.rs:134-137`), `FacetIndex::open_meta`, `VectorIndex::open`/`RerankStore::open` (`vector.rs:177-181, 235, 577`) index `header[0..4]` with no length check, while `TermIndex`/`Lookup`/`RecordStore`/`Hotcache`/`ImpactIndex` (`bm25.rs:112-114`) check `len < HEADER_SIZE` first. A contract-violating transport panics in half the readers, errors cleanly in the other half. Pick the checked convention everywhere.
8. Small: `vector.rs:297-299` + `RerankStore::len` (:610-612) silently truncate the header's u64 count to u32 -- error at open if `n > u32::MAX` instead. `posting.rs:468, 517` `facet_fetch.expect(...)` would abort (uncatchable on wasm) if a future caller passes facet fields without a fetcher -- return `IndexError::BadQuery`.

## Acceptance

- Extend the 45fa50f mutation-fuzz harness to cover: terms router_len, sortcols data_off, splitset bloom summaries, vector cluster directory, bm25 header scale. No panics/hangs/huge allocs on mutated bytes; typed errors only.
- All existing goldens/conformance pass unchanged (readers only; no byte-output changes).

## Outcome (DONE -- item 6 deferred to task 072)

Reader-side only; no format or output-byte changes. Fixed items 1-5, 7, 8:

- **item 1** `terms.rs`: added `MAX_RESIDENT_ROUTER` (512 MiB); `open` rejects an
  implausible `router_len` before the fetch (no native OOM / wasm32 `as usize`
  truncation).
- **item 2** `sortcols.rs`: `values`/`slice_u32` saturate `data_off + off` so a
  near-`u64::MAX` offset can't wrap into the file and return wrong bytes as `Ok`,
  plus a post-fetch length check before the direct-indexing decode.
- **item 3** `splitset.rs`: `bloom_contains` clamps `k` (`0` / `> 64` -> conservative
  "possibly present") -- no billion-round hash-loop hang.
- **item 4** `vector.rs`: cluster code-list sizes computed with `checked_mul` and
  the block-size check compares against the precomputed length (a wasm32
  `count*(4+m)` wrap can no longer pass and decode garbage).
- **item 5** `bm25.rs`: `open` rejects a non-finite (NaN/inf) scale.
- **item 7** header length guards added to `Index::open`, `SortCols::open`,
  `VectorIndex::open`, `RerankStore::open` (a short fetch errors, not panics).
  `FacetIndex::open_meta` was already covered by `rrsf_boot_len`; `TermIndex`/
  `Lookup`/`RecordStore`/`Hotcache`/`ImpactIndex` already had guards.
- **item 8** `VectorIndex::open`/`RerankStore::open` reject `n > u32::MAX` (so
  `len()` can't silently truncate); the four `posting.rs` `facet_fetch.expect(...)`
  sites now return `IndexError::BadQuery` (no wasm-uncatchable abort).

Fuzz coverage (`fuzz_tests.rs`): extended `fuzz_rrsc` to exercise `values_u32`/
`slice_u32`; added `fuzz_rrvi_search_no_panic` (real trained index, mutated boot/
directory through `open`+`search`), and targeted regressions
`bloom_contains_rejects_out_of_range_k`, `rrsb_open_rejects_nonfinite_scale`,
`rrti_open_rejects_implausible_router_len`. Full per-feature test matrix +
clippy (`-D warnings`) + fmt clean.

**Item 6 deferred (task 072):** `splitset.rs` `facet_counts` swallows every
`FacetIndex::open` error as "no sidecar." Distinguishing an absent sidecar from a
transient transport error needs a `FetchError::NotFound` variant (touches every
`RangeFetch` impl) or a manifest presence flag -- an error-taxonomy change, not a
malicious-bytes fix, so it is out of scope for this pass. Tracked in task 072.
