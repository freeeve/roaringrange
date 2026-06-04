"""Export a model2vec static model (default potion-retrieval-32M) to a single
`RRM2` artifact the in-browser/Rust `Model2vec` embedder reads: the vocab + the
token-embedding matrix (int8 per-row quantized) + the BertNormalizer flags. The
browser downloads this once (cached) and embeds queries with no backend.

Validates that an int8 mean-pool reproduces the fp32 model2vec output (cosine).

    pip install 'roaringrange[embed]'
    python model2vec_export.py --model minishlab/potion-retrieval-32M --out potion.rrm2

RRM2 layout (all integers little-endian):
  magic "RRM2"[4] | version u16=1 | dim u32 | vocab u32 | quant u8(0=int8/row) |
  flags u8 (bit0 lowercase, bit1 strip_accents, bit2 handle_chinese, bit3 clean_text) |
  unk_id u32 | reserved -> 32-byte header
  scales: vocab x f32                  (per-row dequant scale)
  codes:  vocab x dim x i8             (row_i ~= codes_i * scale_i)
  vocab:  per id -> u16 len + utf8 bytes (id order)
"""
from __future__ import annotations

import argparse
import json
import struct

import numpy as np


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--model", default="minishlab/potion-retrieval-32M")
    p.add_argument("--out", default="potion.rrm2")
    a = p.parse_args()

    from model2vec import StaticModel

    m = StaticModel.from_pretrained(a.model)
    emb = np.asarray(m.embedding, dtype=np.float32)  # (vocab, dim)
    vocab_size, dim = emb.shape
    tok = m.tokenizer
    tjson = json.loads(tok.to_str())
    norm = tjson.get("normalizer", {}) or {}
    lowercase = bool(norm.get("lowercase", True))
    strip_accents = norm.get("strip_accents")
    strip_accents = lowercase if strip_accents is None else bool(strip_accents)
    handle_chinese = bool(norm.get("handle_chinese_chars", True))
    clean_text = bool(norm.get("clean_text", True))
    unk_id = tok.token_to_id("[UNK]")
    print(f"vocab {vocab_size} dim {dim}; lowercase={lowercase} strip_accents={strip_accents} "
          f"handle_chinese={handle_chinese} clean_text={clean_text} unk_id={unk_id}")

    # int8 per-row quantization: scale_i = max(|row_i|)/127.
    row_max = np.abs(emb).max(axis=1)
    scales = np.where(row_max > 0, row_max / 127.0, 1.0).astype(np.float32)
    codes = np.clip(np.round(emb / scales[:, None]), -127, 127).astype(np.int8)

    # Fidelity: int8 mean-pool vs fp32 model2vec output, on sample texts.
    deq = codes.astype(np.float32) * scales[:, None]
    worst = 1.0
    for txt in ["Hello World", "self-supervised representation learning",
                "CRISPR-Cas9 genome editing", "café NAÏVE Über"]:
        ids = tok.encode(txt, add_special_tokens=False).ids
        ref = m.encode([txt])[0]
        v = deq[ids].mean(0)
        v = v / (np.linalg.norm(v) or 1.0)
        cos = float(np.dot(v, ref))
        worst = min(worst, cos)
        print(f"  int8 cos={cos:.5f}  {txt!r}")
    print(f"worst int8-vs-fp32 cosine: {worst:.5f}")

    flags = (lowercase << 0) | (strip_accents << 1) | (handle_chinese << 2) | (clean_text << 3)
    with open(a.out, "wb") as f:
        header = struct.pack("<4sHIIBBI", b"RRM2", 1, dim, vocab_size, 0, flags, unk_id)
        f.write(header + b"\x00" * (32 - len(header)))
        f.write(scales.tobytes())
        f.write(codes.tobytes())
        for i in range(vocab_size):
            t = tok.id_to_token(i).encode("utf-8")
            f.write(struct.pack("<H", len(t)) + t)
    import os
    print(f"wrote {a.out} ({os.path.getsize(a.out)/1e6:.1f} MB)")


if __name__ == "__main__":
    main()
