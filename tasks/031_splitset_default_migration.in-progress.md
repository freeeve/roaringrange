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
- [x] Demo: term mode routes through the term split set when the (default-on)
      split toggle is set — lazy ~21 KB manifest boot, monolith fallback, the
      same shared-ID-space facet post-filter, term-split file chips. Needs
      deploy.
- [x] Per-split facet sidecars with the bounded fields (year/type/oa/language):
      `derive_split_facets` slices the monolith .rrf along split doc-ID ranges
      (no corpus re-stream) — 389 sidecars, 1.2 GB, verified end-to-end locally
      (`check_split_facet`: type=article → 4,896 filtered hits in split 0 where
      the year-only sidecar gave 0). `topic` (54,631 cats) stays excluded: it
      would bloat the per-split meta the filtered path reads per visited split;
      the demo post-filters topic via the monolith .rrf (SPLIT_FACET_FIELDS).
      **Built + verified locally; needs S3 upload to `openalex-trigram-split/`
      + CloudFront invalidation.**
- [~] Manifest facet-presence (tag 2): mostly MOOT once sidecars carry the
      bounded fields — every split has all four, so presence prunes nothing;
      the unsatisfiable case collapsed to "field not in sidecars", which the
      demo now routes to the post-filter. Revisit only if a truly absent
      (field, cat) filter shows up hot.
- [ ] Budgeted/progressive tiered descent: a sparse query currently opens splits
      tier-by-tier with no first-paint bias or scan-cost cap — mirror the
      monolith cursor's `TAIL_WINDOWS_PER_CALL` contract so the UI can stream.
- [ ] Split cursor/count contract: `RrssIndex` exposes ranked lists only — no
      paging cursor, no `countEstimate`, no `facetCounts`. The demo's count line
      and facet checkboxes silently degrade in split mode.
- [x] DECISION (2026-06-11): **server (Lambda) is the trigram DEFAULT** on the
      full dataset (~1–3 KB / sub-second warm vs dozens of client reads);
      split set stays the client-side default behind the toggle (`?srv=0`).
      Consequence: `openalex-full.rrs` STAYS on S3 — the Lambda reads it.
- [ ] Lambda gaps (current handler is monolith-trigram-only — `INDEX_KEY`
      + `INDEX_FACETS_KEY` env, `Index::open` + `FacetIndex`):
      - no split-set path (the toggles are mutually exclusive because both
        claim the trigram backend; in-region the split fan-out would be cheap,
        nobody has written `SplitSet` support into the handler — and with the
        monolith staying, the value is benchmarking, not necessity);
      - no term mode (`?mode=term` param + `TermIndex::open` would add it;
        client-side term is already 15–50 KB / sub-second, so the win is
        exact totals + facet parity, not bytes).
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
