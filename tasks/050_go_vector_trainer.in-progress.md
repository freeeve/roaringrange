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

## Remaining

1. **roaringrange `go/`: `WriteRRVI(model)`** — the byte-exact `.rrvi` serializer (the
   `Ivfpq::write` layout: 48-B header, opq?, centroids, codebooks, `nlist×12` directory, then
   per-cluster `[ids][codes]`) consuming the `go-ivfpq` `Model`, plus `WriteRerank` (bf16
   `RRVR`). This is deterministic given the trained arrays → golden-testable against Rust.
2. **roaringrange `go/`: RRVI reader/search** (if a Go query path is wanted) → end-to-end
   recall parity vs the Rust-built index.

## Why recall, not byte-exact, for the higher level

A quantized index is approximate by construction, and cross-language `f32` bit-exactness is
fragile — Go may fuse `a*b+c` into an FMA on arm64 where Rust/LLVM does not. So `Train`'s
teeth are recall, not golden bytes. (The deterministic primitives `Rng`/`KMeans` *are*
bit-exact where it is cheap and meaningful — see above — and the `WriteRRVI` serialization is
byte-exact given the trained arrays.)
