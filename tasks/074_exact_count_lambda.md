# 074 — Guard the split-mode "exact count" with an in-region Lambda

**Status:** pending (spun off from task 031's count contract).

## Why

The split-mode **"exact count"** button (shipped `e44dd85`) computes the true match
count **client-side**: `RrssIndex.countExact` fully intersects the query's trigram
postings across all 19 geo splits. Because trigram postings are large even when few
docs match, that full scan **egresses hundreds of MB from CloudFront to the browser
per click** — a live probe moved ~0.5 GB for a 153-hit query (1.2 GB across two
count_exact + two search calls). At CloudFront's ~$0.085/GB, each click is ~$0.04, and
nothing stops a bot (or a curious user) from clicking it repeatedly on broad queries.

The rest of the demo already routes its heavy paths through in-region Lambdas
(CloudFront → API Gateway → Lambda: `/search`, `/search-term`, `/search-hybrid-{tri,term}`,
`/embed` — see [[term-lambda-apigw]]), where S3→Lambda reads are same-region and free
and only a few bytes cross the wire to the browser. The exact count should do the same.

## Goal

Move the exact-count computation server-side for the demo: a Lambda reads the geo split
set from S3 **in-region**, runs `SplitSet::count_exact`, and returns just
`{ count, exact: true }` (a few bytes). CloudFront egress for the button drops from
~0.5 GB/click to ~0. The pure-static client path stays as a fallback when no endpoint is
configured (so the "runs with zero backend" story still holds).

## Design

New `count-exact-lambda` (mirror `examples/search-lambda/`, which is the closest
template — it already has the pieces):

- **`S3Fetch`** (`RangeFetch` over `aws_sdk_s3` `get_object` with a Range header) wrapped
  in **`CachedFetch`** (the existing per-object LRU byte cache) so the intersection's
  repeated posting reads within a warm container skip S3 round-trips — high value here,
  since count_exact re-reads the same large trigram postings.
- A **`SplitFetcher`** over `S3Fetch` (one `fetch_named` per split data-file under the geo
  prefix), plus the manifest. `search-lambda` only wires a monolith `Index`; this needs
  the split-set reader, so depend on `roaringrange` with the **`splits`** feature and open
  a `SplitSet` in the warm `OnceCell` (like `CATALOG`).
- Handler: parse `q` (+ optional `filters`), call
  `SplitSet::count_exact(&resolver, q, &filter)`, return `{ "count": N, "exact": true }`.
- **Result cache** (recommended): the geo split set is immutable, so a query's exact count
  is stable — memoize `query(+filters) -> count` in a process-wide map so repeated clicks
  on the same/again-popular query cost nothing (the CachedFetch already dedups the reads;
  the result cache also skips the intersection CPU).
- Env: `SPLIT_BUCKET`, `SPLIT_MANIFEST_KEY` (`openalex-trigram-geo/openalex.rrss`),
  `SPLIT_BASE_PREFIX` (`openalex-trigram-geo`). Reuse the `lambda_http` + API Gateway shape.

## Demo wiring

- Add `COUNT_EXACT_LAMBDA_URL` to the dataset config. In `splitExactCount()`
  (`examples/openalex/web/index.html`), when it's set, `fetch()` the endpoint (through
  CloudFront → API Gateway, via `realFetch` so it stays off the ranged byte/request
  counters) instead of `rrss.countExact(q, [])`; render the returned count the same way.
- **Keep the client-side `countExact` as the fallback** when the URL is null — the
  no-backend/static path must still work (and `SplitSet::count_exact` / `RrssIndex.countExact`
  stay shipped regardless).

## Gotchas / open questions

- **API Gateway 29s cap** ([[term-lambda-apigw]]): a ~0.5 GB in-region scan should finish
  well under it (in-region bandwidth is high and the reads parallelize), but measure a
  worst-case broad query ("machine learning"-scale). If a pathological query approaches
  the cap, fall back to the `count_estimate` bound (cheap) or a soft time budget rather
  than time out.
- **S3 GET request charges**: the scan is ~tens of thousands of GETs (~38 k in the probe)
  — in-region *transfer* is free, but GETs are ~$0.0004/1000 ≈ $0.015/cold-click. The
  result cache + CachedFetch make repeats free; still far below CloudFront egress.
- **Filters**: v1 can be unfiltered-only (the button is already gated to the unfiltered,
  capped case). If a filtered exact count is wanted later, pass `filters` through; note the
  split filtered path only supports the sidecar-bounded fields.
- **Cold start**: opening the `SplitSet` manifest is one small GET; the `OnceCell` keeps it
  warm. No large resident boot (unlike the model/embedding lambdas), so cold start is cheap.
- **Deploy**: new Lambda (zip or container) + an API Gateway route (e.g. `/count-exact`) +
  a CloudFront behavior, mirroring the term/hybrid lambda deploy. `AWS_PROFILE=openalex-admin`
  ([[aws-profile-openalex-admin]]).

## Non-goals

- Not a WAND/top-k engine — this is the exact *count* only, computed by full intersection.
- Not changing the client `count_exact`/`countExact` API (stays as the static fallback).
- `count_estimate` (the header bound) stays client-side/API-only; only the expensive exact
  scan is worth guarding.
