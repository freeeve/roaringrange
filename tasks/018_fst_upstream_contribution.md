# 018 — Optional upstream contribution of the fst inline-transitions win

**Status:** pending (optional; spun off from task 012)

Task 012 is done on the roaringrange side: the inline single-transition builder-node
change (E3) is built, validated byte-identical, ~17× fewer build allocations / ~30%
faster at 5M-term scale, pushed to the `freeeve/fst` fork (`inline-transitions`), and
adopted across all consumers (builder, wasm/demo client, python). What remains is
purely discretionary open-source contribution — none of it affects roaringrange, all of
it is sign-off-gated.

## Pending items

1. **File Issue A — inline single-transition `BuilderNode` (E3).** Draft is ready at
   `tasks/012a_issue_a_inline_transitions_draft.md`. Leaves the smallvec-vs-no-dep choice
   to the maintainer; carries the allocation + byte-identical-conformance + wall-clock
   data. Not filed.
2. **tantivy-fst PR.** E3 is already ported to `~/tantivy-fst` (branch
   `perf/inline-transitions`, commit `12b70e1`), local/unpushed. Push the branch and
   `gh pr create --repo quickwit-inc/fst --head freeeve:perf/inline-transitions` when ready.
3. **Issue B — reusable builder / `reset()` API (E5).** A feature proposal (not a perf
   bug): `Builder` is one-shot and reallocates the 10000×2 register each build; a
   `reset()` would amortize it across batch builds. Not implemented; propose as an issue
   regardless of E3.

## Caveats (from 012's receptivity check)

- **BurntSushi/fst is dormant** — last functional work 2023; open PRs (even one-line doc
  fixes) sit unmerged 2–4 years. A perf PR is unlikely to be merged in any timeframe.
- **tantivy-fst** (`quickwit-inc/fst`) historically merges PRs and is build-perf-motivated,
  but is also quiet since late 2023 — the more receptive venue if upstreaming.
- Etiquette: open an issue **before** any PR; bring criterion data, not laptop microbench.
- Conformance guard: any change must keep `MapBuilder` output byte-identical (diff vs
  `~/fst-go/testdata/fst_golden.txt`).

## Acceptance

Issue A filed (BurntSushi and/or tantivy-fst) with the supporting data, or an explicit
decision not to upstream. E5 and the tantivy-fst PR are independent, lower-priority,
take-or-leave follow-ups.
