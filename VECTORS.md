# RRVI — roaring range vector index (`RRVI`, version 1)

Range-fetchable layout for **similarity / semantic search** in the same static,
no-backend ethos as the trigram index (`FORMAT.md`). The index is an **IVFPQ**
structure — an inverted file of coarse clusters, each indexed vector stored as a
compact **product-quantization (PQ)** code. A browser downloads a small **boot**
region once (coarse centroids + PQ codebooks + a cluster directory, a few MB),
then each query range-fetches only the `nprobe` nearest clusters' code lists —
independent of corpus size, exactly like the trigram reader's head/tail postings.

`vector_id == doc_id`, so a hit maps straight back through the `RRSR` record
store (`RECORDS.md`) and can hybridize with the trigram (`RRSI`) result set.

All integers little-endian; all vectors/centroids/codebooks are `f32`. Vectors
are **L2-normalized** so cosine == inner product == a monotone function of L2 on
the unit sphere — the reader scans in squared-L2 and reports a cosine-like score.

## Layout

**Header — 48 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRVI"` |
| version | u16 | 2 | `1` |
| metric | u8 | 1 | `0` = inner product (cosine, normalized), `1` = L2 |
| flags | u8 | 1 | bit 0 = an OPQ rotation precedes the centroids |
| dim | u32 | 4 | vector dimensionality `D` |
| nlist | u32 | 4 | coarse (IVF) cluster count |
| m | u32 | 4 | PQ subquantizers (`D % m == 0`) |
| nbits | u8 | 1 | bits per PQ code (`1..=8`); `ksub = 1<<nbits` codes/subspace |
| pad | u8[3] | 3 | zero |
| n | u64 | 8 | total indexed vector count |
| reserved | u8[16] | 16 | zero (room for a future re-rank-blob offset, etc.) |

Let `H = 48`, `ksub = 1<<nbits`, `dsub = D/m`. Every boot-region offset below is
**derived from the header** — there is no offset table for the boot blobs.

**Boot region** (downloaded once, contiguous from `H`):
| blob | present when | size (bytes) |
|---|---|---|
| OPQ rotation `R` (`D×D`, row-major) | `flags & 1` | `D·D·4` |
| coarse centroids (`nlist × D`, row-major) | always | `nlist·D·4` |
| PQ codebooks (`m × ksub × dsub`, row-major) | always | `m·ksub·dsub·4` |
| cluster directory (`nlist` entries) | always | `nlist·12` |

Each **directory entry** is `(offset u64, count u32)`: the absolute file offset of
that cluster's code list and the number of vectors in it. `lists_off` (start of
the range-fetched region) is `H + sizeof(all boot blobs above)`.

**Range-fetched region** — per cluster, contiguous at its directory `offset`:
`[ u32 vector_id × count ][ u8 PQ-code × (count · m) ]`. The ids occupy
`count·4` bytes; the codes follow, `m` bytes per vector (one byte per subquantizer
since `nbits ≤ 8`). A query fetches one such block per probed cluster.

Sizing (484M vectors, `D=256`, `nlist=16384`, `m=32`): boot ≈ centroids
`16384·256·4 ≈ 16 MB` + codebooks `32·256·8·4 ≈ 256 KB` + directory `192 KB`;
per cluster ≈ `30K·(4+32) ≈ ~1 MB`, so `nprobe=16` ≈ ~16 MB/query. Tune `nlist`↑
/ `nprobe`↓ to trade boot size, per-query bytes, and recall.

## Reader
- **boot:** read the 48 B header, then one ranged read of the whole boot region;
  keep OPQ (if any), centroids, codebooks, and the directory in memory.
- **search(query, k, nprobe):**
  1. (metric `0`) L2-normalize `query`; if OPQ present, rotate `q' = R · q`.
  2. find the `nprobe` nearest centroids to `q'` by squared L2 — wholly in memory.
  3. range-fetch those clusters' code lists in one concurrent wave.
  4. per probed cluster `j`: form the residual `r = q' − centroid[j]`, build the
     ADC table `table[s·ksub + t] = ‖r_s − codebook[s][t]‖²` (query **not**
     quantized — asymmetric distance computation), then for each code in the list
     accumulate `dist = Σ_s table[s·ksub + code_s]` into a bounded top-`k` heap.
  5. return the `k` smallest-distance `(doc_id, score)` pairs, best-first. Score is
     `1 − dist/2` (≈ cosine) for metric `0`, or `−dist` for metric `1`.

Because each cluster's residual depends on its centroid, the ADC table is rebuilt
per probed cluster — the standard residual-IVFPQ path.

## Build
The pure-Rust trainer (`roaringrange::build_ivfpq`, native, behind the `vector`
feature) trains the coarse quantizer and PQ codebooks with dependency-free
k-means and emits this layout — intended for tests and small/medium corpora. The
production path trains the index with FAISS (`OPQ`,`IVF`,`PQ`) and
`roaringrange::build_ivfpq_from_parts` writes the identical bytes (the reader is
the same either way). One `RRVI` per embedding model (each model is a different
vector space).

## Re-rank sidecar (`RRVR`, optional)
PQ ADC is lossy. The optional `<name>.rrvi.rerank` sidecar stores each vector at
higher precision so a query can fetch the exact vectors for only its top-`r` PQ
candidates and rescore them (`VectorIndex::search_rerank`). It is range-fetched —
`r` small ranged reads — and **off by default** (full storage is large: ~248 GB
at 484M·256·bf16).

**Header — 20 B:** magic `"RRVR"`, version `u16`=1, precision `u8`, pad `u8`,
dim `u32`, n `u64`. **Body:** a dense array keyed by doc ID — vector `id` is at
`20 + id·dim·2`. Precision `0` = **bf16** (the high 16 bits of each f32: full f32
exponent range, 8-bit mantissa, trivially exact to decode, 2 bytes/dim). Written
by `roaringrange::write_rerank`.

## Hybrid (vector + trigram)
`reciprocal_rank_fusion(&[vector_ids, trigram_ids], k≈60)` blends an `RRVI` result
list and an `RRS` (trigram) result list into one ranking with no score
normalization — a doc near the top of either list ranks high. Exposed to the
browser as `reciprocalRankFusion(vectorIds, trigramIds, kParam)`.

## Model2vec embedder (RRM2)

To embed a query **in the browser with no backend**, the `<name>.rrm2` artifact carries a
static model2vec embedder: a WordPiece vocab + an int8-quantized token-embedding matrix + the
BERT normalizer flags. `Model2vec::from_bytes` (wasm: `Model2vecEmbedder`) reads the whole file
once and `embed(query)` does tokenize → gather static token rows → mean-pool → L2-normalize,
byte-compatible with the Python model2vec; the result feeds `VectorIndex::search`. Emitted by
`python/scripts/model2vec_export.py`.

All integers little-endian.

| section | bytes | contents |
|---|---|---|
| header | 32 | magic `"RRM2"`; version `u16`=1; `dim u32`; `vocabSize u32`; `quant u8` (0 = int8); `flags u8`; `unkId u32`; reserved to 32 B |
| scales | `vocabSize × 4` | per-row dequant scale `f32` (`row = code × scale`) |
| codes | `vocabSize × dim` | int8 embedding codes, row-major |
| vocab | variable | token strings in id order, each `[len u16][UTF-8 bytes]` |

`flags` bits: `1`=lowercase, `2`=strip-accents, `4`=handle-CJK, `8`=clean-text (the BertNormalizer
settings, applied identically at query time so the in-browser tokenization matches the export).
