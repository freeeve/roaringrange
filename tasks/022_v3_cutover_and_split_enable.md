# Task 022 — v3 cutover + re-enable the split-set mode

Coordinated migration to bring the OpenAlex demo fully consistent on **RRSI v3** and turn the
**split-set** toggle back on. Gated on the in-flight v3 monolith build. Do NOT run the heavy
steps while that build (or the Gemma embed) is competing for the box.

## ⏯ RESUME HERE — status 2026-06-09 evening (context cleared mid-cutover)
**v3 monolith is BUILT, VERIFIED, and STAGING to S3. Strategy chosen: OVERWRITE IN PLACE (option a).**
- Built: `/tmp/oa-out/openalex-full.rrs` (105.4 GB, RRSI **v3**, 114,556,791 trigrams) via `build_trigram_monolith`
  (commit `cfa643b`). Verified queryable: `cargo run --release --example query_rrs -- /tmp/oa-out/openalex-full.rrs "machine learning"` → 20 hits; gibberish → 0.
- **Uploading to a STAGING key** `s3://openalex-eve/openalex-full-v3.rrs` (non-destructive; live v2 untouched).
  Detached bg job, log `/tmp/oa-out/upload_monolith_v3.log`. Confirm it finished before cutover:
  `AWS_PROFILE=openalex-admin aws s3api head-object --bucket openalex-eve --key openalex-full-v3.rrs --query ContentLength --output text` should equal `stat -f%z /tmp/oa-out/openalex-full.rrs`.
- Steps 1 (stripped 29 KB manifest, live) + 2 (split re-enabled in `index.html`) are DONE.
- Committed+pushed this session (tag **v0.8.0**): libzstd record-decode fix (`331613a`), `build_trigram_monolith`
  (`cfa643b`), client **range cache** (`ba3ca3b` + demo `8b50350`), `query_rrs` (`67fc810`), demo copy fixes (`47d5f6f`).

**Remaining cutover (run top-to-bottom once the staging upload is confirmed complete):**
```bash
export AWS_PROFILE=openalex-admin
# 1. verify the staged v3 object (header v3 + full size)
aws s3api get-object --bucket openalex-eve --key openalex-full-v3.rrs --range bytes=4-5 /dev/stdout | xxd -p   # → 0300
aws s3api head-object --bucket openalex-eve --key openalex-full-v3.rrs --query ContentLength --output text     # == stat -f%z /tmp/oa-out/openalex-full.rrs
# 2. atomic overwrite of the LIVE key (same-bucket multipart copy; no re-upload). THIS STARTS THE BREAKAGE WINDOW.
aws s3 cp s3://openalex-eve/openalex-full-v3.rrs s3://openalex-eve/openalex-full.rrs --cache-control "public, max-age=31536000, immutable"
# 3. deploy v3 wasm + web assets (NO --no-build): ships v3 reader + range cache + copy fixes + split toggle
( cd /Users/efreeman/roaringrange/examples/openalex && ./deploy.sh )
# 4. invalidate the overwritten data file (deploy.sh only invalidates /index.html /how-it-works.html)
aws cloudfront create-invalidation --distribution-id E3H4W2Y0UYDT7E --paths /openalex-full.rrs
```
Then verify live: trigram (mono + split toggle), term, semantic, hybrid render + facets filter; perf-bar footer shows `cache N hits`. CloudFront dist = `E3H4W2Y0UYDT7E`. Open question left for the user: run the cutover automatically when the upload verifies, or pause for a go. After success, optional cleanup: `s3 rm openalex-full-v3.rrs` and local `/tmp/oa-out/{openalex-full.rrs,openalex-full.rrs.rrwork,records-full.*}` to reclaim disk. Other bg job still running: Gemma embed (`embed_gemma_memmap.py`, separate, ~hours).

## Why
The reader/wasm crate is **RRSI v3-only** (commit `d4cc15b`). Live state today (2026-06-09):
- `openalex-full.rrs` monolith = **v2** → only the *old* (v2) deployed wasm can read it.
- trigram split bodies (`openalex-trigram-split/openalex-s*.rrs`) = **v3** → need a v3 wasm.
- So one wasm can't serve both. The fix is to make **all** trigram artifacts v3 and ship a v3 wasm.

