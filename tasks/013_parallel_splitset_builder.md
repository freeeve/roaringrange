# Task 013 — Parallel (per-range) term split-set builder

**Status:** pending

Make the term split-set build actually use the machine. The current driver
(`rust/examples/build_term_splitset.rs`, task-A) is **native + bounded-RAM + correct**, but
measured **~1.4 cores / ~14.5k docs/s — the same speed as the single-threaded monolith.**
Parallelizing only the JSON parse moved the bottleneck without removing it.

## Why the task-A driver doesn't parallelize

Measured on a 1M smoke (3 splits, 127.7 MB, 68.7s; 88.7s user / 68.7s real ≈ 1.4 cores;
peak RAM 406 MB). The driver fans the **record read + `serde_json` parse** across rayon, then
feeds text to **one sequential `TermSplitSetBuilder`**. But parse was *not* the dominant cost:
- **Tokenize + Snowball stem + `BTreeMap` insert** run serially inside the builder's `add_text`.
- The **zstd record decode** very likely serializes on a shared decompression context, so even
  the "parallel" `RecordStore::get` calls bottleneck on one lock.

Both of those are the bulk of the work, and both are serial. Bounded RAM (`drain_sealed`
streaming to disk) is the one thing that worked perfectly — keep it.

## Design — K independent per-range workers

Partition `[0, N)` into **K contiguous rank bands** (K ≈ cores). Each band is built by an
**independent worker** with its **own `RecordStore`** (own zstd context → real parallel decode)
and its **own `TermSplitSetBuilder`** (parallel tokenize + stem + insert), streaming its sealed
splits to disk. After all workers join, **merge their split metadata into one `.rrss` manifest**.
Bands map to rank tiers (band 0 = top tier), so the merged manifest stays rank-ordered.

Expected: ~K× throughput (the serial work is now per-core), bounded RAM = `K × byte_cap_in_ram`
(keep `K × byte_cap` well under RAM), no shared-context decode lock.

## Required builder changes (`rust/src/splitset_build.rs` — the task-007 file)

1. **Global-doc-id base.** A worker building band `[lo, hi)` must stamp `docIdLo = lo` and
   hand out global ids `lo..`. The builder currently starts global ids at `0`. Add an
   initial-global-id to `TermSplitBuildConfig` (or a builder setter). Local 0-based posting ids
   inside each split are unchanged; only the manifest's `docIdLo/docIdHi` and the split's head/tail
   doc-id mapping must reflect the global base.
2. **Manifest merge.** Combine the K workers' splits into one manifest. Two options:
   - expose each builder's `Vec<SplitSpec>` (have `drain_sealed`/`finish` return specs alongside
     the blobs), collect them all, fix `tier` + ordering by `docIdLo`, assign a **global split
     filename sequence** (`‹prefix›-s00000.rrt` … across all bands, no per-worker collisions),
     and call the existing `write_splitset(specs, config)` once; **or**
   - a `merge_manifests(&[rrss bytes]) -> rrss bytes` helper that re-reads K manifests and re-emits
     one (renaming the referenced split files to the global sequence).
   The first is cleaner (no re-parse) if exposing `SplitSpec` is acceptable.

## Driver (`build_term_splitset` parallel mode, or a new example)

- Partition `[0, N)` into K bands; one worker thread per band (`std::thread` or a rayon scope).
- Each worker: own `RecordStore`, own `TermSplitSetBuilder` (global base = band lo, unique split
  name range), feed its docs in rank order, `drain_sealed` → write split files for its band.
- Join; collect specs; write the single `.rrss` manifest.
- Keep the task-A serial driver as the **correctness reference** — the parallel build must produce
  a reader-equivalent split set (same docs, same postings, openable + byte-conformant manifest).

## Open questions / considerations

- Split filename scheme: global sequence vs per-worker prefix + rename at merge.
- Tier granularity: one tier per band, or finer tiers within a band (the reader walks `(tier,
  docIdLo)` and short-circuits, so tier count affects top-K pruning granularity).
- Per-split `RRSF` facet sidecars (if facets are added) per worker, named in the same sequence.
- `K × byte_cap` must stay under RAM (the bound knob); pick K = cores, byte_cap so `K × ~1.5×cap`
  fits comfortably.
- Go conformance: the merged manifest must match the Go writer byte-for-byte (extend `go/`).
- Whether to fold this into `build_term_splitset` (a `--workers N` flag) or ship a sibling example.

## Acceptance

~K× speedup over task-A on the same corpus, bounded RAM, identical on-disk format, the output
opens in the `splitset` reader and returns equivalent results to the serial build, and the
manifest passes Go byte-conformance.
