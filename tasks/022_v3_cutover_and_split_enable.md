# Task 022 — v3 cutover + re-enable the split-set mode

Coordinated migration to bring the OpenAlex demo fully consistent on **RRSI v3** and turn the
**split-set** toggle back on. Gated on the in-flight v3 monolith build. Do NOT run the heavy
steps while that build (or the Gemma embed) is competing for the box.

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
