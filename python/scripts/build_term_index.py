"""Build an `RRTI` term index over the first N docs of the OpenAlex record store.

    python build_term_index.py <N> <out.rrt>

Dumps (doc_id, title+abstract) via the `dump_records` example, tokenizes + builds
through the `roaringrange.write_term_index` binding. doc_id order is the shared
rank order, so the `.rrt` composes with the other indexes.
"""
import json
import subprocess
import sys
import time

import roaringrange as rr

N = int(sys.argv[1]) if len(sys.argv) > 1 else 1_000_000
OUT = sys.argv[2] if len(sys.argv) > 2 else "/tmp/oa-out/openalex-1m.rrt"
DUMP = "rust/target/release/examples/dump_records"
IDX, BIN, DICT = (
    "/tmp/oa-out/records-full.idx",
    "/tmp/oa-out/records-full.bin",
    "/tmp/oa-out/openalex-full.dict",
)

t0 = time.time()
proc = subprocess.Popen([DUMP, IDX, BIN, DICT, str(N)], stdout=subprocess.PIPE, bufsize=1 << 22)
docs = []
for line in proc.stdout:
    did, _, js = line.partition(b"\t")
    try:
        rec = json.loads(js)
    except ValueError:
        continue
    text = ((rec.get("t") or "") + " " + (rec.get("ab") or "")).strip()
    docs.append((int(did), text))
print(f"[{time.time() - t0:.0f}s] dumped {len(docs):,} docs; building RRTI...", flush=True)
rr.write_term_index(OUT, docs, 65_536)
print(f"[{time.time() - t0:.0f}s] wrote {OUT}", flush=True)
