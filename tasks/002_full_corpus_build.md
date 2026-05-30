# 002 — Full ~250M-work corpus build

Status: **deferred** — hold until the 47.8M demo upgrade (upload + flip) is live.

## Destination — MUST be separate
The full build must NOT clobber the 47.8M demo. Use distinct outputs:
- local: `/tmp/oafull.*` (not `/tmp/oarust.*`)
- S3 keys: e.g. `openalex-250m.rrs`/`.rrf`, `records-250m.bin`/`.idx`
  (the 47.8M demo lives at `openalex-47m.*` / `records-47m.*`)
- a separate bucket/prefix is fine if preferred.

## How (machinery is ready)
S3 streaming (`-in s3://openalex/data/works/`) + chunked build (`-chunks K`) are both
done and proven (chunked output is byte-identical to single-pass).
- **Preferred — cloud (us-east-1):** stream from S3 in-region (GB/s, free egress) on a
  big-RAM box → likely skip chunking (2 reads) → ~1–2 h.
- **Local fallback:** download the 595 GiB once (892 GiB free), then chunked build from
  disk (`-chunks ~5`) → ~overnight. (Streaming locally re-reads ~3.6 TB across K passes —
  avoid; download once instead.)
- **Memory:** the 47.8M build peaked 51.6 GiB; chunks don't hold records, so ~50–62M
  docs/chunk is safe → `K ≈ 4–5` for 250M.

## Expected scale (extrapolated from the 47.8M build)
`.rrs` ~40–50 GiB (sublinear — trigram vocabulary saturates ~30M trigrams), records
~50 GiB, facets ~1 GiB. Index chunking only needed if built in < ~80 GiB RAM.
