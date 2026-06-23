# Task 050 — Go RRVI vector (`.rrvi`) IVFPQ trainer + RRVR rerank

**Status:** in progress (2026-06-23). Started as a standalone library, **`~/go-ivfpq`**
(`github.com/freeeve/go-ivfpq`, MIT) — the fst-go / go-stemmers spin-out pattern, because
the trainer's generic guts (k-means / PQ / IVF) are reusable and married to nothing
roaringrange-specific; only the `.rrvi`/RRVR serialization is, and that stays here.

## Progress

- **`~/go-ivfpq` scaffolded + trainer landed** (committed locally): `Rng` (xorshift64\*),
  `KMeans` (Lloyd's), `normalize`, **`Train` (`= build_ivfpq`)**, **`FromParts`
  (`= build_ivfpq_from_parts`)**, and `Reconstruct` (centroid + PQ-decoded residual).
- **Cross-implementation conformance wired.** Two `#[cfg(test)]` printers in
  `rust/src/vector_build.rs` (`conformance_ref::print_kmeans_ref` / `print_ivfpq_ref`, gated
  on `RR_UPDATE_FIXTURES=1`, no public-API change) emit the `Rng` sequence, a k-means run, and
  a full `build_ivfpq` model over fixed corpora — every `f32` as its raw `u32` bits — into
  `~/go-ivfpq/testdata/{kmeans_ref,ivfpq_ref}.txt`. The Go tests assert the
  cross-language-robust parts match: the Rng sequence and k-means **centroids bit-for-bit**
  (assignments exact), and the trainer's **coarse centroids bit-for-bit + list membership
  exactly**. PQ codebooks/codes are FMA-sensitive across languages, so PQ quality is checked
  by `TestTrainRecall` instead — **recall@10 = 0.85** vs a brute-force baseline.

- **`WriteRRVI` + `WriteRerank` landed** (`go/vector.go`): the byte-exact `.rrvi` / `.rrvr`
  serializers consuming the `go-ivfpq` `Model` (roaringrange `go/` now depends on
  `github.com/freeeve/go-ivfpq v0.1.0`). Golden-conformed against Rust `Ivfpq::write` /
  `write_rerank` via `gen_rrvi_golden` + `go/vector_test.go` + Rust
  `build_tests::rrvi_golden_matches` (deterministic `build_ivfpq_from_parts` fixture +
  bf16 `f32ToBF16` round-to-nearest-even). The train → serialize → reader-format loop is
  closed.

## Remaining

1. **Perf pass on `~/go-ivfpq`** (requested): allocation reductions first, then a profiling
   pass — the `go-stemmers` / fst perf-plan pattern.
2. **(Optional) RRVI reader/search in Go** — only if a Go *query* path is wanted; needed for
   end-to-end recall parity vs the Rust-built index, not for building/serializing.

## Why recall, not byte-exact, for the higher level

A quantized index is approximate by construction, and cross-language `f32` bit-exactness is
fragile — Go may fuse `a*b+c` into an FMA on arm64 where Rust/LLVM does not. So `Train`'s
teeth are recall, not golden bytes. (The deterministic primitives `Rng`/`KMeans` *are*
bit-exact where it is cheap and meaningful — see above — and the `WriteRRVI` serialization is
byte-exact given the trained arrays.)
