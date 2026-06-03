# 004 — Similarity vector search (IVFPQ, range-fetchable on S3)

Add semantic / similarity search to roaringrange, in the same static, range-fetched-
from-S3 ethos as the trigram index. Two query-embedding **modes** over the **same**
search machinery:

1. **Lambda mode** — query text → Lambda → embedding provider API → query vector →
   wasm does the IVFPQ search via S3 range reads. Lambda is a thin key-hiding proxy.
2. **Pure-static mode** — **model2vec in wasm** (tokenize → mean-pool static token
   embeddings, no neural net) produces the query vector → same wasm search. No backend.

The search (range reads + ADC + re-rank) is identical and lives in wasm; only the query
embedder differs. `vector_id == doc_id`, so results reuse the existing record store and
can hybridize with the trigram search.

## Locked decisions
- **Mode 1 embedder:** an **open, on-device-tier** model so the corpus embeds **locally
  for $0** and the query runs the **same model+recipe**. **Start with
  `EmbeddingGemma-300M`** (smallest → fastest cold start, smallest Lambda package, fastest
  484M local embed; MRL 768→256→128, 100+ langs, 2048 ctx — ample for titles+abstracts).
  Its two frictions are manageable: license = one-time Gemma-terms acceptance (we never
  commit weights to the repo; the Lambda/build only *uses* them); no-fp16 →
  **int8-quantize the ONNX** (Gemma's intended path, "~200 MB"), so fp32 isn't forced on
  Lambda CPU. **Upgrade to `Qwen3-Embedding-0.6B`** (Apache-2.0, fp16 ONNX, 32k ctx,
  MRL→256, instruction-aware) *only if* Gemma's quality is insufficient.
  - **Decide on a SUBSET before the full embed:** bench Gemma (and Qwen, cheaply) on a few
    hundred OpenAlex title/query pairs (NDCG@25) and commit to the keeper *first*. A full
    embed is ~days and switching models means re-embedding all 484M + re-training the
    index — don't pay that twice. (arctic-v2/bge-m3/nomic also work, but larger / less
    Lambda-friendly.)
  - **Query path:** preferred — **Lambda runs the model itself** as a **container image with
    `onnxruntime` + the (int8) ONNX export** (NOT PyTorch — too big), byte-identical to the
    corpus recipe, no external key, pay-per-invoke (cold start via provisioned concurrency /
    keep-warm). Alt — same model on a host (Ollama Cloud / Together / DeepInfra / Replicate).
    OpenAI text-embedding-3-small (~$1K/484M) is a paid fallback. **Bedrock does NOT fit**
    (Titan/Cohere closed → no free local corpus pass).
- **CRITICAL: corpus and query must use the identical model + pooling + query/doc prefix**
  (Qwen3's instruction prefix / EmbeddingGemma's `task:` prefixes — asymmetric query vs
  document), else the spaces don't match. The Lambda-runs-the-model option guarantees this;
  a hosted API needs the prefix/pooling matched exactly.
- **Mode 2 embedder:** `minishlab/potion-retrieval-32M` model2vec (**512-d**).
- **D is a per-index header field** (one format/reader serves any D). Recommend mode 2
  (model2vec) = **512** (native); mode 1 = MRL-truncate to **256** (shrinks the IVF-PQ +
  speeds distance math; rerank at full dim if recall needs it) or 512 — bench. Each mode is
  a different vector space → **one RRVI per mode**.
- **Training:** FAISS (Python) trains IVFPQ + encodes all vectors → a Rust tool exports
  to the `RRVI` range-fetchable layout. The **reader is pure-Rust/wasm** (no FAISS).
  FAISS is build-time-only, so the crate's minimal-dep/runtime ethos is preserved.
- **"+" = optional re-rank**: after ADC top-R, range-fetch higher-precision vectors for
  those R and rescore. Off for v1 (see Risks: full-vector blob is ~0.5–1 TB at 484M).

