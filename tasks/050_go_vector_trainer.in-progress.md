# Task 050 — Go RRVI vector (`.rrvi`) IVFPQ trainer + RRVR rerank

**Status:** in progress (2026-06-23). Started as a standalone library, **`~/go-ivfpq`**
(`github.com/freeeve/go-ivfpq`, MIT) — the fst-go / go-stemmers spin-out pattern, because
the trainer's generic guts (k-means / PQ / IVF) are reusable and married to nothing
roaringrange-specific; only the `.rrvi`/RRVR serialization is, and that stays here.

## Progress

- **`~/go-ivfpq` scaffolded + core landed** (committed locally, not yet pushed): `Rng`
  (xorshift64\*) and `KMeans` (Lloyd's) + `normalize`.
- **Cross-implementation conformance wired.** A `#[cfg(test)]` printer in
  `rust/src/vector_build.rs` (`conformance_ref::print_kmeans_ref`, gated on
  `RR_UPDATE_FIXTURES=1`, no public-API change) emits the `Rng` sequence and a k-means run
  over a fixed well-separated corpus, every `f32` as its raw `u32` bits, into
  `~/go-ivfpq/testdata/kmeans_ref.txt`. The Go tests assert the Rng sequence **and** the
  k-means result match — assignments exact, **centroids bit-for-bit** (they hold bit-exact
  because well-separated assignments are robust and the centroid mean has no fused mul-add).

## Remaining

1. **`Train`** (`= build_ivfpq`): residuals → per-subspace PQ codebooks + per-vector codes →
   scatter into inverted lists. Reuses `KMeans`. Cross-check vs the Rust `build_ivfpq` model
   (likely assignments-exact + codebooks bit-or-ε), then **recall@k vs brute force**.
2. **`FromParts`** (`= build_ivfpq_from_parts`): assemble + validate an externally-trained
   model (e.g. FAISS export).
3. **roaringrange `go/`**: `WriteRRVI(model)` byte-exact serializer + `WriteRerank` (bf16),
   then the RRVI reader/search + end-to-end recall parity vs the Rust-built index.

## Why recall, not byte-exact, for the higher level

A quantized index is approximate by construction, and cross-language `f32` bit-exactness is
fragile — Go may fuse `a*b+c` into an FMA on arm64 where Rust/LLVM does not. So `Train`'s
teeth are recall, not golden bytes. (The deterministic primitives `Rng`/`KMeans` *are*
bit-exact where it is cheap and meaningful — see above — and the `WriteRRVI` serialization is
byte-exact given the trained arrays.)
