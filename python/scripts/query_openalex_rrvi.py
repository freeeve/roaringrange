"""Query an OpenAlex `RRVI` from the CLI, for eyeballing relevance: embed a text
query with the same model2vec model, search the `.rrvi` via the crate's
`rrvi_query` example, and print the matching titles from the dumped records.

    python python/scripts/query_openalex_rrvi.py \\
        openalex-head.rrvi head.jsonl "self-supervised representation learning"

(The production query path is the in-browser wasm reader; this just exercises the
same index from the shell.)
"""
from __future__ import annotations

import argparse
import json
import os
import struct
import subprocess
import sys
import tempfile

sys.path.insert(0, "python/scripts")
from model2vec_embed import embed_texts  # noqa: E402


def load_titles(jsonl: str) -> dict[int, str]:
    titles = {}
    with open(jsonl, encoding="utf-8") as f:
        for line in f:
            did, _, js = line.partition("\t")
            titles[int(did)] = json.loads(js).get("t") or ""
    return titles


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("rrvi")
    p.add_argument("jsonl")
    p.add_argument("query", nargs="+")
    p.add_argument("--k", type=int, default=10)
    p.add_argument("--nprobe", type=int, default=16)
    p.add_argument("--model", default="minishlab/potion-retrieval-32M")
    a = p.parse_args()

    q = " ".join(a.query)
    qv = embed_texts([q], a.model)[0].astype("<f4")
    qpath = tempfile.mktemp(suffix=".bin")
    with open(qpath, "wb") as f:
        f.write(struct.pack("<II", 1, qv.shape[0]))
        f.write(qv.tobytes())

    run = subprocess.run(
        ["cargo", "run", "--release", "--quiet", "--example", "rrvi_query",
         "--features", "vector", "--", a.rrvi, qpath, str(a.k), str(a.nprobe)],
        cwd="rust", capture_output=True, text=True,
    )
    os.unlink(qpath)
    if run.returncode:
        print(run.stderr, file=sys.stderr)
        sys.exit(1)
    ids = [int(x) for x in run.stdout.split()]

    titles = load_titles(a.jsonl)
    print(f"query: {q!r}")
    for rank, doc_id in enumerate(ids, 1):
        print(f"{rank:2}. [{doc_id}] {titles.get(doc_id, '?')[:96]}")


if __name__ == "__main__":
    main()
