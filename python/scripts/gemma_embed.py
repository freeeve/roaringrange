"""Mode-1 embedder: EmbeddingGemma-300M (an open, on-device-tier transformer) via
sentence-transformers. The corpus embeds locally for $0 and the query runs the
*same* model + recipe (the Lambda, or a host), so the spaces match.

Requires the `gemma` extra and HuggingFace access to a gated model:
    pip install 'roaringrange[gemma]'                 # sentence-transformers + torch
    huggingface-cli login                             # token that accepted the
    # ...after accepting terms at hf.co/google/embeddinggemma-300m  # Gemma license

CRITICAL recipe note (the #1 correctness risk): EmbeddingGemma is **asymmetric** —
documents and queries use different task prompts. Embed the corpus with
`embed_documents` and the query with `embed_query`; mixing them breaks retrieval.
EmbeddingGemma is 768-d with Matryoshka (MRL) — `dim` truncates + renormalizes to
256/512/768 (the spec leans 256 for mode 1; we default 512 to match the format).
"""
from __future__ import annotations

import numpy as np

DEFAULT_MODEL = "google/embeddinggemma-300m"


def load(model_name: str = DEFAULT_MODEL, dim: int = 512):
    """Loads EmbeddingGemma with MRL truncation to `dim` (renormalized by ST)."""
    try:
        from sentence_transformers import SentenceTransformer
    except ImportError as e:  # pragma: no cover - environment dependent
        raise SystemExit(
            "sentence-transformers is required: pip install 'roaringrange[gemma]' "
            "(and accept the Gemma license + `huggingface-cli login`)"
        ) from e
    return SentenceTransformer(model_name, truncate_dim=dim)


def embed_documents(model, texts) -> np.ndarray:
    """Document-side embeddings (use for the corpus / `.rrvi` build)."""
    return np.asarray(
        model.encode_document(list(texts), normalize_embeddings=True),
        dtype="float32",
    )


def embed_query(model, texts) -> np.ndarray:
    """Query-side embeddings (use at search time — different prompt than documents)."""
    return np.asarray(
        model.encode_query(list(texts), normalize_embeddings=True),
        dtype="float32",
    )
