# hybrid-search-lambda

Regional **hybrid search** Lambda for the OpenAlex demo. One crate, deployed as **two**
functions via the `TEXT_MODE` env:

- `roaringrange-hybrid-tri` — trigram intersection (`Index`, `.rrs`) ⊕ Gemma vector arm
- `roaringrange-hybrid-term` — BM25 term (`TermIndex` + `ImpactIndex`) ⊕ Gemma vector arm
  (this is "BM25 with semantic")

Both arms run in-region over S3 range reads and are fused by **reciprocal rank fusion**;
only the paged IDs + facet counts cross the wire. Unlike the trigram/term search Lambdas,
this one **embeds the query in-process** (EmbeddingGemma via onnxruntime) — no cross-Lambda
`/embed` hop — which is why it ships as a **container image** (the 305 MB int8 model can't
fit a 250 MB zip).

The embedder (`src/embed.rs`) is a byte-faithful Rust port of the Python `embed-lambda`
(`handler.py`): query-prompt → tokenize → ONNX → masked mean-pool → dense head → L2 →
MRL-truncate to 512 → L2. Verified at cosine `0.99999999` vs the Python embed on the same
onnxruntime version (`cargo run --example embed_check -- "<query>"` with `ORT_DYLIB_PATH` set).

## Build & deploy

The `bootstrap` binary is built with cargo-lambda (arm64, `ort` `load-dynamic`, so it
dlopens onnxruntime at runtime); the image bakes in the onnxruntime `.so` + the model.

```sh
# 1. Stage the model artifacts (gitignored; from the embed lambda's export_model.py output)
mkdir -p model
cp ../embed-lambda/model/{model.int8.onnx,tokenizer.json,recipe.json} model/
python3 -c "import numpy as np; d=np.load('../embed-lambda/model/dense.npz'); \
  [np.ascontiguousarray(d[k].astype('<f4')).tofile(f'model/dense_{k.lower()}.bin') for k in ['W0','b0','W1','b1']]"

# 2. onnxruntime shared lib for linux/aarch64 (dlopened via ORT_DYLIB_PATH in the image)
pip download --platform manylinux_2_28_aarch64 --only-binary=:all: --no-deps onnxruntime==1.24.1 -d /tmp/ort
( cd /tmp/ort && unzip -o *.whl )
cp /tmp/ort/onnxruntime/capi/libonnxruntime.so.1.24.1 ./libonnxruntime.so

# 3. Build the runtime binary, stage it into the build context
cargo lambda build --release --arm64
cp target/lambda/bootstrap/bootstrap ./bootstrap

# 4. Build + push the image (single-arch — Lambda rejects buildx attestation manifests)
ECR=499548155503.dkr.ecr.us-east-1.amazonaws.com/roaringrange-hybrid
aws ecr get-login-password | docker login --username AWS --password-stdin "${ECR%/*}"
docker buildx build --platform linux/arm64 --provenance=false --sbom=false -t $ECR:vX.Y.Z --push .

# 5. Deploy as two Image functions (TEXT_MODE differs); set the ECR repo policy so Lambda can pull
#    (Principal lambda.amazonaws.com: ecr:BatchGetImage + ecr:GetDownloadUrlForLayer), and on
#    create set --image-config '{"Command":["bootstrap"]}' (the provided.al2023 entrypoint needs
#    a handler arg). Front each with API Gateway + a CloudFront /search-hybrid-{tri,term} behavior.
```

Env (most come from the Dockerfile `ENV`): `INDEX_BUCKET`, `TEXT_MODE` (trigram|term),
`RRVI_KEY`, `INDEX_FACETS_KEY`; trigram needs `TRIGRAM_KEY`, term needs `TERM_KEY` +
`IMPACTS_KEY`; `ORT_DYLIB_PATH` + `EMBED_MODEL_DIR` are baked into the image.

## Cold start

Container init is ~8 s (tri) / ~11 s (term): ONNX session build (~4–8 s) overlapped with the
index opens (~2.5–5.5 s). The first cold start after a new image deploy adds ~10 s of image
lazy-load (one-off, still under API Gateway's 29 s cap). Warm requests are ~0.3–0.6 s. For
zero cold-start latency, add provisioned concurrency.
