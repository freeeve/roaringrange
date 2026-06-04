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
(Titan/Cohere) → different space + paid corpus pass. See `VECTORS.md` / task 004.

## Runtime: onnxruntime, no torch
`export_model.py` bakes the whole sentence-transformers pipeline (transformer +
pooling + dense + normalize) into one ONNX graph at build time; the image carries
only `onnxruntime` + `tokenizers` + `numpy` (small, fast cold start). The handler
just applies the query prompt, tokenizes, runs the graph, MRL-truncates, and
L2-normalizes.

## Build

1. **Export the model** (needs torch + the gated model — one-time, local):
   ```sh
   pip install 'roaringrange[gemma]' optimum onnxruntime
   # accept terms at hf.co/google/embeddinggemma-300m, then `huggingface-cli login`
   python export_model.py --model google/embeddinggemma-300m --out model --dim 512
   ```
   This writes `model/{model.onnx,tokenizer.json,recipe.json}` and **validates**
   that the Lambda's onnxruntime recipe reproduces `encode_query` (cosine > 0.999).
   The query prompt is captured from the model into `recipe.json` — never
   hard-coded. (`model/` is git-ignored; weights are never committed.)

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