## Cost / scale (build-time knob, not baked in)
- With the open-model approach, **both modes embed the full 484M for $0** (API cost):
  - Mode 2 (model2vec): local, **~hours** (static, no transformer).
  - Mode 1 (open transformer): local, **~2–6 days** on the Mac GPU (Ollama/MLX/ONNX),
    or **~$50–150** on a rented GPU for a few hours. Query-time: pennies (Lambda compute
    or host).
- OpenAI fallback (if chosen): ~$0.8–1.5K for 484M + Batch-API orchestration.
- The pipeline takes any doc subset (`-limit` / id list), so validate on a small set first.

## RRVI format (new artifact; `RRVI` magic; all integers LE; vectors L2-normalized
## so cosine == inner product == L2 on the sphere)
Mirrors the .rrs philosophy: small **boot** region (downloaded once), large
**range-fetched** region (only nprobe lists per query).

**Header (boot):** magic `RRVI`, version, D (512), metric, nlist, m (PQ subquantizers),
nbits (8 → 256 codes/subspace), N (vector count), flags (residual=1, opq, rerank).

**Boot blobs (download once, a few MB):**
- OPQ rotation matrix `D×D` f32 (optional; improves PQ accuracy).
- Coarse centroids `nlist × D` f32. (Sizing: nlist≈16–64K for 484M → 16–64 MB boot —
  the key boot-vs-query tradeoff; consider PQ-compressing centroids or a 2-level IMI if
  64 MB boot is too heavy. For the OpenAI subset, nlist is small → trivial boot.)
- PQ codebooks `m × 256 × (D/m)` f32 (tiny, e.g. m=32 → 32·256·16·4 ≈ 512 KB).
- Cluster directory: `nlist × (u64 offset, u32 count)`.

**Range-fetched region (per query, only nprobe clusters):** per cluster, contiguous
`[u32 vector_ids × count][m-byte PQ codes × count]`. nprobe clusters → nprobe ranged
GETs. Sizing at 484M, nlist=16K, m=32: avg list ≈ 30K vecs × (4+32) ≈ ~1 MB/list;
nprobe=16 → ~16 MB/query (tune nlist↑ / nprobe to cut this). Consider splitting ids and
codes into two blobs so a query can fetch codes first, ids only for survivors.

**Optional re-rank blob:** higher-precision vectors (f16 or SQ-int8) keyed by doc_id, for
range-fetching the top-R candidates only.

## Default IVFPQ params (tune empirically)
nlist ≈ `4·√N` (subset) up to 16–65K (484M); m = D/16 = 32 (16-d subspaces), nbits = 8;
nprobe = 8–32 (recall/latency knob); metric = inner product on normalized vectors;
residual encoding + OPQ on. Validate recall@k vs a FAISS flat baseline on a held-out set.

## Components to build
1. **Embedder (build-time, per mode)**
   - Mode 1: batch OpenAI `text-embedding-3-small` (Batch API for scale), title+abstract
     (cap ~512 tok), MRL-truncate → 512, L2-normalize. Reuse the OpenAlex record source.
   - Mode 2: model2vec `potion-retrieval-32M` locally over the same text (fast, free).
   - Output: `(doc_id, f32[512])` aligned to existing doc-id (rank) order.
2. **IVFPQ trainer + RRVI writer**
   - FAISS `index_factory("OPQ32,IVF<nlist>,PQ32")`, train on a 1–10M sample, add all,
     then a **Rust exporter** reads the FAISS index and writes the RRVI layout above.
   - Lives in `examples/openalex/builder` (or a new `vectors` builder) + a `python/`
     training script.
3. **wasm reader** (pure Rust, in `rust/src/`, behind a feature; reuses `RangeFetch`)
   - boot (centroids+codebooks+OPQ+directory); `nprobe` nearest centroids in-memory;
     range-fetch lists; build ADC distance tables; scan codes; top-k heap; optional
     re-rank; return `Vec<(doc_id, score)>` → records via existing `RecordStore`.
