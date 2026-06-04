"""Build an `RRVI` similarity index for the OpenAlex demo from dumped records.

Input is the `<doc_id>\\t<record-json>` stream produced by the crate's
`dump_records` example (records are in rank order, so a prefix is the top-N most
cited works). Each record's title (`t`) and abstract (`ab`) are embedded with a
model2vec static model, then FAISS trains `OPQ,IVF,PQ` and exports the `.rrvi`.

    cargo run --release --example dump_records --features zstd -- \\
        records.idx records.bin records.dict 100000 > head.jsonl
    pip install 'roaringrange[train,embed]'
    python python/scripts/build_openalex_rrvi.py head.jsonl openalex-head.rrvi

`doc_id` is preserved, so a hit maps to the same record as the trigram index.
"""
from __future__ import annotations

import argparse
import json
import sys

import numpy as np

sys.path.insert(0, "python/scripts")
from faiss_to_rrvi import export_to_rrvi  # noqa: E402
from model2vec_embed import embed_texts  # noqa: E402


def read_jsonl(path: str):
    """Yields `(doc_id, text)` from a `<doc_id>\\t<json>` file; text = title +
    abstract."""
    with open(path, encoding="utf-8") as f:
        for line in f:
            did, _, js = line.partition("\t")
            rec = json.loads(js)
            t = rec.get("t") or ""
            ab = rec.get("ab") or ""
            yield int(did), (t + " " + ab).strip()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("jsonl", help="<doc_id>\\t<json> records from dump_records")
    p.add_argument("out", help="output .rrvi path")
    p.add_argument("--nlist", type=int, default=1024)
    p.add_argument("--m", type=int, default=32, help="PQ subquantizers (must divide dim)")
    p.add_argument("--model", default="minishlab/potion-retrieval-32M")
    p.add_argument("--limit", type=int, default=0, help="cap rows (0 = all)")
    args = p.parse_args()

    ids, texts = [], []
    for did, text in read_jsonl(args.jsonl):
        ids.append(did)
        texts.append(text)
        if args.limit and len(ids) >= args.limit:
            break
    print(f"read {len(ids)} records; embedding with {args.model} ...", flush=True)

    emb = embed_texts(texts, args.model)
    print(f"embedded -> {emb.shape}; training FAISS + exporting ...", flush=True)

    doc_ids = np.asarray(ids, dtype="uint32")
    stats = export_to_rrvi(emb, doc_ids, args.out, args.nlist, args.m, metric="ip")
    print("wrote", args.out, "->", stats)


if __name__ == "__main__":
    main()
