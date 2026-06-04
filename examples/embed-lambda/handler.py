"""Query-embedding Lambda (mode 1): text -> EmbeddingGemma query vector.

The browser sends the query text here; this returns just the query vector, and the
wasm reader does the RRVI similarity search client-side over S3 range reads. The
vector is byte-identical to the corpus recipe because the same ONNX (exported from
the same sentence-transformers model) and the same query prompt are used — see
`export_model.py`, which validates the match.

Runtime deps: onnxruntime + tokenizers + numpy only (no torch). The model artifacts
(`model/model.onnx`, `tokenizer.json`, `recipe.json`) are baked into the image.

Invoke via a Lambda Function URL: `GET ?q=<text>` or `POST {"q": "<text>"}` ->
`{"vector": [f32...], "dim": D}`. CORS-open for the static site.
"""
import base64
import json
import os

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

_MODEL_DIR = os.path.join(os.path.dirname(__file__), "model")
_sess = None
_tok = None
_recipe = None
_in_names: set[str] = set()


def _load():
    """Lazy one-time load, reused across warm invocations."""
    global _sess, _tok, _recipe, _in_names
    if _sess is None:
        _recipe = json.load(open(os.path.join(_MODEL_DIR, "recipe.json")))
        _tok = Tokenizer.from_file(os.path.join(_MODEL_DIR, "tokenizer.json"))
        _sess = ort.InferenceSession(
            os.path.join(_MODEL_DIR, "model.onnx"),
            providers=["CPUExecutionProvider"],
        )
        _in_names = {i.name for i in _sess.get_inputs()}
    return _sess, _tok, _recipe


def embed_query(text: str) -> np.ndarray:
    """Embeds one query: query-prompt + tokenize -> ONNX -> MRL-truncate -> L2."""
    sess, tok, recipe = _load()
    enc = tok.encode(recipe.get("query_prompt", "") + text)
    feed = {}
    if "input_ids" in _in_names:
        feed["input_ids"] = np.array([enc.ids], dtype=np.int64)
    if "attention_mask" in _in_names:
        feed["attention_mask"] = np.array([enc.attention_mask], dtype=np.int64)
    out = sess.run(None, feed)[0][0]
    dim = int(recipe.get("dim", out.shape[0]))
    v = out[:dim].astype(np.float32)
    norm = float(np.linalg.norm(v))
    if norm > 0:
        v = v / norm
    return v


def _query_from_event(event) -> str:
    params = event.get("queryStringParameters") or {}
    q = params.get("q") or ""
    if not q and event.get("body"):
        body = event["body"]
        if event.get("isBase64Encoded"):
            body = base64.b64decode(body).decode("utf-8")
        try:
            q = (json.loads(body) or {}).get("q", "")
        except (ValueError, TypeError):
            q = ""
    return (q or "").strip()


def lambda_handler(event, context):
    headers = {
        "content-type": "application/json",
        "access-control-allow-origin": "*",
        "access-control-allow-headers": "content-type",
    }
    if (event.get("requestContext", {}).get("http", {}).get("method")) == "OPTIONS":
        return {"statusCode": 204, "headers": headers, "body": ""}

    q = _query_from_event(event)
    if not q:
        return {"statusCode": 200, "headers": headers, "body": json.dumps({"vector": [], "dim": 0})}
    v = embed_query(q)
    return {
        "statusCode": 200,
        "headers": headers,
        "body": json.dumps({"vector": [float(x) for x in v], "dim": int(v.shape[0])}),
    }
