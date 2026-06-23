# Task 050 — Go RRVI vector (`.rrvi`) IVFPQ trainer + RRVR rerank (deferred)

Port `build_ivfpq` / `build_ivfpq_from_parts` + `write_rerank` (`rust/src/vector_build.rs`)
to `go/` — the IVFPQ trainer (coarse k-means + product-quantization codebooks) and the
bf16 re-rank blob. The heaviest build-side gap; native-only and feature-gated even in
Rust. **Deferred** — nothing in the pipeline needs it (the demo's RRVI is Rust/Python
built; the Gemma full-corpus RRVI is task 021, EC2/native), and there is **no Go RRVI
reader** either, so a trainer would be orphaned on both ends.

(The standalone trigram `.rrs` **monolith writer** that used to be bundled here was split
out to **task 051** and shipped — it is unrelated: a deterministic byte-exact serializer,
not an approximate trainer.)

## Why this is not just "port the other writers"

The trainer is simple and deterministic in Rust (plain Lloyd's k-means, no k-means++, a
trivial xorshift `Rng` with a fixed seed, OPQ left `None` / trained externally), so it is
*portable* — but byte-exact Go↔Rust conformance is the wrong bar and fragile:

- **Float bit-exactness is fragile across languages.** The k-means assignment hinges on the
  L2 distance accumulation `acc + diff*diff`. **Go may fuse that into a single FMA on
  arm64** (the Go spec permits fusion) **while Rust/LLVM does not** (it fuses only on an
  explicit `mul_add`). One different rounding → a different `argmin` on a near-tie → a
  different cluster assignment → centroids diverge → the whole index drifts. Reproducing
  byte-identical centroids would mean hand-suppressing FMA and matching reduction order —
  the zstd "byte-stability is a trap" lesson in a different costume.
- **A vector index is approximate by construction.** Two IVFPQ indexes that differ only by
  float rounding give essentially identical recall, so the conformance that *matters* is
  **recall parity** (Go-trained index within ε of Rust's recall@10 on a fixed corpus), not
  golden bytes.

## Plan if/when a Go vector-build need appears (reader-first, recall-parity)

1. **Port the RRVI reader + search path to Go first** — it is the more useful half (Go can
   then *query* vector indexes) and is the prerequisite for any recall harness.
2. **Build a recall harness in Go** — brute-force kNN baseline + the RRVI search path,
   measuring recall@10 on a fixed embedding corpus.
3. **Then port the trainer**, asserting **recall parity** with the Rust-built index (within
   ε), *not* byte equality. Do not chase cross-language f32 bit-exactness.
