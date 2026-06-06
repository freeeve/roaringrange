# Task 012 — fst (Rust) build-perf investigation + possible upstream PRs

**Status:** in-progress (planning + fork done 2026-06-06). Spun off from task 010
(the byte-exact Go port at `~/fst-go`, https://github.com/freeeve/fst-go).

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
2. **Fixed, shared inputs.** Reuse the same corpora the Go harness builds
   (`~/fst-go/testdata/corpora.txt`, `name<TAB>keyhex:val,…`), or regenerate
   deterministically in Rust. Include the big ones (1M shared + ~800K diverse).
3. **Control variance.** Quiet machine, on AC power; run multiples; report
   medians + CIs; acknowledge laptop limits (no frequency pinning available).
4. **Profile** the build (macOS: `cargo instruments` / Instruments time profiler,
   or `samply`). Key question: what fraction is in `malloc`/`free` vs.
   `Registry` (hash/equal) vs. node `compile`?
5. **Allocator baseline (do this FIRST).** Run benches with system malloc AND a
   `#[global_allocator]` of mimalloc/jemalloc. If a faster allocator closes the
   Go gap, there is **no algorithmic story** — write it up and stop.

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
2. Read this file. Shared corpora live at `~/fst-go/testdata/corpora.txt`; golden
   at `~/fst-go/testdata/fst_golden.txt`.
3. Do **E1 (allocator) first** via `~/fst/bench/`, then E0/E2, then E3/E5.
4. Keep every change byte-exact (diff vs the golden). Issue-before-PR.
