# embed-lambda — mode-1 query embedder

A container-image AWS Lambda that turns query **text** into a query **vector** with
the *same* open model the corpus was embedded with (EmbeddingGemma-300M), so the
spaces match. The browser calls this for the query vector only; the WASM reader
(`RrviIndex`) then does the similarity search itself over S3 range reads. The
heavy work — embedding the 484M-doc corpus — happened locally for $0; this is just
the per-query embed.

```
browser ──"q"──▶ embed-lambda (EmbeddingGemma ONNX) ──vector──▶ browser
browser ──RrviIndex.search(vector)──▶ S3 range reads ──▶ doc IDs ──▶ records
```

Why a Lambda running the model (not Bedrock): the query vector must come from the
identical model + prompt + pooling as the corpus, and the model is open so it runs
in our own pay-per-invoke function. Bedrock only offers *closed* embedders
(Titan/Cohere) → different space + paid corpus pass. See `VECTORS.md`.

## Runtime: onnxruntime, no torch
`export_model.py` exports the **transformer** to ONNX with optimum (raw
`torch.onnx.export` trips on Gemma3 attention; optimum's sentence-transformers
export clashes with current ST — so we export the transformer and replay the rest)
and extracts the model's post-steps — masked mean-pool, the dense layers
(`dense.npz` + activations), the query prompt — into `recipe.json`. The image
carries only `onnxruntime` + `tokenizers` + `numpy` (no torch). `handler.py`
replays exactly: query-prompt → tokenize → transformer ONNX → masked mean-pool →
dense → normalize → MRL-truncate. Validation confirms this reproduces
`encode_query` at **cosine 1.0** (EmbeddingGemma's 2 dense layers are Identity).

## Build

1. **Export the model** (needs torch + the gated model — one-time, local):
   ```sh
   pip install 'roaringrange[gemma]' optimum optimum-onnx onnxruntime
   # accept terms at hf.co/google/embeddinggemma-300m, then `huggingface-cli login`
   python export_model.py --model google/embeddinggemma-300m --out model --dim 512
   ```
   This writes `model/{model.onnx,dense.npz,tokenizer.json,recipe.json}` and
   **validates** that the Lambda recipe reproduces `encode_query` (asserts cosine
   > 0.999; got 1.0). The query prompt + dense activations are captured from the
   model into `recipe.json`/`dense.npz` — never hard-coded. (`model/` is
   git-ignored; weights are never committed. `model.onnx` is ~1.1 GB fp32 —
   int8-quantize it with optimum for a smaller image; re-run the validation.)

2. **Build + push the image:**
   ```sh
   docker build -t embed-lambda .
   aws ecr create-repository --repository-name embed-lambda
   ACCT=$(aws sts get-caller-identity --query Account --output text); REGION=us-east-1
   aws ecr get-login-password --region $REGION | docker login --username AWS \
       --password-stdin $ACCT.dkr.ecr.$REGION.amazonaws.com
   docker tag embed-lambda $ACCT.dkr.ecr.$REGION.amazonaws.com/embed-lambda:latest
   docker push $ACCT.dkr.ecr.$REGION.amazonaws.com/embed-lambda:latest
   ```

3. **Create the function** (container image; give it room + a warm path):
   ```sh
   aws lambda create-function --function-name embed-lambda \
       --package-type Image \
       --code ImageUri=$ACCT.dkr.ecr.$REGION.amazonaws.com/embed-lambda:latest \
       --role <execution-role-arn> --memory-size 3008 --timeout 30 --architectures arm64
   aws lambda create-function-url-config --function-name embed-lambda \
       --auth-type NONE --cors '{"AllowOrigins":["*"],"AllowMethods":["GET","POST"]}'
   ```
   Cold start loads the ONNX once; add **provisioned concurrency** (or a keep-warm
   ping) for a low-latency demo. Build the image on `arm64` to match `--architectures`.

## Use

```sh
curl "$FUNCTION_URL?q=self-supervised+representation+learning"
# {"vector":[...512 floats...],"dim":512}
```

In the browser: `fetch(URL+"?q="+enc).then(r=>r.json())` → `new Float32Array(j.vector)`
→ `rrvi.search(vec, k, nprobe)`.

## Notes
- **Recipe match is the whole game.** Build the corpus `.rrvi` with the *document*
  side of the same model (`embed_documents` in `python/scripts/gemma_embed.py`,
  `dim=512`); this Lambda uses the *query* side. `export_model.py`'s cosine check
  guards it.
- For the full 484M corpus, int8-quantize the ONNX (`optimum`'s dynamic
  quantization) to shrink the image and speed inference; re-run the validation.
