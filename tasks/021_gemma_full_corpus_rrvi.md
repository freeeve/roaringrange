# 021 — Full-corpus (484M) Gemma embedding + RRVI (open-model vector mode)

**Status:** pending

Make the open-model Gemma vector path cover the **full 484M corpus**, as the peer to the
already-live **model2vec** 484M RRVI. This is the remaining feature work in task 004 (the
open-model "Mode 1" path) — `[[vector-search-plan]]`, `tasks/004_vector_search.in-progress.md`.

## Current state

- A **10M-subset** Gemma embed is running locally on the MacBook MPS
  (`embed_gemma_memmap.py 10000000 /tmp/oa-out/gemma-10m`, ~45–82 docs/s → ~2 days for 10M).
  That subset is for demo testing only (see the partial-RRVI note below), not the full corpus.
- The embed Lambda is built/deployed (`examples/embed-lambda`, "warm ~43ms behind CloudFront
  /embed"), but the demo's Mode-1 toggle is hidden because `rrviGemma`/`EMBED_LAMBDA_URL` are
  `null` in `index.html`.

## Why a GPU server (not the MacBook)

At ~45–82 docs/s on this Mac's MPS, 484M works ≈ **~68 days** — infeasible locally. The full
embed needs a GPU box (the `ec2-full-build.sh` pattern, but GPU-class). `build_full_rrvi_gemma.py`
+ `embed_gemma_memmap.py` are the streaming/resumable builders; they need to run where the GPU is.

## Scope

1. Embed all 484M works with **EmbeddingGemma-300M** (same model+recipe the query Lambda uses;
   `truncate_dim=512`, MRL) in rank order on a GPU server → `gemma-484m.{f32,ids}` (resumable
   via the checkpoint, like the local run).
2. Build the Gemma **IVFPQ RRVI** from the embeddings → `openalex-484m-gemma.rrvi`.
3. Upload to `s3://openalex-eve/`, set `rrviGemma` + `EMBED_LAMBDA_URL` in `index.html`'s `full`
   dataset to enable the Mode-1 (open-model) semantic toggle, and deploy.

## Acceptance

The demo's open-model (Gemma) semantic mode is live over the full 484M corpus, alongside the
model2vec mode, with query embedding served by the Lambda. Closes task 004's last item.
