# 058: fix(demo): surface errors instead of hanging/silently degrading

**Severity: HIGH/MED (user-visible on the live demo).** All in `examples/openalex/web/index.html`. Line refs @ 849f9c2.

## Findings

1. **HIGH -- server-mode paging has no error handling** (`index.html:1817`). `goPageServer` calls `const map = await fetchRecords(missing);` bare, unlike `goPage` (:1703) and `goPageRanked` (:1855) which try/catch. `pageTo` (:1881) and `runSearch` (:2136-2139) use `try/finally` with NO catch, and event handlers call `runSearch(true)` un-awaited. A transient S3/network error during a server-mode page (the DEFAULT mode) -> unhandled promise rejection, skeleton cards stay on screen forever, no message (the ticker stops via `finally`, so it looks "done").
2. **MED -- `facetFilter` silently renders UNFILTERED results** (`index.html:2119-2122`). `catch (_) { applyFacetCounts(null); return { ids, ... } }` -- if `filterIds` fails while facet checkboxes are active, the full unfiltered list renders under a summary still claiming "... + facets". Wrong results, zero indication. Hits every ranked mode (term/semantic/hybrid/split). Show an error state (or at least a "filters unavailable" badge) instead of pretending the filter applied.
3. **MED -- zstd dict fetched without an `ok` check** (`index.html:2562-2563`). `new Uint8Array(await (await fetch(DICT_URL)).arrayBuffer())` on the fallback boot path: a 404/403 hands the error page's bytes to `openWithDict` as the dictionary; open succeeds, every compressed record then fails to inflate -> blank cards, real cause hidden. Check `resp.ok` and fail boot loudly.
4. **LOW -- hybrid client arms swallow all errors** (`index.html:2296, 2305, 2346, 2354`). Every arm is `try { ... } catch (_) {}`; if both arms fail (offline), hybrid renders "0 results" as a verdict. Term/semantic modes surface errors; hybrid should too (at minimum when BOTH arms error).

## Acceptance

- Kill the network mid-paging in each mode (devtools offline): every mode shows an error state, none hangs on skeletons.
- Facet filter failure with active checkboxes shows a visible degradation notice, not silently-unfiltered results.
- Dict URL 404 at boot -> clear boot error, not blank cards later.
- Redeploy: wasm/web deploy per `deploy.sh` (AWS_PROFILE=openalex-admin), verify live.
