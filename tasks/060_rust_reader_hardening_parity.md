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
