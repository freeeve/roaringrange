# 020 — RRTI term index: live deployment + later phases

**Status:** pending (spun off from task 005)

Task 005 is done on the library side: the RRTI reader, blocked dictionary (task 009),
builder, wasm `RrtIndex`, Python binding, CI gate, stemming/stopwords, and prefix completion
are all complete, and the demo term-mode UI is wired. What remains is making term search
actually live, plus the optional later-phase index features.

## Blocking the live demo (term mode is currently broken-live — `.rrt 403`)

The demo has a "Term" search radio and `RrtIndex.open(RRT_URL)`, but the artifact is never
built or uploaded, so term mode fails on first use.

1. **Build a v2 `openalex-…-stem.rrt`.** Today only the standalone Python script
   (`python/scripts/build_term_index.py`) emits one, and older artifacts are RRTI v1 (the v2
   reader rejects v1 with `BadVersion`). Produce a current v2 artifact for the live dataset.
2. **Add a monolithic `-rrt` build path to the OpenAlex builder** (`builder/src/main.rs`
   currently emits `.rrt` only inside split-sets via `-term-splits`) — optional if the Python
   script stays the source of truth, but cleaner for the demo build.
3. **Upload it in `deploy.sh`** — add the `.rrt` to the `--data` upload list (it is absent
   today) and deploy so term mode works live.

## Optional later phases (from 005's phasing — genuine future scope)

- **Step 4 — inline rare postings + residency:** inline very-short postings into the FST
  output / a resident head-presence bitmap for hot terms (no fetch for common-term head).
- **Step 6 — hot-phrase materialization.**
- **Step 8 — positional postings** (phrase queries). Explicitly "optional, later".
- **`Catalog` wiring:** add `TermIndex` to the `Catalog` facade (composition convenience;
  not in the original phasing steps).

## Acceptance

Term search is live in the demo (a deployed v2 `.rrt`, term mode returns results). The
later-phase items are independent, take-or-leave follow-ups.
