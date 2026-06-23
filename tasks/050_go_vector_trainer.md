# Task 050 — Go RRVI vector (`.rrvi`) IVFPQ trainer + RRVR rerank (deferred)

Port `build_ivfpq` / `build_ivfpq_from_parts` + `write_rerank` (`rust/src/vector_build.rs`)
to `go/` — the IVFPQ/OPQ trainer (coarse k-means + product-quantization codebooks +
optional OPQ rotation) and the bf16 re-rank blob. The heaviest build-side gap; it is
native-only and feature-gated even in Rust. **Deferred** unless Go-side vector index
building is specifically needed — the trigram/term/BM25/facet/records/split-set
builders cover the demo's build pipeline without it. Also missing: a standalone
trigram `.rrs` monolith writer (`write_index` / `build_trigram_monolith`), if a Go
monolith path (not just split sets) is ever wanted.
