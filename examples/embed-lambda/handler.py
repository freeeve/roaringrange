"""Query-embedding Lambda (mode 1): text -> EmbeddingGemma query vector.

The browser sends the query text here; this returns just the query vector, and the
wasm reader runs the RRVI similarity search client-side over S3 range reads. The
vector is byte-identical to the corpus recipe: the same transformer ONNX, the same
query prompt, and the same post-steps (masked mean-pool -> the model's dense layers
-> normalize -> MRL truncate). `export_model.py` validates the match.

Runtime deps: onnxruntime + tokenizers + numpy (no torch). Artifacts baked into the
image under `model/`: `model.onnx` (transformer), `tokenizer.json`, `recipe.json`
(prompts + dim + dense activations), `dense.npz` (the dense layers' weights).

Invoke via a Lambda Function URL: `GET ?q=<text>` or `POST {"q": "<text>"}` ->
`{"vector": [f32...], "dim": D}`. CORS-open for the static site.
"""
import base64
import json
import math
import os

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

_DIR = os.environ.get("EMBED_MODEL_DIR", os.path.join(os.path.dirname(__file__), "model"))
_sess = None
_tok = None
_recipe = None
_dense: list = []
_in_names: set = set()
_out_name = None


def _load():
    """Lazy one-time load, reused across warm invocations."""
    global _sess, _tok, _recipe, _dense, _in_names, _out_name
    if _sess is None:
        _recipe = json.load(open(os.path.join(_DIR, "recipe.json")))
        _tok = Tokenizer.from_file(os.path.join(_DIR, "tokenizer.json"))
        # Prefer the int8-quantized model (4x smaller, faster) when present.
        onnx = "model.int8.onnx"
        if not os.path.exists(os.path.join(_DIR, onnx)):
            onnx = "model.onnx"
        _sess = ort.InferenceSession(
            os.path.join(_DIR, onnx), providers=["CPUExecutionProvider"]
        )
        _in_names = {i.name for i in _sess.get_inputs()}
        outs = [o.name for o in _sess.get_outputs()]
        _out_name = "last_hidden_state" if "last_hidden_state" in outs else outs[0]
        npz_path = os.path.join(_DIR, "dense.npz")
        if os.path.exists(npz_path):
            d = np.load(npz_path)
            _dense = [(d[f"W{i}"], d[f"b{i}"]) for i in range(int(_recipe.get("n_dense", 0)))]


def _activation(name: str, x: np.ndarray) -> np.ndarray:
    if name in ("Identity", "Linear", ""):
        return x
    if name == "Tanh":
        return np.tanh(x)
    if name == "ReLU":
        return np.maximum(x, 0.0)
    if name in ("GELU", "GELUActivation"):
        return 0.5 * x * (1.0 + np.vectorize(math.erf)(x / math.sqrt(2.0)))
    raise ValueError(f"unsupported dense activation {name!r}")


def embed_query(text: str) -> np.ndarray:
    """query-prompt + tokenize -> transformer ONNX -> masked mean-pool -> dense
    layers -> normalize -> MRL-truncate -> renormalize."""
    _load()
    enc = _tok.encode(_recipe.get("query_prompt", "") + text)
    ids = np.array([enc.ids], dtype=np.int64)
    mask = np.array([enc.attention_mask], dtype=np.int64)
    feed = {}
    if "input_ids" in _in_names:
        feed["input_ids"] = ids
    if "attention_mask" in _in_names:
        feed["attention_mask"] = mask
    if "token_type_ids" in _in_names:
        feed["token_type_ids"] = np.zeros_like(ids)

    lhs = np.asarray(_sess.run([_out_name], feed)[0])[0]  # (seq, hidden)
    m = mask[0][:, None].astype(np.float32)
    pooled = (lhs * m).sum(axis=0) / max(float(m.sum()), 1.0)

    x = pooled.astype(np.float32)
    for (w, b), act in zip(_dense, _recipe.get("dense_acts", [])):
        x = x @ w.T + b
        x = _activation(act, x)

    norm = float(np.linalg.norm(x))
    if norm > 0:
        x = x / norm
    dim = int(_recipe.get("dim", x.shape[0]))
    v = x[:dim]
    n2 = float(np.linalg.norm(v))
    if n2 > 0:
        v = v / n2
    return v.astype(np.float32)


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
    if event.get("requestContext", {}).get("http", {}).get("method") == "OPTIONS":
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