## Already done (ready to ship)
- **Term mode fixed + live** — `index.html` points at `openalex-484m-stem.rrt`. (deployed, `--no-build`).
- **Split manifest fixed** — `splitset_strip_summaries` produced `/tmp/oa-out/openalex-trigram.stripped.rrss`
  (**727 MB → 29 KB**, Bloom/facet summaries dropped). Verified: opens + `"machine learning"` → 10 hits
  against the real split bodies. Split bodies are already v3 and already uploaded (780 objects).
- **`split: null`** in `index.html` (so the demo can't trip on the old 727 MB manifest meanwhile).

## Prereq
- [ ] v3 `openalex-full.rrs` build finished (the `build_trigram_monolith` job; `dash.sh` → "trigram mono DONE").
      Verify v3: `curl -s -r 4-5 …/openalex-full.rrs | xxd -p` → `0300`.
- [ ] Box quiet enough (v3 monolith done; pause Gemma if needed — see [[live-build-state]]).

## Cutover steps (do together — see the coordination risk)
1. **Upload the fixed split manifest** (tiny, harmless to do early):
   `aws s3 cp /tmp/oa-out/openalex-trigram.stripped.rrss s3://openalex-eve/openalex-trigram-split/openalex.rrss --cache-control "public, max-age=31536000, immutable"` (AWS_PROFILE=openalex-admin).
   (Optional: build a real `.rrhc` so split boot is 1 RTT — `write_splitset_bundle` with `max_splits`=tier-0;
   not required, the demo's `openBundle` falls back to a plain open on the 29 KB manifest.)
2. **Re-enable split** in `index.html` DATASETS `full`:
   `split: { rrss: "openalex-trigram-split/openalex.rrss", base: "openalex-trigram-split", rrhc: "openalex-trigram-split/openalex.rrhc", rec: "records-full" }`
   (records-full is correct — split global doc id == monolith rank == records-full id; verified from the builders).
3. **Upload the v3 monolith** `openalex-full.rrs` (~100 GB) to S3 (overwrites the v2). `.rrf`/`.rril`/`.dict` are
   version-independent and **reused** (no rebuild). If a v3 secondary/`openalex-newest` is deployed, rebuild it too.
4. **Deploy a v3 wasm + invalidate the monolith**: `AWS_PROFILE=openalex-admin ./deploy.sh` (NO `--no-build` —
   rebuilds the wasm v3) **and** invalidate `/openalex-full.rrs` (the data cache is normally never invalidated).

### ⚠ Coordination risk
The big data files are served `immutable`/long-TTL. Between "v3 monolith on S3" and "v3 wasm live +
`.rrs` invalidated", a client can pair a v2 wasm with a v3 `.rrs` (or vice-versa) → trigram breaks.
Options: (a) accept a short window, do steps 3+4 back-to-back + invalidate `/index.html /how-it-works.html
/openalex-full.rrs`; or (b) ship the v3 monolith under a **new key** (`openalex-full-v3.rrs`) + point `idx`
at it, copying the sidecars to matching `-v3` names — no overwrite, clean rollback, costs the sidecar copies.

## Verify after cutover
- [ ] Headless / manual: trigram (mono **and** split toggle), term, semantic, hybrid all render + facets filter.
- [ ] Re-run `cargo run --release --features "terms splits vector" --example live_bench` — now the v3 monolith
      reads, so the **trigram split-vs-monolith** row finally populates (the comparison that was blocked).
      Use `SPLIT_BENCH=1` (manifest is 29 KB now, cheap).

## Notes
- The split-set dropped Bloom entirely (demo choice). If rare/absent-term split queries matter later, regen the
  manifest with right-sized Bloom (needs each split's vocab — read the split dicts; bigger job than the strip).
- Tooling added this session (committed): `live_bench.rs`, `splitset_strip_summaries.rs`.
