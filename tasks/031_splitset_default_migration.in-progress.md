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
- [x] Demo: term mode routes through the term split set when the split toggle
      is set — lazy ~21 KB manifest boot, monolith fallback, the same
      shared-ID-space facet post-filter, term-split file chips.
      **2026-06-11 REVERT: term split is no longer default-on** (toggle stays;
      `?split=1` deep-links it). A present-but-rare query — "roaring bitmaps",
      live — matches nothing in the top tiers and descends all 243 splits
      SEQUENTIALLY (~6 serial round-trips each → 114 reqs / 675 KB / 76 s and
      climbing) where term mono answers in ~6 reads / ~1 s. The bench's
      term-split wins were on common queries; rare queries invert it. The
      term set also has NO Bloom (and Bloom can't prune present-but-rare
      anyway). Re-defaulting needs the budgeted/parallel descent below.
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
      ALSO: visit splits in **parallel waves** (e.g. 8 concurrent) — the 76 s
      "roaring bitmaps" descent above is latency (243 × ~6 serial round-trips),
      not bytes (675 KB). Both are prerequisites for term-split default.
- [~] **Geometric split sizing** (2026-06-11): worst-case descent scales with
      split COUNT (fixed ~4–6 RTTs per split), so retarget from 243×~270 MB
      flat to doubling tiers. Target ≤20 splits per the user's call: base
      512 MiB doubling to an 8 GiB ceiling → ~18 trigram / ~12 term splits.
      Keeps a small cheap top split for common queries while a FULL descent
      is ~12–18 visits ≈ 2–3 s serial, sub-second with waves — vs 76 s today.
      Trade accepted: a visited split's postings scale with its docs, so the
      common-query bytes win shrinks ~5–10× (still well under mono's worst
      case). Quickwit targets ~10M docs/split — flat 2M was over-partitioned.
      **Builder support SHIPPED v0.12.0** (`byte_cap_max` in Rust + Go +
      Python, `cap_for`/`capFor` pinned by the shared geometric golden;
      examples take `cap_max_mb`).
      **TERM GEO SET BUILT 2026-06-11** — but NOT via the greedy builder: at
      multi-GiB caps its open-split map crawled (~1.9K docs/s, 71 h ETA;
      killed). `slice_term_monolith` instead slices the existing mono `.rrt`
      by doc range in ONE sequential pass (selftest: byte-identical to the
      builder given the same ranges) — **12 splits / 53 GB in 32 min**
      (2M docs doubling to 64M; 240 MB → ~10 GB). Verified: "roaring
      bitmaps" = same hits as mono through 12 splits in 3.2 s local (was
      243 splits / 76 s live); "machine learning" 29 ms. Upload to
      `s3://openalex-eve/openalex-term-geo/` (NEW prefix — immutable names
      never overwritten) chained behind the .rrb upload.
      **TRIGRAM GEO SET BUILT + VERIFIED 2026-06-13** via `slice_trigram_monolith`
      (commit b8e84f9): one sequential read of the 113 GB v3 monolith fanned into
      19 streaming split writers — base 2M doubling to a 32M-docs cap (half the
      term slicer's, since trigram postings run ~2x bytes/doc), 498 MB → 11.5 GB
      per split (the [126M,158M] mid-tier heaviest), 109 GB total, **13 min**.
      `--selftest` asserts byte-identical to `SplitSetBuilder`; plain split search
      returns the SAME top hits as the monolith ("machine learning"/"quantum
      computing"/"crispr") and faceted split search filters correctly off the
      derived sidecars. Manifest is 1.5 KB (flags=0, no per-split summaries — the
      `.rrf` sidecars resolve filters with no `FLAG_FACET` gate; the deployed flat
      manifest was summary-stripped anyway). `derive_split_facets` wrote 19
      `.rrf` sidecars (year/type/oa/language; topic excluded). NO `.rrhc`: each geo
      split's sparse index is ~1.8 MB so the big tiers cold-open regardless, and a
      plain manifest open is one cheap GET (demo `SPLIT_RRHC_URL` now guards on
      `SPLIT.rrhc` so an absent bundle is null, not a 404). Local dir
      `/tmp/oa-out/splitset-trigram-geo`.
      **DONE + LIVE 2026-06-13**: 109 GB uploaded to `s3://openalex-eve/openalex-trigram-geo/`
      (40 objects = 19 `.rrs` + 19 `.rrf` + manifest + corpus-wide global Bloom,
      byte-sizes verified against local). Demo cutover committed (9223c96, full
      dataset trigram `split` → `openalex-trigram-geo`; no `.rrhc`; `SPLIT_RRHC_URL`
      guarded) + deployed via `deploy.sh` (reader hash `87eb44ad1a`, CloudFront
      invalidation `I32ZL7FTEL6JNQK88ZXFPZE07L`). Live-verified over CloudFront:
      manifest 200/1489 B, split `RRSI` v3 magic + correct sizes via range read,
      sidecar + bloom 200, deployed `index.html` references the geo prefix. Tagged
      `v0.13.0`. **The default trigram split now descends 19 geo tiers, not 389.**
      REMAINING (separate, non-blocking): lite manifest regen — the `?ds=lite`
      config still points at the flat `openalex-trigram-split/`, which keeps working,
      so the flat set stays on S3 for `?ds=lite` + `?split=0` deep links.
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
