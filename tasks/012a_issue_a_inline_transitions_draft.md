# DRAFT — upstream Issue A for BurntSushi/fst (not yet filed)

> Status: draft for review. Do not file without sign-off. Target repo:
> https://github.com/BurntSushi/fst — file as an issue first (issue-before-PR).

---

**Title:** `Builder`: inline single-transition nodes to remove per-node heap allocation during construction

## Summary

During construction, `BuilderNode.trans` is a `Vec<Transition>`. The builder
allocates and frees one such vector per compiled node, and the overwhelming
majority of nodes have exactly one transition (linear suffix chains), so
construction performs on the order of one heap allocation per node. Storing the
0/1-transition case inline — spilling to the heap only for branching nodes —
removes most of these allocations, with byte-identical output and no new
required dependency.

## Evidence

Measured with a counting global allocator wrapping the system allocator over a
single `MapBuilder::memory()` build (fst 0.4.7). Allocation counts are
deterministic, so they reproduce regardless of machine.

Reproducible on the bundled `data/words-100000` (each line → an increasing u64):

| build | alloc ops / key | alloc bytes / key |
|-------|-----------------|-------------------|
| 0.4.7 | 5.86 | 622 |
| inline-1 | 1.05 | 140 |

On a higher-cardinality corpus (~5M distinct terms from an OpenAlex
title/abstract/author vocabulary), where the bounded registry deduplicates a
smaller fraction of nodes, the effect is larger and per-key allocations stop
growing with size:

| build | alloc ops / key | total alloc ops (5M keys) |
|-------|-----------------|---------------------------|
| 0.4.7 | 10.57 | 52.8M |
| inline-1 | 0.63 | 3.1M |

So ~5–17× fewer allocations depending on the corpus.

**Wall-clock.** Paired baseline-vs-inline-1 build A/B (`MapBuilder::memory`,
alternating order; the paired ratio cancels laptop thermal drift):

| corpus | rounds E3 faster | speedup (mean / median ratio) |
|--------|------------------|-------------------------------|
| 300k   | 10 / 10          | ~10% (0.899 / 0.896)          |
| 5M     | 6 / 6            | **~30%** (0.695 / 0.706)      |

The larger win at scale matches the allocation data: the bounded registry
deduplicates a smaller fraction of a large, diverse vocabulary, so far more nodes
allocate under the old `Vec`. (Absolute build times drift on a laptop — e.g. the
5M baseline ranged 4.2–5.4 s as the machine warmed — but the per-round paired
ratio is stable, which is why the comparison is reliable.)

## Proposed change

Replace `BuilderNode.trans: Vec<Transition>` with an inline-capacity-1 storage
type that:

- keeps 0 or 1 transitions inline; spills to a `Vec` only for branching nodes;
- `Deref`s to `[Transition]`, so every read site is unchanged;
- compares and hashes by transition sequence, so the registry's node dedup is
  unchanged;
- retains a spilled buffer across the registry's `clone_from` (matching today's
  `Vec` reuse).

This is a ~one-line type change with `smallvec` (`SmallVec<[Transition; 1]>`), or
a small hand-rolled enum to keep `fst` dependency-free. Both produce identical
results in my testing. I assumed you'd prefer to keep `fst` dependency-free, so I
have the no-dep version ready — happy to go either way.

## Conformance

- Serialized FST is **byte-identical** to 0.4.7 (verified by SHA-256 of the
  output on both corpora above).
- The full existing test suite passes, including the build roundtrip and
  `quickcheck` tests.

## Caveats / questions for you

- The most robust metric is **allocations** (deterministic, reproducible
  anywhere). The wall-clock figures (~10% at 300k, ~30% at 5M) were taken with a
  paired protocol on a laptop; the paired ratio is stable across thermal drift,
  but a run on a dedicated quiet machine would firm up the exact percentages.
  (Independent cross-check: swapping the global allocator to mimalloc alone gives
  ~13% on the 5M build — consistent with allocation traffic being a real fraction
  of build cost.)
- `smallvec` vs hand-rolled no-dep — your preference?

Repro available: a small `corpus_stat` helper (counting allocator over one build
of a sorted `key<TAB>value` corpus). Happy to share or inline it.
