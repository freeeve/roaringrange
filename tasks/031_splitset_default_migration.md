# 031 — Split sets as the default; retire the monoliths

Make the RRSS split set the demo's default trigram backend (done in this task's
first commit), close the remaining parity gaps, then retire the monolithic index
files from S3 once nothing references them.

## Why (live 484M bench, 2026-06-10, `live_bench` over CloudFront, K=25)

| query | trigram mono | trigram split | term mono | term split |
|---|---|---|---|---|
| machine learning | 870 KB · 7.8 s | 244 KB · 6.4 s | 15.5 KB · 0.9 s | 41 KB · 0.9 s |
| crispr gene editing | 862 KB · 13.2 s | 237 KB · 10.2 s | **14.15 MB** · 3.4 s | **263 KB** · 3.3 s |
| quantum computing | 947 KB · 12.2 s | 261 KB · 8.2 s | 17.5 KB · 0.6 s | 43.5 KB · 1.4 s |
| deep residual learning | 1.07 MB · 10.0 s | 302 KB · 8.5 s | 20.5 KB · 1.2 s | 46.5 KB · 1.8 s |

Identical hit sets in every row. Splits win or tie everywhere and **cap the
worst case** (term mono's 14.15 MB pathological intersection → 263 KB). The
Lambda server path is faster still (~0.3–0.9 s warm, ~1–2.7 KB to the client)
but costs per-invoke and needs the mono `.rrs` on S3 — it is the main blocker
for deleting the trigram monolith.

## Steps

- [x] Demo: split mode default-ON for trigram (`?split=0` = opt-out, URL encodes
      the exception; `forceSplit` datasets pinned on; server toggle re-engages
      the default on exit). Shipped in this commit; needs deploy.
- [ ] Demo: migrate **term mode** to the term split set (it has no split toggle
      today — either add one or hard-switch; bench says no parity gates needed).
- [ ] Per-split facet sidecars: build with all 5 fields (today only `year`), so
      split +facet stops returning empty for type/oa/lang/venue filters.
- [ ] Manifest facet-presence dimension (TLV tag 2 already exists — populate it
      in the full build) so an unsatisfiable filter is answered from the resident
      manifest in **0 requests** (post-v0.9.1 it is 2 small reads × 389 splits).
- [ ] Budgeted/progressive tiered descent: a sparse query currently opens splits
      tier-by-tier with no first-paint bias or scan-cost cap — mirror the
      monolith cursor's `TAIL_WINDOWS_PER_CALL` contract so the UI can stream.
- [ ] Split cursor/count contract: `RrssIndex` exposes ranked lists only — no
      paging cursor, no `countEstimate`, no `facetCounts`. The demo's count line
      and facet checkboxes silently degrade in split mode.
- [ ] Lambda: either repoint `/search` at the split set (new code path) or
      accept that server mode dies with the monolith.
- [ ] S3 retirement, **only after the above** (≈ $0.023/GB·mo):
      - `openalex-full-v3.rrs` staging copy — 113.2 GB (~$2.60/mo). Pure
        duplicate of the live monolith; deletable immediately on confirmation.
      - `openalex-full.rrs` — 113.2 GB (~$2.60/mo). Blocked by: Lambda /search,
        demo `?split=0` opt-out path, old deep links.
      - `openalex-484m-stem.rrt` — 53.8 GB (~$1.24/mo). Blocked by: demo term
        mode (term split not wired into the demo yet).
      - Keep: `openalex-full.rrf` (facet counts + post-filter share the global
        doc-ID space), records-full store, `.rrvi`, `.rrhc`, both split trees.

Net savings if all three go: ~$6.40/mo. The lol is acknowledged; the deep-link
breakage is the real cost — old `?split=1`/mono URLs must keep resolving to
*something* sensible.
