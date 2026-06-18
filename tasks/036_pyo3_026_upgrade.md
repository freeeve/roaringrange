# Task 036: upgrade pyo3 0.22 → 0.26 (deferred)

**Status: not started, low priority — pick up when a trigger below fires.**

## Current state

`python/Cargo.toml`: `pyo3 = { version = "0.22", features = ["extension-module",
"abi3-py38"] }` → resolved to **0.22.6**. The bindings in `python/src/lib.rs` are
written against the 0.22 API.

## Why it's deferred (not worthwhile yet)

- The python bindings are **build-side tooling** (build roaringrange datasets), not
  a hot or user-facing runtime — low stakes for being on the latest pyo3.
- We use **`abi3-py38`**, so the wheel is forward-compatible by design: it loads on
  newer CPython (3.13, …) without a rebuild. No runtime-compat pressure.
- 0.22 works; the bump is **mechanical churn** (`Bound`/signature migration) with
  **zero functional gain** today.

## Triggers that flip it to worth-doing

- Want **free-threaded / no-GIL Python** (3.13t) support (needs a recent pyo3).
- The build actually **breaks** on a Python/toolchain we target.
- We want a specific **0.26 feature or fix**.
- 0.22 drops out of maintenance and that becomes a concern.

## What it entails (when done)

- Bump the dep `0.22` → `0.26` in `python/Cargo.toml`.
- Work through the compile errors against 0.26's API — primarily the continued
  `Bound<'py, T>` smart-pointer migration and removal of the old GIL-Refs API, plus
  assorted `#[pymethods]`/signature/deprecation updates. Drive it by the actual
  compiler errors + pyo3's per-version migration guides (0.23/0.24/0.25/0.26), not
  from memory.
- Keep the existing helpers (`io_err`/`build_err`/`index_err`, the `Language::
  from_code`/`parse_metric`/`parse_policy` parsers) behaving the same.

## Acceptance

- `cd python && cargo check` clean; an `abi3` wheel builds (`maturin build`).
- A smoke test: build a small `.rrt`/`.rrvi`/split set via the Python API and read
  it back, matching pre-upgrade output.
- Its own focused commit; lands in the next tag.
