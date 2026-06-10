# 024 — Container-granular facet membership probes (kill the 90 MB filter case)

**Status:** pending

When facets filter a **ranked-list** result (term / semantic / hybrid / split modes),
`ResolvedFilter::full_bitmap` fetches each selected category's complete posting — the
year=2020 case is ~90 MB — to answer a membership question about **≤ 600 candidate doc IDs**.

## Design

Roaring's NO_RUNCONTAINER layout has an offset table (`posting.rs::parse_dir` already decodes
it) mapping each 64K-doc container to its byte range. The candidates span only a handful of
containers, so:

1. `rust/src/index.rs` (or `posting.rs`): a `ResolvedFilter::contains_ids(ids) -> RoaringBitmap`
   (or `filter_ids`) path that, per selected category posting:
   - fetches the posting's header (8 + 8·size bytes — the same `needed_header_len` read
     `TailScan` does),
   - computes which containers the candidate ids touch,
   - fetches only those containers' byte ranges,
   - tests membership per candidate.
   Within-field OR / across-field AND combine per candidate, not per full bitmap.
2. Fall back to `full_bitmap` when the posting is small (header read not worth it — e.g.
   posting < 256 KB) or when the container fan-out approaches the whole posting.
3. `wasm.rs filterIds` switches to the new path; the demo needs no change.

## Expected effect

year=2020 over 600 vector candidates: ~90 MB → ~(header ~KBs) + (≤ #distinct buckets ×
~8 KB containers) — typically a few hundred KB. Facet **counts** over the full corpus still
use resident heads (unchanged); this only changes filtering a candidate list.

## Acceptance

- Byte-meter comparison in the demo (perf bar) before/after on year=2020 + semantic search.
- Conformance: filtered results identical to the `full_bitmap` path (unit test comparing
  both paths over synthetic postings spanning many containers, incl. run/array/bitmap
  container types — note run containers (RUNCONTAINER cookie) lack the offset table → keep
  the full fetch fallback for those postings).
