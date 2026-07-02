# 059: fix(lambda): shared bounded range cache + O(1) LRU + facet-count parity

**Severity: HIGH (OOM risk) + MED (facet parity).** All in `examples/hybrid-search-lambda/src/main.rs`. Line refs @ 849f9c2.

## Findings

1. **HIGH -- four independent 8 GiB caches on a 10 GiB Lambda** (`main.rs:43, 202-211`). `open()` wraps EACH S3 object in its own `CachedFetch::new(..., CACHE_CAP_BYTES)`; term mode creates four (`.rrt`, `.rrb`, `.rrvi`, `.rrf`), so the aggregate cap is up to 32 GiB -- nothing bounds total memory. The comment claims the 8 GiB cap "holds both arms' hot working sets" but the cap is per-fetcher. Under sustained diverse traffic the container exceeds the 10 GiB limit -> OOM kill mid-request -> full cold start (8-11 s) for the next caller. Fix: ONE shared `Arc<Mutex<RangeCache>>` across all fetchers, keyed by `(object_key, offset, len)`, with a single global cap.
2. **MED -- `RangeCache::get` is O(n) per HIT** (`main.rs:96-99`). `self.order.iter().position(|x| x == k)` linearly scans the recency `VecDeque` on every cache hit, while holding the mutex. At 8 GiB of KB-scale entries that is 10^5-10^6 entries -> ms of scan per read, many reads per query. The in-repo `rust/src/range_cache.rs` already solved this with an O(log n) BTreeMap recency index (that type is `!Send` by design for wasm -- copy the recency structure, not the type).
3. **MED -- Lambda facet counts disagree with the client hybrid path** (`main.rs:352, 369-373`). The Lambda computes `h.facets.counts(&fused_bm)`: (a) over the PRE-filter fused set even when `filters` were applied (client `filtered_ids` in `rust/src/wasm.rs:428-443` counts post-filter survivors); (b) head-only resident `counts` (`facet.rs:515`) vs the client's head+tail `counts_full` -- fused ids beyond doc 65535 simply aren't counted. Symptom: toggling the demo's "server" switch changes the facet panel numbers for the same query; filtered categories show inflated counts server-side. Fix: count the post-filter set with `counts_full` semantics to match the client.

## Acceptance

- Total cache memory bounded by one configurable cap regardless of how many objects are open; soak-test locally (or with repeated diverse queries) that RSS plateaus under the Lambda limit.
- Cache hit path no longer scans: micro-bench or reasoning note in the commit.
- Same query + filters returns identical facet counts from `/search-hybrid-*` and the client path (spot-check a query whose hits span doc 65536).
- Container image rebuild/redeploy per term-lambda-apigw notes (buildx provenance=false, ECR roaringrange-hybrid, AWS_PROFILE=openalex-admin); re-check cold start stays under the 29 s API GW cap.

## Outcome (DONE)

All three findings fixed in `examples/hybrid-search-lambda/src/main.rs`; deployed live as image
`roaringrange-hybrid:v0.27.0` on both functions.

- **Finding 1 (shared bounded cache):** dropped the local per-object `RangeCache` and reused the
  crate's `roaringrange::range_cache::RangeCache` (public, un-gated). One
  `Arc<Mutex<RangeCache>>` is now shared by every `CachedFetch` via the `open` closure, keyed by
  `(object_key, offset, len)`, so total cached bytes are bounded by ONE global cap regardless of
  how many objects term mode opens (was up to 4 x 8 GiB = 32 GiB against a 10 GiB Lambda).
- **Finding 2 (O(1) LRU):** the crate `RangeCache` uses a `BTreeMap` recency index, so the
  cache-hit path is O(log n) rather than the old `VecDeque::position` linear scan under the mutex.
  Its eviction is unit-tested in `rust/src/range_cache.rs`.
- **Finding 3 (facet parity):** facet counts now use `counts_full` (head+tail, top-64 per field)
  over the POST-filter result set, matching the client `filtered_ids` path. The old
  `counts(&fused_bm)` was head-only over the pre-filter set, which inflated filtered categories
  and dropped ids beyond doc 65535. Also moved `fused_bm` construction into the filter branch.

Deploy: `cargo lambda build --release --arm64` -> `docker buildx build --platform linux/arm64
--provenance=false --sbom=false -t <ECR>:v0.27.0 --push` -> `update-function-code` on both
`roaringrange-hybrid-tri` and `roaringrange-hybrid-term` (AWS_PROFILE=openalex-admin).

Live verification (https://openalex.evefreeman.com/search-hybrid-{tri,term}):
- Cold start (clean, forced via a config touch): 9.1 s for term -> HTTP 200, well under the 29 s
  API GW cap. (The very first post-deploy invocation 500'd once on the documented one-off
  image-lazy-load; `OnceCell` does not cache the init error, so it self-healed on retry.)
- Warm: ~0.4-0.6 s.
- Facet parity self-consistency, `filters=[["type","article"]]`: tri total 428 / article count
  428; term total 365 / article count 365 -- counts reflect the post-filter set exactly, and
  non-selected type categories are absent. Pre-fix these counts would have exceeded the total.
- Both functions: `State=Active`, `LastUpdateStatus=Successful`, `Max Memory Used ~1 GB`.

Not done (deploy-observability, not code): a sustained-traffic RSS soak proving the plateau --
the single-shared-cap design bounds it by construction and the crate cache's eviction is
unit-tested, so this is left as an optional live observation.
