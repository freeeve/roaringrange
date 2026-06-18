"""Build a full-corpus `RRTI` term index by **streaming** through the `TermBuilder`, so
the corpus text never lives in memory all at once — only the accumulated postings do.

    python build_term_index_stream.py <N> <out.rrt> [language]

`language` is "english" (Snowball stemming) or omitted (unstemmed). doc_id order is the
shared rank order, so the `.rrt` composes with the other indexes.
"""
import json
import subprocess
import sys
import time

import roaringrange as rr

N = int(sys.argv[1])
OUT = sys.argv[2]
LANGUAGE = sys.argv[3] if len(sys.argv) > 3 else None
HEAD_BOUNDARY = 65_536
CHUNK = 50_000
DUMP = "rust/target/release/examples/dump_records"
IDX, BIN, DICT = (
    "/tmp/oa-out/records-full.idx",
    "/tmp/oa-out/records-full.bin",
    "/tmp/oa-out/openalex-full.dict",
)

t0 = time.time()


def log(msg):
    print(f"[{time.time() - t0:7.0f}s] {msg}", flush=True)


builder = rr.TermBuilder(head_boundary=HEAD_BOUNDARY, language=LANGUAGE, stopwords=False)
log(f"streaming {N:,} docs into TermBuilder (language={LANGUAGE!r})...")
proc = subprocess.Popen([DUMP, IDX, BIN, DICT, str(N)], stdout=subprocess.PIPE, bufsize=1 << 22)
chunk = []
done = 0
for line in proc.stdout:
    did, _, js = line.partition(b"\t")
    try:
        rec = json.loads(js)
    except ValueError:
        continue
    text = ((rec.get("t") or "") + " " + (rec.get("ab") or "")).strip()
    chunk.append((int(did), text))
    if len(chunk) >= CHUNK:
        builder.add_many(chunk)
        done += len(chunk)
        chunk = []
        if done % 5_000_000 < CHUNK:
            log(f"added {done:,} docs, {builder.term_count():,} distinct terms")
if chunk:
    builder.add_many(chunk)
    done += len(chunk)
log(f"streamed {done:,} docs, {builder.term_count():,} distinct terms; writing {OUT}...")
builder.finish(OUT)
log(f"done: wrote {OUT}")
