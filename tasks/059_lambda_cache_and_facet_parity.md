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
