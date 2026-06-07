# Task 016 — RRS v3: collapse head/tail into one uniformly-paged posting

**Status:** in progress. Spun from [014](014_rrs_tail_paging_followups.md) **Part 1**.

Collapse the RRS trigram index's separate head/tail posting storage into **one bitmap per term**,
paged uniformly by container bucket. The head was a build-time `[0, head_boundary)` prefix stored
as its own blob, directly addressed for the instant first page — redundant now that `TailScan`
(`posting.rs`) pages any container bucket. v3 drops the dual storage, `head_boundary`, and the
build-time split; the dict entry shrinks 24 → 20 B.

**Decisions (locked):** v3-only reader (no v2 compat — the live index + split sets rebuild to v3
and deploy with the new reader); Rust now, **Go port a follow-up** (`go/splitsetbuild.go` stays v2
this pass — see Follow-ups).

## Measured savings (why it's a simplification, not a space play)
Per term: 8 B posting framing (one duplicate `cookie+size` removed — empirically exact for every
posting shape) + 4 B dict (24→20). Live `openalex-full`: ngrams = 114,588,535 → **≈1.28 GiB / 1.375 GB
≈ 1.20%** of the 114.5 GB file (dict −0.46 GB, framing −0.92 GB). The dict shrinks 17%
(2.75→2.29 GB), which also trims the per-query dict-block read (512×24=12 KB → 10 KB). The win is
**structural** (one paging mechanism, no `HEAD_BOUNDARY`, no build split, `search`/`search_candidates`
collapse onto the cursor), not space.

## Format (RRSI v3)
- **Header 20 → 16 B:** `magic[4] + version[2]=3 + gram[2] + ngrams[4] + stride[4]`. Drops `head_boundary`.
- **Dict entry 24 → 20 B:** `key u64 + offset u64 + size u32` (drops `headSize`/`tailSize`).
- **Postings:** one portable RoaringBitmap per term at `[offset, size)`.
- The deployed v2 `head_boundary` was **1,048,576 = 16 buckets** (not 2¹⁶). v3's eager prefix uses a
  reader constant `EAGER_BUCKETS = 16`, so the instant-first-page candidate set + facet counts match v2.

## Change list / progress — Rust DONE 2026-06-07
- [x] `build.rs`: `write_index(gram, stride, entries: Vec<(u64, Vec<u8>)>)` — 16 B header, 20 B dict,
      single posting; new `serialize_posting`; `split_posting` KEPT (still used by `RRSF`/`RRTI`).
      `FORMAT_VERSION = 3`, `HEADER_SIZE = 16`. Also v3'd the chunked streaming writer
      `chunk::merge_partials_to_rrs`.
- [x] `index.rs`: `DictRec{offset,size}`; `parse_block` 20 B; `DICT_ENTRY=20`; header 16 B, accept v3
      only; `posting(key)` replaces `head()`/`tail()`; `fetch_head_prefixes`; `Cursor` eager prefix;
      `ensure` ranges = whole posting, tail paged from `EAGER_BUCKETS`; whole-tail fallback excludes
      `[0, EAGER_DOC_BOUND)`; `search`/`search_candidates` use the eager prefix + whole-posting
      fallback. `rrs_boot_len` accepts v3. Dropped `head_boundary` field + `head_boundary()`.
- [x] **`EAGER_BUCKETS = 1`** (not 16): the eager prefix must equal the `RRSF` facet **head**
      boundary (1 bucket = 64 K), or a facet-tail doc in buckets the eager set covers but the facet
      head doesn't gets dropped from `head_result` yet skipped by the tail scan. (The deployed v2
      `head_boundary` of 1,048,576 with a 64 K facet head was a latent inconsistency v3 removes.)
      This also matches the §014 "head = bucket 0" premise.
- [x] `posting.rs`: `TailScan::open(min_key)` skips buckets `< min_key`; `fetch_head_prefix`.
- [x] callers: `splitset_build.rs`, `splitset_write.rs` (flush+compact+`read_rrs_entries`),
      `secondary.rs`, `catalog.rs`, `build_tests.rs` (hand-rolled v3 spec builder), `index.rs`/`build.rs`
      tests, `examples/{secondary,density}.rs`. `SplitSetWriter.head_boundary` field removed.
- [x] `FORMAT.md` rewritten to v3.
- [x] golden regenerated from Rust v3 (`regen_shared_golden`).
- [x] **Verified:** 112 lib tests + 58 default + integration green; clippy clean (all features +
      wasm32); the real wasm reader opens regenerated **v3** splitset-demo data over HTTP Range
      (openBundle + search + facetCounts correct). splitset-demo data + both demo readers rebuilt to v3.

## Follow-ups (out of this pass)
- [x] **openalex BUILDER → v3** (DONE 2026-06-07): `main.rs` `build_index` + `phased.rs`
  `merge_partials_to_rrs` call + `secondary.rs` `remap_text_index` (v3 read+write) and its test all
  emit/read v3; facet (`RRSF`) builders keep head/tail. Builds all-targets clean; 8 builder tests pass
  (incl. the secondary remap round-trip → v3 reader). `headtune.rs` still reads the v2 head/tail layout
  and is now **obsolete** (it tuned the removed `head_boundary`) — left as-is, not on the build path.
- **Go port v3** (separate task): `go/splitsetbuild.go` → v3, regenerate the golden, re-assert Rust↔Go.
  Until then the committed `go/testdata/rrss_build_golden.txt` is **v3 (Rust) vs v2 (Go)** — the Go
  conformance test is expected to FAIL by design.
- **Cheap eager prefix:** v3-v1 derives bucket 0 via the full container directory (~60 KB for very
  common terms). Optimize to the partial read (§014: header → size → first offset entries → byte range).
- **RRSF facet sidecar** keeps its own `head/tail` (64 K) — same collapse is a later option.
- **Re-deploy:** v3-only reader rejects the live v2 index — the deployed `openalex-full.rrs` + split
  sets must be rebuilt to v3 (needs the builder follow-up) and deployed with the new reader.