4. **Lambda** (mode 1) — `examples/.../search-lambda` sibling. Preferred: **runs the open
   model itself (ONNX/Candle)** → 512-d normalized query vector (no external key, pay-per-
   invoke, byte-identical to the corpus recipe). Alt: proxy the same model on a host
   (Ollama Cloud / Together / Cloudflare; secret key, never logged; match prefix/pooling).
   Either way it returns just the vector — wasm does the search. (Alt: lambda does the whole
   search — rejected, breaks the static-search reuse.)
5. **model2vec wasm embedder** (mode 2) — tokenizer + token-embedding matrix + mean-pool:
   - Ship the potion token-embedding matrix (`vocab × 512`; quantize to int8 ≈ vocab·512 B
     to shrink the one-time download) + the tokenizer (HF wordpiece — need a wasm-friendly
     tokenizer: port the vocab/normalizer, or use `tokenizers` wasm). Embed = tokenize →
     gather rows → mean-pool → (model2vec's) normalize. **Biggest unknown — see Risks.**

## Integration
- Sibling artifacts on S3: `<name>.rrvi` (+ optional `.rrvi.rerank`), one per mode. Boot
  blobs cached; lists range-fetched. doc_id → existing record store for result payloads.
- Reuse `crate::fetch::RangeFetch` (the same abstraction the .rrs reader uses).
- Future: **hybrid** — combine trigram (RRS) and vector (RRVI) candidate sets / scores.

## Implementation order
1. RRVI format module + writer + a tiny in-memory round-trip test (small synthetic set).
2. Pure-Rust reader (boot + nprobe + ADC + top-k), tested against a brute-force baseline
   on a small set (recall check).
3. FAISS training script (python/) + Rust exporter (FAISS → RRVI); validate recall@k.
4. model2vec embedder (build-time, mode 2) → vectors → index → end-to-end query test.
5. wasm bindings for the reader; mode-2 in-browser path (model2vec wasm embedder).
6. OpenAI batch embedder (mode 1) on the subset → index; Lambda embedding proxy.
7. (Optional) re-rank blob + stage; hybrid with trigram search.

## Risks / open questions
- **model2vec tokenization in wasm** — the hardest piece. potion uses a HF tokenizer;
  need a wasm-friendly tokenizer (port vocab + normalization, or `tokenizers` crate on
  wasm). De-risk early (step 4/5).
- **nlist vs boot size** at 484M (16–64 MB boot). Consider compressing coarse centroids
  or a 2-level (IMI) coarse quantizer if boot is too heavy.
- **Re-rank storage** — full f32 vectors at 484M ≈ 1 TB (f16 ≈ 0.5 TB, SQ-int8 ≈ 0.25 TB)
  on S3. Range-fetched per query (cheap), but storage is large → default off; revisit.
- **Recipe match** (corpus vs query: weights + pooling + prefix) — the #1 correctness
  risk; Lambda-runs-the-model eliminates it, a hosted API must match it exactly.
- **Local corpus embed time** (~2–6 days on the Mac GPU, or a rented GPU) + **Lambda
  model size/cold-start** if running the model in-Lambda (favor a small model like nomic,
  or provisioned concurrency).
- **Per-query bytes** (~16 MB at the example params) — tune nlist↑/nprobe↓; the slow-
  mobile latency lens from the head-tuning work applies here too.
- Confirm `text-embedding-3-small` MRL truncation to 512 holds quality (vs 256/1536).

## Progress

### 2026-06-03 — Steps 1–2 done (RRVI format + writer + pure-Rust reader)
The pure-Rust foundation is implemented, tested, and lint-clean. Everything is
build-time-or-reader only, no external deps (no FAISS, no models, no network).

- **Format:** `VECTORS.md` documents the frozen `RRVI` v1 byte layout (48 B
  header; boot region = optional OPQ `D×D` + coarse centroids `nlist×D` + PQ
  codebooks `m×ksub×dsub` + cluster directory `nlist×(u64 off,u32 count)`;
  range-fetched region = per cluster `[u32 id×count][u8 code×(count·m)]`). All
  boot offsets derive from the header — no boot offset table.
- **Reader** (`rust/src/vector.rs`, `VectorIndex<F: RangeFetch>`, wasm-safe,
  always pure-Rust): one-time boot read, in-memory `nprobe` nearest-centroid
  pick, one concurrent wave of per-cluster list GETs, residual ADC scan, bounded
  top-k → `Vec<VectorHit{doc_id,score}>` (score = `1−dist/2` ≈ cosine for IP).
  Confirmed it compiles for `wasm32-unknown-unknown`.
- **Native trainer/writer** (`rust/src/vector_build.rs`, native-only): dependency-
  free k-means coarse quantizer + per-subspace PQ codebooks + per-vector codes;
  `build_ivfpq(vectors, IvfpqParams) -> Ivfpq`, `Ivfpq::write`/`to_bytes`.
  Deterministic (seeded xorshift). This is the test/small-corpus path; the FAISS
  exporter (step 3) will emit the same bytes for scale.
- **Gating:** behind a non-default `vector` feature (adds no deps). CI
  (`.github/workflows/ci.yml`) and `.githooks/pre-push` now run the crate gates a
  second time with `--features vector` so the module stays tested + linted.
- **Tests** (`rust/tests/vector.rs`, `--features vector`, all green): header
  round-trip; scan-all-clusters returns every doc id once, scores non-increasing;
  self-query is top-1 (score ~1); **recall@10 = 0.87 vs exact-cosine brute force**
  on 1600 clustered vectors (floor asserted at 0.75); identity-OPQ == no-OPQ
  (exercises the rotation path); edge cases (k/nprobe 0 → empty, dim mismatch →
  `IndexError::BadQuery`, empty build → error).

Remaining: **3** FAISS trainer (`python/`) + Rust exporter (FAISS→RRVI) at scale;
**4** model2vec build-time embedder (mode 2) → end-to-end query test; **5** wasm
bindings for the reader + in-browser model2vec embedder; **6** open-model corpus
embed + Lambda query embedder (mode 1); **7** optional re-rank blob + trigram
hybrid. Not committed yet (pending review).

### 2026-06-03 — Python (PyO3) bindings + Python CI/PyPI
Exposed the build-side vector path to Python and set up Python CI for a PyPI
release (user is creating the project).

- **`VectorBuilder`** in `python/src/lib.rs` (mirrors the existing text `Builder`):
  `VectorBuilder(dim, nlist, m, nbits=8, metric="ip"/"l2", kmeans_iters=25,
  seed=None)`, `.add(doc_id, vector)`, `.add_many(pairs)`, `.build(path)` →
  `VectorBuildStats(vectors,dim,nlist,m,nbits)`; writes one `.rrvi`. Wraps core
  `build_ivfpq`/`Ivfpq::write`; bad params/metric/dim → `ValueError`. Enabled the
  core `vector` feature on the `roaringrange_core` dep. Added `Ivfpq` getters
  (`dim/nlist/subquantizers/nbits`) so stats report post-clamp values.
- **Verified at runtime:** `maturin build` (abi3, cp38-abi3) → installed into a
  venv → `pytest python/tests` (9 tests) green; the smoke test parses the written
  `.rrvi` header and asserts it matches the returned stats.
- **CI** (`ci.yml`): `python-build` builds the abi3 wheel once; `python-test`
  installs it and runs pytest on **CPython 3.12, 3.13, 3.14** (abi3 forward-compat,
  so no build on 3.14). `release.yml`: per-platform wheels via `maturin-action` +
  **PyPI Trusted Publishing** (OIDC, env `pypi`) on `v*` tags — wheels only (the
  `../rust` path dep blocks a buildable sdist until the core crate is on crates.io).
- **Docs:** `python/README.md` (VectorBuilder usage + install), `rust/README.md`
  (`vector` feature), top-level `README.md` (specs row + similarity-search
  section), all updated.

### 2026-06-03 — Step 3 done (FAISS training + RRVI export at scale)
Production build path: train `OPQ,IVF,PQ` with FAISS, export the trained parts to
RRVI without retraining in Rust.

- **Rust** (`vector_build.rs`): `IvfpqParts` + `build_ivfpq_from_parts` — assemble
  an `Ivfpq` from already-trained centroids/codebooks/OPQ + per-vector
  (id, assignment, code), validating every length/range, then `write`. No k-means.
- **Python** (`python/src/lib.rs`): `write_rrvi_from_faiss(out, dim, nlist, m,
  centroids, codebooks, ids, assignments, codes, nbits=8, metric, opq=None)` —
  takes the FAISS arrays as **little-endian byte buffers** (numpy `.tobytes()`), so
  the wheel needs **no numpy dep**. Decodes, calls `build_ivfpq_from_parts`, writes.
- **Script** (`python/scripts/faiss_to_rrvi.py`, `[train]` extra = numpy+faiss-cpu):
  `export_to_rrvi(vectors, doc_ids, out, nlist, m, metric)` trains
  `OPQ{m},IVF{nlist},PQ{m}` (8-bit), extracts OPQ rotation (`A`, no bias), coarse
  centroids (rotated space), PQ codebooks, and per-vector cluster+code from the
  inverted lists (FAISS row id → doc_id), and calls the binding. `+report_recall`.
- **CLI** (`rust/examples/rrvi_query.rs`, `required-features=["vector"]`): reads an
  `.rrvi` + a queries blob, prints top-k doc IDs — the cross-check harness.
- **VERIFIED end-to-end with real FAISS 1.14 / numpy 2.4**: exported a 12.8K×64
  index, read it back through the Rust reader, and compared to FAISS's own IVFPQ
  search on the same index → **recall@10 = 0.9995** (top-10 identical, in order).
  Confirms the OPQ/centroid/codebook orientation. Rust tests:
  `from_parts_matches_hand_computed_adc` (exact ADC) + `from_parts_rejects_*`;
  pytest: `write_rrvi_from_faiss` header + validation (no numpy needed in CI).
- All gates green: 55 lib + 8 vector tests, 11 pytest, clippy (default + vector,
  incl. the example), fmt. Constraints: 8-bit PQ codes; OPQ bias must be zero.

Remaining: **4** model2vec build-time embedder (mode 2); **5** wasm bindings for
the reader + in-browser model2vec; **6** open-model corpus embed + Lambda (mode 1);
**7** re-rank blob + trigram hybrid.

### 2026-06-03 — Step 5 (reader half): wasm `RrviIndex` binding
The browser read path for similarity search.

- **`RrviIndex`** in `wasm.rs` (gated `#[cfg(feature = "vector")]`, so it appears
  only with `--features "wasm vector"`): `open(url)`, `search(Float32Array query,
  k, nprobe) -> RrviHits`, plus `dim/nlist/len/isEmpty`. `RrviHits` exposes aligned
  `ids` (Uint32Array) and `scores` (Float32Array), best-first. Reuses `WasmFetch`,
  mirroring `RrsIndex`. Build: `wasm-pack build --target web --features "wasm vector"`.
- Verified: compiles + clippy-clean for `wasm32-unknown-unknown` under both
  `"wasm vector"` (binding present) and `"wasm"` alone (binding gated out). The
  binding is thin glue over the natively-tested `VectorIndex` (8 tests + the FAISS
  cross-check); a full browser test needs wasm-pack + a served file (as with
  `RrsIndex`, exercised via the live demo) and is deferred.

Step 5 remainder = the in-browser **model2vec** query embedder (the hard part: a
wasm tokenizer). Demo wiring waits on a query embedder (mode 1 Lambda or mode 2
model2vec). Remaining steps: **4** model2vec (mode 2), **5b** in-browser model2vec,
**6** open-model + Lambda (mode 1), **7** re-rank + trigram hybrid.
