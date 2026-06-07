# Task 012 — fst (Rust) build-perf investigation + possible upstream PRs

**Status:** **done** (2026-06-07). The investigation reached a verdict (allocation gap,
not allocator/variance), the fix (E3 — inline single-transition builder nodes, no-dep) is
implemented, validated byte-identical, ~17× fewer build allocations / ~30% faster at 5M
scale, pushed to the `freeeve/fst` fork, and **adopted across every roaringrange consumer**
(rust crate, OpenAlex builder, wasm/demo client, python — all lockfiles pin the fork commit
`0c2ac459`). The remaining work is optional, sign-off-gated **upstream contribution** (file
Issue A, the tantivy-fst PR, the E5 reuse API) — spun off to **task 018**; none of it affects
roaringrange. Spun off from task 010 (the byte-exact Go port at `~/fst-go`,
https://github.com/freeeve/fst-go).

## Why this exists

While benchmarking the Go `fst` port against the Rust `fst` crate, the Go builder
sometimes measured **faster** than Rust (e.g. on medium/large builds, 0.5×–0.95×
Rust time on go1.26.4). That is surprising for a BurntSushi crate, so this task
investigates **whether it reflects a real, improvable inefficiency in the Rust
crate** — and, if so, prepares defensible upstream contributions.

**Default hypothesis (skeptical): it does NOT.** The most likely causes are (a)
the macOS system allocator vs. Go's runtime allocator, and (b) measurement
variance — Rust's `diverse_1M` build measured anywhere from **0.6 s to 1.4 s for
the identical binary** across runs; the run-to-run noise dwarfs the Go-vs-Rust
gap. The investigation must rule these out **before** any upstream contact.

## Setup (done)

- **Fork:** https://github.com/freeeve/fst (fork of BurntSushi/fst), cloned to
  **`~/fst`**. `origin` = freeeve/fst, `upstream` = BurntSushi/fst. Working branch
  **`bench/rust-vs-go`** (off `master`).
- **Version parity:** `master` is at **0.4.7** — exactly what `~/fst-go` targets,
  so no tag checkout is needed (bench `master`; the Go golden is from 0.4.7 too).
- **Existing criterion suite:** `~/fst/bench/` (`fst-bench`, criterion 0.3.1,
  `src/bench.rs`, `harness = false`). **Build on this**, don't reinvent it.
  Release/bench profiles are already optimized (`debug = true` only adds symbols).
- **Go side:** `~/fst-go` — `compare_test.go` (wall-clock Go-vs-Rust harness) and
  `rustgen/src/bin/bench.rs` (Instant-loop Rust bench reading shared `corpora.txt`).
  These are *indicative*, not rigorous; criterion replaces the Instant loop.

## Methodology — rigor first

1. **criterion for every Rust measurement** (warmup, outlier rejection, CIs).
   Extend `~/fst/bench/src/bench.rs`; never quote Instant-loop numbers for a PR.
2. **Realistic inputs, repo-clean (see "Corpus" below).** Primary corpus is the
   actual demo workload: the OpenAlex term vocabulary, streamed from the public
   snapshot and never vendored. The synthetic Go corpora (`shared_1M`/`diverse_1M`)
   stay as a cheap reproducible cross-check, and the small in-repo word lists are
   what any upstream bench uses (so the upstream diff carries no OpenAlex data).
3. **Control variance.** Quiet machine, on AC power; run multiples; report
   medians + CIs; acknowledge laptop limits (no frequency pinning available).
4. **Profile** the build (macOS: `cargo instruments` / Instruments time profiler,
   or `samply`). Key question: what fraction is in `malloc`/`free` vs.
   `Registry` (hash/equal) vs. node `compile`?
5. **Allocator baseline (do this FIRST).** Run benches with system malloc AND a
   `#[global_allocator]` of mimalloc/jemalloc. If a faster allocator closes the
   Go gap, there is **no algorithmic story** — write it up and stop.

## Corpus (realistic, OpenAlex-derived) — repo-clean, streamed from source

The demo's real FST workload (term mode) is a sorted **term dictionary / router
`MapBuilder`** built from tokenized academic text (title + reconstructed abstract
+ authors + venue), mapping each term to a u64 block location. So the benchmark
corpus is the **distinct term vocabulary** of OpenAlex Works — far more
representative than the tiny synthetic conformance set (`corpora.txt`'s largest
case is 123 KB).

- **Generator:** `~/fst-go/cmd/oacorpus` (stdlib-only Go). Streams the public
  OpenAlex Works snapshot over HTTPS (`s3://openalex/data/works/…/*.gz`, gzip
  JSON Lines, no AWS creds — rewrites the manifest's `s3://` URLs to
  `https://openalex.s3.amazonaws.com/…`), mirrors the demo's text pipeline,
  tokenizes to lowercased Unicode letter/digit terms, accumulates the distinct
  vocabulary to `-budget`, then writes sorted `term<TAB>value` lines (value packs
  a 40-bit offset + 24-bit length, the demo's router value shape). Raw works are
  streamed and discarded — nothing is vendored.
- **Repo-clean:** derived corpora land in `~/fst-go/testdata/*.corpus`, which is
  `.gitignore`d. The upstream `fst` bench stays OpenAlex-free.
- **Scale:** target **5–10M distinct terms** for the confirmation run (decided
  with the user). Smoke corpus (120K terms from ~12K works) already validates the
  pipeline and builds a clean `fst::Map`.
- **Criterion hook (upstream-clean):** `~/fst/bench/src/bench.rs` gained a
  generic `build_corpus` bench gated on `FST_BENCH_CORPUS` (any sorted
  `key<TAB>value` file). It mentions no OpenAlex and is dormant when the env var
  is unset, so the committed diff is a small reusable affordance, not a data dep.
  Run: `FST_BENCH_CORPUS=~/fst-go/testdata/openalex_terms.corpus cargo bench -- build_corpus`.

## Experiments — priority order

- **E0 — Reproduce on criterion.** Bench `MapBuilder::memory` build over the
  shared corpora (small → 1M). Establish a stable baseline + measured variance.
  Re-run the Go harness alongside; confirm the gap survives rigorous timing.
- **E1 — Allocator swap (cheapest, highest-probability explanation).** Add
  mimalloc (and/or jemalloc) as the global allocator in the bench crate; re-run
  E0. Hypothesis: closes most/all of the gap → "not an fst inefficiency."
- **E2 — Profile.** If a gap persists post-E1, profile to localize it. Expect
  `Registry` (FNV hash + structural `eq`) + allocator. Only a non-allocator
  hotspot justifies an algorithmic PR.
- **E3 — `SmallVec` for `BuilderNode.trans`** (the strongest reusable idea; see
  table below). Replace `Vec<Transition>` with `SmallVec<[Transition; 1]>` so the
  overwhelmingly common single-transition node — both during build and in every
  register cell — needs **zero** heap allocation. Measure on criterion; this is
  the Rust analog of the Go arena + free-list wins.
- **E4 — (if E3 helps) register-cell pre-reserve / node-trans pooling.** Only if
  SmallVec doesn't already capture it.
- **E5 — Builder reuse API (`reset`).** Feature, not a fix: the crate's `Builder`
  is one-shot (`into_inner(self)` consumes; `Builder::memory()` reallocates the
  10000×2 register each build). A `reset()`/reusable builder amortizes that across
  many builds (batch workloads). Measure; propose as an **issue** regardless.

## Results log

**2026-06-06 — corpus + E0 + E1 (macOS laptop, system vs mimalloc).**
- Corpus: OpenAlex 5M-distinct-term vocabulary, 1.69M works streamed (1.9 GB) in
  6m23s; vocab did **not** saturate early (long multilingual/name/numeric tail).
  183 MB corpus, builds a clean `fst::Map`.
- E0 (system): 5M build ≈ 7–11.5 s/build, ~8–13 MiB/s. 120k ≈ 68 ms — so 5M is
  mildly **superlinear** (registry/cache pressure), which is the E2 target.
- **E1 (mimalloc): no measurable effect.** Median change vs system bounced
  −1.2% (5M, p=0.90) and +4.6% (300k/50-samples, p=0.49) — indistinguishable
  from zero, sign flipped. **Allocator hypothesis not supported.**
- **Methodology caveat:** identical 5M code measured 7.15 s then 11.5 s across
  runs. The laptop can't resolve <~15% effects. Decision-grade E3/E5 comparison
  needs a quieter environment **or** a noise-robust protocol — fix before E3.
- **Gap status: UNCONFIRMED on the real corpus.** No Go-vs-Rust head-to-head on
  the OpenAlex corpus yet (original observation was synthetic). Next step.

**2026-06-06 — repeated averaging attempt (user asked for a better average).**
8-round paired A/B on a 100k-term corpus, 80 samples/run. Result: **the laptop is
too noisy/thermally unstable to average away allocator-scale effects.**
- System-allocator noise floor: **CV ≈ 28%**, range 148–324 ms — and the same
  build was ~68 ms an hour earlier (2–5× thermal drift under sustained load).
- Order-effect confound: always ran sys→mim, so the 2nd run is warmer; rounds 1–6
  showed mim ~25% "faster", rounds 7–8 reversed (mim ~50% "slower") as throttling
  hit. The averaged ratio 0.909 is an **artifact**, not a verdict.
- Conclusion: effects < ~30% are unresolvable here. Need a controlled box
  (pinned freq / isolated cores) **or** a cooldown + alternating-order protocol,
  **or** pivot to E2 profiling (time *fractions*, noise-robust). The bench now
  honors `FST_BENCH_SAMPLES` / `FST_BENCH_MEAS_SECS` for repeated runs.

**2026-06-06 (later) — Gemma stopped; deterministic alloc counting = the answer.**
The user stopped the contending local Gemma job. Two noise-free tools added:
`~/fst-go/rustgen/src/bin/allocs.rs` (Rust, counting global allocator) and
`~/fst-go/cmd/oaallocs` (Go, runtime.MemStats). Output is **byte-identical**
between Go and Rust (5.9 MB @ 300k, 139.5 MB @ 5M) — parity holds.

- **THE finding — allocation gap (deterministic, machine-noise-free):**
  | corpus | Rust allocs/key | Go allocs/key | Rust ÷ Go |
  |--------|-----------------|---------------|-----------|
  | 300k   | 6.39 (1.92M)    | 0.115 (34.5k) | ~56×      |
  | 5M     | 10.57 (52.8M)   | 0.0141 (70.5k)| ~750×     |
  Go's total allocations barely grow with key count (free-list/arena recycle the
  node `trans` backings); Rust allocs scale with node count and per-key *rises*
  (registry dedups less of a bigger vocab). **This is the real Go-vs-Rust gap —
  an allocation-efficiency gap, not a mystery.**
- **E1 corrected:** on the cooled machine with alternating-order paired runs,
  **mimalloc is ~13% faster (8/8 rounds, paired ratio 0.87)**. Absolute CV still
  ~20% (laptop warms under load), but the *paired ratio* is stable — so report
  paired ratios, not absolute times. The earlier "no effect" was Gemma noise.
- **Mechanism confirmed:** `build.rs:90` `BuilderNode.trans: Vec<Transition>` is
  the per-node alloc/free churn; `registry.rs` `clone_from` is bounded. E3
  (`SmallVec<[Transition;1]>`) inlines the dominant single-transition node →
  targets exactly this. E3 ≫ E5 for Rust (Go's reuse-vs-fresh allocs are equal,
  so Reset/E5 saves little once a free-list exists).
- **Verdict so far:** the gap is real and is E3's target. Allocation reduction is
  provable noise-free even on this laptop; confirm the *time* win on a quiet box.

**2026-06-06 (later still) — E3 implemented + validated (`SmallVec<[Transition;1]>`).**
Changed `BuilderNode.trans: Vec<Transition>` → `SmallVec<[Transition; 1]>` in the
fork (`build.rs`, +`smallvec` dep; test-only `smallvec!` in `registry.rs`/`node.rs`).
Measured with a fork-based counting allocator (`~/fst/bench/src/bin/corpus_stat.rs`):
| corpus | baseline allocs/key | E3 allocs/key | reduction |
|--------|---------------------|---------------|-----------|
| 300k   | 6.385 (1.92M)       | 0.621 (186k)  | 10.3×     |
| 5M     | 10.568 (52.8M)      | 0.621 (3.10M) | 17×       |
- **Byte-identical output** (SHA-256 baseline == E3, both corpora; `cmp` ✓) and
  **all 136 crate tests pass**. Allocs/key is now **constant 0.62** (was rising) —
  per-node Vec churn structurally gone (Go free-list behavior, achieved by inlining).
- **Open:** (a) rigorous *time* delta on a quiet box (laptop noise; mimalloc's ~13%
  is a floor since E3 removes the alloc work entirely, not just speeds it up);
  (b) for the PR, prepare a **no-dep** inline-1 variant — `fst` core is currently
  dependency-free and BurntSushi favors minimal deps, so `smallvec` may be resisted.
- **Ready to draft upstream Issue A** with this allocation + conformance data.

**2026-06-06 (later still²) — no-dep variant built (the upstream-ready form).**
Replaced `smallvec` with a hand-rolled `enum Trans { None, One(Transition),
Many(Vec<Transition>) }` in `build.rs` (Deref/DerefMut to `[Transition]`, slice-based
PartialEq/Eq/Hash, Extend/FromIterator/From<Vec>, IntoIterator for &/&mut so all
read sites are unchanged). `clear()` keeps a spilled `Many` buffer so the registry
reuses it across `clone_from` (matches old Vec). **Zero new dependencies** (fst
core back to dep-free). Results:
| corpus | baseline | no-dep | reduction |
|--------|----------|--------|-----------|
| 300k   | 6.385    | 0.692  | 9.2×      |
| 5M     | 10.568   | 0.627  | 16.9×     |
Matches the `smallvec` version (0.62), **byte-identical** (SHA-256 == baseline),
all 136 tests pass, rustfmt-clean. This is what the PR should carry; mention
"or just use smallvec" as the simpler alternative in the issue.

**2026-06-06 — committed + Issue A drafted.**
- Reproducible-from-bundled-data point (maintainer can run): `data/words-100000`
  → key/ordinal corpus: baseline **5.86** allocs/key → inline-1 **1.05** (5.6×),
  byte-identical. (Smaller win than OpenAlex 5M's 17× because the bounded registry
  dedups more of an English word list.)
- Commits (nothing pushed):
  - fork `~/fst` @ `bench/rust-vs-go`: `perf(raw): store builder node transitions
    inline…` (E3, the PR candidate) + `bench: add external-corpus build benchmark
    and allocation diagnostics`.
  - harness `~/fst-go` @ new branch `bench/openalex-corpus`: `test: add OpenAlex
    corpus streamer and build allocation counters`.
- **Issue A draft:** `tasks/012a_issue_a_inline_transitions_draft.md` — leaves the
  smallvec-vs-no-dep choice to the maintainer. NOT filed (needs sign-off).
- Remaining/optional: rigorous wall-clock delta on a quiet box; file Issue A;
  then E5 (reuse API) as a separate, lower-priority feature issue.

**2026-06-06 — wall-clock measured locally (Gemma stopped → machine quiet).**
Paired baseline-vs-E3 A/B (two prebuilt bench binaries, alternating order, 300k
OpenAlex corpus, 50 samples/run): **E3 ~10% faster build** — 10/10 rounds faster,
mean 0.899 / trimmed-mean 0.896 / median 0.896 paired ratio. First run showed
baseline CV **1.1%** (vs 20–28% under Gemma), confirming paired-ratio + quiet
machine = trustworthy. Consistent with mimalloc's ~13% and the alloc reduction.
Issue A draft updated with the wall-clock number. So: yes, local wall-clock works
with the paired protocol; no cloud box needed for a defensible figure (a dedicated
box would only firm up the exact %).

**2026-06-06 — 5M wall-clock (the realistic demo scale).** Added
`~/fst/bench/src/bin/build_time.rs` (clean single-build timer, no allocator
instrumentation) for paired A/B on builds too slow for criterion's ≥10 samples.
Paired baseline-vs-E3 on the **5M** corpus: **E3 ~30% faster** (6/6 rounds, mean
ratio 0.695, median 0.706) — vs ~10% at 300k. The time win scales with the
allocation win, exactly as predicted (less registry dedup of a large vocab → more
per-node allocs eliminated). Headline for Issue A: ~30% faster + ~17× fewer
allocations at demo scale, byte-identical, no deps.

**2026-06-06 — upstream receptivity check (informs whether to file/PR).**
- Fork base == upstream/master (`5907b47`); change applies cleanly, no rebase.
- **BurntSushi/fst is dormant.** Last functional work 2023; latest commit (Sep
  2024) is a FUNDING file. Open PRs — incl. one-line doc/badge fixes (#174, #141,
  #147…) — sit unmerged for 2–4 years; no recent merges. A perf PR, however clean,
  is unlikely to be merged in any timeframe. No prior art on builder-alloc/smallvec.
- **tantivy-fst** (`quickwit-inc/fst`, the crate Tantivy/Quickwit use) historically
  *does* merge PRs (last in Nov 2023, e.g. PSeitz) and is build-perf-motivated, but
  also quiet since late 2023. More receptive audience in principle.
- **Conclusion:** the change is technically a reasonable PR, but calibrate effort:
  issue-first + low investment; the real value is the validated fork. Cross-posting
  to tantivy-fst is worth considering. Don't polish a big PR pre-signal.

**2026-06-06 — base choice confirmed; tantivy-fst is a fork.**
- `quickwit-inc/fst` (crate `tantivy-fst` 0.5.0) is a GitHub **fork of BurntSushi/fst**;
  its builder is essentially unchanged from 0.4.7 (still `trans: Vec<Transition>`),
  so E3 ports there nearly verbatim and they haven't done it.
- We correctly based on **BurntSushi/fst 0.4.7**: `roaringrange/rust/Cargo.toml`
  depends on `fst = "0.4"` (BurntSushi, not tantivy-fst), and the Go port + golden
  are pinned to 0.4.7. tantivy-fst would have mismatched both for a crate we don't use.
- tantivy-fst = better *contribution venue* (not a better base). Porting E3 onto
  0.5.0 is ~the same diff; offer to both if upstreaming.

**2026-06-06 — E3 ported to tantivy-fst 0.5.0 + validated.**
- Cloned the real `quickwit-inc/fst` (tantivy-fst 0.5.0) to `~/tantivy-fst`
  (origin=`freeeve/tantivy-fst`, upstream=`quickwit-inc/fst`), branch
  `perf/inline-transitions`, committed `12b70e1` (the no-dep `Trans` change; rustfmt'd).
- Same no-dep change applied cleanly (edition 2021, byteorder, but builder identical).
  Validated via `examples/corpus_stat.rs` (untracked, local): allocs/key
  6.385→0.692 (300k), 10.568→0.627 (5M, 16.9×); **byte-identical** (SHA tantivy
  base==E3); **137 tests pass**. Wall-clock ~30% expected (identical builder/allocs
  to the BurntSushi measurement).
- **GitHub fork wrinkle:** can't cleanly fork `quickwit-inc/fst` under `freeeve`
  (network already forked as `freeeve/fst`). `freeeve/tantivy-fst` exists but its
  GitHub *parent* is BurntSushi/fst; it's in the same network, so a PR from a
  branch there to `quickwit-inc/fst` still works. Push branch + `gh pr create
  --repo quickwit-inc/fst --head freeeve:perf/inline-transitions` when ready.
- Not pushed; no PR filed (needs sign-off).

**2026-06-06 — fork pushed + adopted in roaringrange builders.**
- Fork-name fix: an earlier `gh repo fork … --fork-name tantivy-fst` had **renamed
  `freeeve/fst` → `freeeve/tantivy-fst`** (GitHub one-fork-per-network). Renamed it
  **back to `freeeve/fst`**; `freeeve/tantivy-fst` now redirects there. Content was
  always intact (BurntSushi 0.4.7 fork).
- Pushed to `freeeve/fst`: branch **`inline-transitions`** (= master + the E3 perf
  commit `0c2ac45`, clean — for consumption) and `bench/rust-vs-go` (full work).
- **roaringrange now consumes the fork:** `rust/Cargo.toml` `fst` dep → `git =
  "https://github.com/freeeve/fst", branch = "inline-transitions"` (behind `terms`;
  flows to the OpenAlex builder + all readers). `cargo check --features terms`
  compiles `fst v0.4.7 (git …#0c2ac459)` clean. Committed on roaringrange branch
  `perf/fst-fork-inline-transitions` (`0195af2`), off `main`. Byte-identical output,
  so existing indexes/readers unaffected.
- `~/tantivy-fst` origin re-pointed to the canonical `freeeve/fst` URL. tantivy-fst
  PR work (branch `perf/inline-transitions`, commit `12b70e1`) remains local/unpushed.

**2026-06-07 — fork adoption audited across all consumers; python brought on board.**
- The fork dep is **already on `main`** (commit `0195af2` is an ancestor; `main` is 5
  commits ahead). The side branch `perf/fst-fork-inline-transitions` is fully merged
  (`git log main..branch` empty) → stale, deletable. No merge needed.
- Consumer audit (lockfile `source` pins):
  - `rust/Cargo.lock` and `examples/openalex/builder/Cargo.lock` → fork `#0c2ac459` ✓
  - demo/client wasm build (`deploy.sh`: `--features "wasm zstd vector terms splits hotcache"`)
    links fst via the rust crate → fork ✓
  - `python/Cargo.lock` was **stale (crates.io)** — repointed via `cargo update -p fst`
    → fork `#0c2ac459`.
- Fixing python's lock surfaced a **pre-existing, fst-unrelated** break: the binding
  still called the v2 `write_index` (head/tail split + `head_boundary` arg). Migrated
  `python/src/lib.rs` to the v3 signature (`serialize_posting` → `Vec<(u64, Vec<u8>)>`,
  no `head_boundary`); facet `.rrf` path unchanged (still `split_posting`). `cargo check`
  green, rustfmt-clean.
- Note on where the win lands: inline-transitions is a **build-time** `MapBuilder`
  alloc/speed win (~17× fewer allocs, ~30% faster at 5M), so it benefits the **builder**
  and the **python `write_term_index`** writer. The reader/client reads fst bytes
  byte-identically, so linking the fork there is for consistency, not a runtime delta.

## Go optimizations → Rust reusability

The allocation wins made in the Go port (`~/fst-go`), and whether each maps to a
Rust improvement. (The Go register is `10000 × 2` cells; node dedup is a bounded
LRU, not perfect minimization — same as Rust.)

| # | Go optimization | Rust crate today | Reusable upstream? |
|---|---|---|---|
| 1 | Inline pending transition (value+flag, was a heap `*lastTransition` per suffix byte) | Already `Option<LastTransition>` inline in the stack `Vec` | **No** — Rust already does this |
| 2 | Transition-slice **free list** (recycle popped nodes' `trans` backings) | `BuilderNode.trans: Vec<Transition>` alloc'd on first push, dropped/freed when the node is popped & compiled → malloc/free **churn per node** | **Maybe** — pooling could cut churn, but **E3 subsumes it** |
| 3 | Pre-warmed **register arena** (cap-1 `trans` for all 20k cells from one slice) | Each `RegistryCell` node's `trans` Vec allocates on first `clone_from`; 20000 cells → same cold-alloc pattern Go had | **Yes — via `SmallVec<[Transition;1]>`** (E3): inlines the single-transition case for build **and** cache → kills #2 and #3 together. Idiomatic; strongest candidate. |
| 4 | Allocation-free `Get` (reused reader scratch) | Crate reader reads transitions lazily from bytes; no per-node alloc | **No** — Rust reader already zero-alloc |
| 5 | `Builder.Reset()` (reuse register/buffers across builds) | No reuse API; one-shot builder reallocates the register each build | **Yes — as a feature** (E5). Propose as an issue. |
| 6 | Pre-sized 10 KB output buffer | `Builder::memory()` already `Vec::with_capacity(10*1024)` | **No** — copied **from** Rust |

Net: two real candidates — **E3 (`SmallVec`)** for per-node/per-cell alloc, and
**E5 (reuse API)** as a feature. Everything else is already handled in Rust or was
a Go-port-only fix. And **E1 (allocator) gates all of it** — if it explains the
gap, E3/E5 become "nice absolute wins," not "Go beats Rust" stories.

## Upstream issues — fine-grained, one per optimization

File a **separate, focused issue per optimization** that proves out (not one
umbrella issue). Each issue stands alone: its own motivation, its own criterion
data (median + CI, system allocator **and** mimalloc/jemalloc so it's clearly
allocator-independent), and its own byte-identical-conformance note. Open the
issue **before** any PR, and only after E1 shows the win isn't just the
allocator. Land them independently so the maintainer can take, defer, or decline
each on its own merits.

- **Issue A — Inline single-transition `BuilderNode` (E3).** `BuilderNode.trans`
  is a `Vec<Transition>`; the overwhelmingly common node has exactly one
  transition, so every such node (build-time **and** every registry cache cell)
  pays a heap alloc/free. Proposal: `SmallVec<[Transition; 1]>` (or a no-dep
  inline-1 enum fallback — upstream may resist a new dep). Subsumes Go-port wins
  #2 (trans free-list) and #3 (register arena). Evidence gate: robust criterion
  win + zero output-byte change.
- **Issue B — Reusable builder / `reset()` (E5).** `Builder` is one-shot
  (`into_inner(self)` consumes; `Builder::memory()` reallocates the 10000×2
  register every build). Proposal: a `reset()` / reusable-builder API that
  amortizes the register + output buffer across many builds (batch workloads).
  Framed as a **feature** issue — propose regardless of E1 (it's an API gap, not
  a perf bug). Evidence: the per-build allocation it removes + a batch-build
  criterion delta.

Anything else from the Go→Rust table is already handled in Rust or was a
Go-port-only fix — do **not** file issues for those. If E2 surfaces a new
non-allocator hotspot, it earns its own issue under the same bar.

## Decision gates

- **E1 closes the gap** → conclusion: macOS allocator, not `fst`. Write up; do not
  open a perf PR. Optionally still propose E5 (reuse) as a feature.
- **E3 shows a robust, allocator-independent improvement on criterion** (median +
  CI, multiple machines ideally) → prepare a PR, but **open an issue with the data
  first**.
- **E5** → propose as a feature issue regardless of E1/E3.

## Upstream etiquette

`fst` is mature and expert-maintained (Tantivy/Meilisearch depend on it). The bar
for a perf PR is high: bring **criterion** numbers + a profile, not laptop
microbenchmarks. **Open an issue before any PR.** A `SmallVec` change touches the
hot path and node equality — verify it changes **no output bytes** (run the
Go-vs-Rust conformance: build the shared corpora and diff against
`~/fst-go/testdata/fst_golden.txt`).

## Pitfalls / notes

- Conformance guard: any Rust change must keep `MapBuilder` output **byte-identical**
  (the whole point of `~/fst-go`). Diff against `fst_golden.txt` after every change.
- Don't move published tags; work on `bench/rust-vs-go`.
- `SmallVec` adds a dependency to `fst` — upstream may resist; have a no-dep
  fallback (e.g. a hand-rolled inline-1 enum) ready if proposing it.

## Resume checklist (after context clear)

1. `cd ~/fst` (branch `bench/rust-vs-go`, fork of BurntSushi/fst @ 0.4.7).
2. Read this file + memory `fst-rust-perf-plan`.
3. **Corpus harness is built** (see "Corpus"): `~/fst-go/cmd/oacorpus` streams the
   OpenAlex term vocabulary; `~/fst/bench/` has the `FST_BENCH_CORPUS`-gated
   `build_corpus` criterion bench. Smoke corpus validated; a 5M-term pull was
   started. Confirm `~/fst-go/testdata/openalex_terms.corpus` exists (regenerate
   via `go run ./cmd/oacorpus -budget 5000000 -out testdata/openalex_terms.corpus`)
   — it's `.gitignore`d, so it won't survive a clean checkout.
4. Next experiments, in order: **E1 (allocator)** in `~/fst/bench/` against the
   OpenAlex corpus + the synthetic `shared_1M`/`diverse_1M` cross-check; then
   E0 baseline write-up / E2 profile; then E3 (`SmallVec`) / E5 (reuse).
5. Keep every change byte-exact (diff vs `~/fst-go/testdata/fst_golden.txt`).
   Upstream output = **fine-grained per-optimization issues** (see that section),
   issue-before-PR, with allocator-independent criterion data.
