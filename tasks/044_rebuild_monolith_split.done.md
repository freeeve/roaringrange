# Task 044 — rebuild the trigram monolith + geo split from the correct records

## ❌ CLOSED — NOT A BUG (misdiagnosis, 2026-06-22)

The monolith was **never misaligned**. A full rebuild from the live records
produced **identical** result ids; `verify_monolith_aligned` passes; and
`dump_record` confirms each trigram hit's record genuinely contains all 9 "roaring
bitmap" trigrams (`match=true`). The "wrong" results are **trigram substring
false-positives**: long popular docs (3000+-trigram physics papers, shallow ranks)
contain all the short 3-grams and out-rank the real roaring-bitmap papers (short,
deep ranks 11M–36M), which ARE in the result set but buried. Term/hybrid modes
match word tokens and are correct. **No rebuild or upload needed.** The real lever
is relevance for multi-word trigram queries (phrase/proximity — task 042 — or steer
the demo to term/hybrid). The keepers from this investigation: `verify_monolith_aligned`
and `dump_record` (rust/examples). Original (mistaken) plan preserved below.

---

The live server-trigram demo returns **wrong documents** (e.g. `roaring bitmap` →
proton-proton-collision papers). Root cause: the trigram monolith
`openalex-full.rrs` (and the geo split sliced from it) was built on a **different
`records-full` ordering than the one that's live**, so its doc IDs don't map to
the right records. Rebuild it from the live records so `doc-id == records
position` lines up with everything else.

## Evidence (audited 2026-06-19/20)

- `/search` (trigram monolith) `roaring bitmap` → ids `[167928, 455638, 465585]`
  → render as physics papers. `/search-term` → ids `[36283355, …]` →
  **"Optimizing Druid with Roaring bitmaps"** (correct). Same `records-full`,
  only term is right.
- Client geo split returns the **identical** ids `[167928, 455638, 465585, …]` as
  the monolith — it was sliced from it (`slice_trigram_monolith`), so it inherits
  the same break. Switching the demo to the split does NOT fix it.
- A strict-AND trigram match for `roaring bitmap` can't be a collisions paper, so
  the monolith's doc 167928 ≠ `records-full`'s doc 167928 → different orderings.

## Which `records-full` is correct

The **live `records-full` (2026-06-03)** is canonical and correct: term, vector
(`.rrvi`), and bm25 (`.rrb`) were all built on it and resolve the right docs.
`openalex-full.rril` (06-03) is a **DOI→docs lookup, not a doc-id remap**, so
there is no cheap remap fix. Do NOT swap `records-full` — that would break the
three modes that currently work. The monolith is the lone outlier and must
conform to the live records.

S3 bucket split by doc-id ordering (order A = live/correct, B = broken):

| order A (correct, keep) | order B (broken, rebuild) |
| --- | --- |
| `records-full.idx/.bin`, `openalex-full.rril/.dict/.rrf` · 06-03 | `openalex-full.rrs` · 06-10 |
| `openalex-484m.rrvi` · 06-05 · `openalex-484m-stem.rrt` · 06-07 · `…-stem.rrb` · 06-11 | `openalex-trigram-geo/*` · 06-13 |

No corrected file is hiding: bucket is unversioned, single version of each, no
alt/staging key. Order-B records was never uploaded (or was a divergent local
copy on the build box).

## Prereq — records are NOT local

`/tmp/oa-out` is gone; nothing matches on disk. Pull the **live** records from S3
(do NOT reuse any local copy — a stale/divergent local records is the likely
cause of the original divergence):

```
aws s3 cp s3://openalex-eve/records-full.idx .
aws s3 cp s3://openalex-eve/records-full.bin .      # 115 GiB
aws s3 cp s3://openalex-eve/openalex-full.dict .
```

Run on an **in-region build box** (the original was EC2; ~300 GiB disk for
records + monolith + work, RAM sized to `chunk_docs` — a c7g 32 GiB OOM'd at
4M-doc chunks historically). Local Mac has ~1 TiB free but in-region S3 I/O is
far better. `AWS_PROFILE=openalex-admin` for all S3.

## Recipe

1. **Build the monolith** from the live records (`N = 484369476` total docs;
   features must match the original libzstd record decode — see
   [[v3-monolith-build-libzstd]]):
   ```
   cargo run --release --features "zstd" --example build_trigram_monolith -- \
     records-full.idx records-full.bin openalex-full.dict 484369476 \
     openalex-full.rrs [chunk_docs sized to box RAM]
   ```
2. **Verify alignment BEFORE upload** — the mandatory gate that prevents re-breaking
   prod. Run `verify_monolith_aligned` against the **same live records** (samples
   docs across the corpus and confirms each is listed under its own record's rarest
   trigrams; a monolith built from a different records ordering fails here):
   ```
   cargo run --release --features zstd --example verify_monolith_aligned -- \
     openalex-full.rrs records.idx records.bin openalex-full.dict 300
   ```
   Must print `OK: monolith is doc-id-aligned with these records.` (exit 0). A
   non-zero exit / `MISALIGNED` means the build used the wrong records — do NOT
   upload. (Spot-check too: `query_rrs openalex-full.rrs "roaring bitmap"` should
   return the roaring-bitmap papers, cross-checked against the term Lambda's ids.)
3. **Re-slice the geo split** from the verified monolith:
   ```
   cargo run --release --features "splits" --example slice_trigram_monolith -- \
     openalex-full.rrs out-geo openalex 2000000 32000000 484369476
   ```
   Decide on `openalex-global.bloom`: the split config references it but it is
   currently **404** on the live path (`/openalex-global.bloom`) — either generate
   it during the slice or drop the `bloom:` reference in `index.html` DATASETS.
4. **Upload** (overwrite live keys):
   ```
   aws s3 cp openalex-full.rrs s3://openalex-eve/openalex-full.rrs \
     --cache-control "public, max-age=31536000, immutable"
   aws s3 cp out-geo/ s3://openalex-eve/openalex-trigram-geo/ --recursive \
     --cache-control "public, max-age=31536000, immutable"
   ```
   `.rrf`/`.rril`/`records-full`/`.dict` are order A and stay valid — reused, not
   rebuilt.
5. **Invalidate** CloudFront (`E3H4W2Y0UYDT7E`): `/openalex-full.rrs`,
   `/openalex-trigram-geo/*`, `/index.html`.
6. **Verify live**: server `/search` and the client split for `roaring bitmap` →
   "Optimizing Druid with Roaring bitmaps" etc.; spot-check a few more queries
   against term mode (which is the correct reference).

## Acceptance

- `curl -G https://openalex.evefreeman.com/search --data-urlencode "q=roaring bitmap"`
  returns ids that render as roaring-bitmap papers (match term mode's docs).
- Client split trigram (servermode off) renders the same correct papers.
- No regression in term / semantic / hybrid (they were never broken).

## Side cleanup

Abort the stale 2026-06-09 `openalex-trigram-split/openalex-s003xx.rrs` **incomplete
multipart uploads** (old 389-split set, abandoned) — they cost storage:
`aws s3api list-multipart-uploads --bucket openalex-eve` then `abort-multipart-upload`.

## Prevention

Always build the monolith from the **S3-live** `records-full` (not a local copy),
and run step 2's alignment check before any upload. The `doc-id == records
position` invariant only holds if the build's records == the live records.
