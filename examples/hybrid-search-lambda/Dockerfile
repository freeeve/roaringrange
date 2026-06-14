# Container-image Lambda for the hybrid search function (deployed twice via TEXT_MODE).
# Unlike the zip-based trigram/term search Lambdas, this one bakes in the EmbeddingGemma
# ONNX model so it can embed the query in-process (no cross-Lambda /embed hop), which the
# 305 MB int8 model can't fit in a 250 MB zip.
#
# The bootstrap is built OUTSIDE Docker with cargo-lambda (arm64, provided.al2023-compatible,
# `ort` load-dynamic so it dlopens onnxruntime at runtime):
#   cargo lambda build --release --arm64
#   cp target/lambda/bootstrap/bootstrap ./bootstrap
# and the model/ artifacts are staged from ../embed-lambda/model (+ dense_*.bin). See README.
FROM public.ecr.aws/lambda/provided:al2023

# onnxruntime shared library (linux/aarch64), dlopened by `ort` via ORT_DYLIB_PATH.
COPY libonnxruntime.so /var/task/lib/libonnxruntime.so
ENV ORT_DYLIB_PATH=/var/task/lib/libonnxruntime.so

# Baked EmbeddingGemma artifacts: model.int8.onnx + tokenizer.json + recipe.json + dense_*.bin.
COPY model/ /var/task/model/
ENV EMBED_MODEL_DIR=/var/task/model

# The custom-runtime executable. The provided.al2023 entrypoint (/lambda-entrypoint.sh)
# requires a handler name as its one argument, then execs /var/runtime/bootstrap; the Rust
# bootstrap ignores the handler arg, but CMD must supply it.
COPY bootstrap /var/runtime/bootstrap
CMD ["bootstrap"]
