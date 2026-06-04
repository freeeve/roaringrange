# Container-image Lambda for the mode-1 query embedder. onnxruntime + tokenizers
# only (no torch) — the model is baked in as an ONNX graph by export_model.py.
#
#   python export_model.py --model google/embeddinggemma-300m --out model --dim 512
#   docker build -t embed-lambda .
# See README.md for pushing to ECR and creating the function.
FROM public.ecr.aws/lambda/python:3.12

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# Exported artifacts: model.onnx + tokenizer.json + recipe.json (run
# export_model.py first; `model/` is .gitignored — weights are never committed).
COPY model/ ${LAMBDA_TASK_ROOT}/model/
COPY handler.py ${LAMBDA_TASK_ROOT}/

CMD ["handler.lambda_handler"]
