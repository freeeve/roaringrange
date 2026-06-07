# Task 014 — RRS tail-paging follow-ups: head/tail collapse (v3) + facet-filter scan cost

**Status:** pending

Two follow-ups left open after **incremental ranged tail pagination** shipped
(crate: `059c700`, `2fdb8cf`; demo: `3f0902e`, `84f9cec`). Both concern the RRS
trigram reader's tail paging (`rust/src/posting.rs` `TailScan`, driven by
`rust/src/index.rs` `Cursor::ensure`). Neither is urgent; capture so they aren't lost.

Background now in place: each posting is a NO_RUNCONTAINER roaring bitmap with a
container **offset table**, containers keyed by `doc_id >> 16` (a 64K-doc "bucket").
`TailScan` reads each posting's directory once, then intersects candidate buckets in
ascending (citation-rank) order a window at a time, so a page fetches only the buckets
it spans. It is facet-aware (per-bucket within-field OR / across-field AND, facet
postings read via the filter's own `.rrf` fetcher).

---

## Part 1 — Collapse head/tail into a uniformly-paged single bitmap (RRS v3)

**Observation:** `HEAD_BOUNDARY = 65536 = 2¹⁶`, and containers are keyed by `doc >> 16`,
so the "head" is **exactly container bucket 0**. Head/tail is therefore just "bucket 0
vs. every other bucket." Given seekable containers (which `TailScan` now uses), the
separate head/tail storage is largely redundant — it predates container-level ranged
reads, when a separately-addressed head blob was the only way to fetch a cheap
top-results prefix without reading the whole posting. `TailScan` generalizes that to any
bucket, so the head is now a special case of the general mechanism.

**v3 shape:** one bitmap per term; page uniformly by bucket. "The head" becomes
"eagerly fetch bucket 0 (+ compute in-memory facet counts) for the instant first page,"
everything else paged on demand via the same scan. Drops `HEAD_BOUNDARY`, the dual
head/tail blobs, and `build::split_posting`'s build-time split.

**What the current split still buys (must be preserved):**
- The head is directly addressed by `head_offset/head_size` — the top tier is fetched in
  **one wave for all query trigrams** at cursor construction, without parsing a container
  directory (which is ~60 KB for very common terms). Instant most-cited results + facet
  counts. A unified design must re-derive bucket 0 cheaply: read header → `size` → the
  first two offset-table entries → bucket 0's byte range (a couple of small reads, not the
  whole directory).

**Cost / why it's a deliberate v3, not a reactive change:**
- On-disk **format change** → rebuild every index (incl. the ~114 GB `openalex-full.rrs`).
- Re-verify any **byte-exact ports / conformance** (the Go build side) and bump the format
  version in `FORMAT.md`; reader either supports v2 + v3 or there's a migration.
- The dict entry shrinks (`key + offset + size`, dropping the separate `head_size`/`tail_size`
  split) — touches `index.rs` `DictRec`/`parse_block` and `build.rs` `write_index`.

**Acceptance:** a v3 reader pages a term's full posting by bucket with bucket 0 fetched
eagerly (matching today's head latency + facet counts), no `HEAD_BOUNDARY`, results
identical to v2 for the same corpus, format documented and (if ported) byte-conformant.

---

## Part 2 — Facet-filtered tail scans still read tens of MB

**Symptom:** filtering a query to e.g. `year=2020` downloaded ~90 MB of tail to fill one
page. Largely **expected** for this index design, but worth optimizing.

**Why:** doc IDs are citation rank; a year facet is orthogonal to that, so `year=2020`
matches are scattered across the whole rank space and usually gut the cheap head (top-cited
papers skew older). Filling a 25-result page then pages deep into the tail, scanning buckets
in rank order until 25 also-2020 hits appear. **Each scanned bucket must read every query
trigram's container (~8 KB for common terms) to know which docs match there**, then AND with
the facet — so a *sparse* filtered page touches many buckets and the trigram-container reads
dominate. The facet postings themselves are small per bucket. Confirm in the perf panel via
the "Tail fetch" req count (≈ buckets × postings).

Note also: `intersect_key_window` (`posting.rs`) reads **all** postings' window containers
concurrently — it does **not** apply the smallest-first seed-and-shrink that the whole-tail
`tail_intersect_and` does. So the incremental path is less byte-efficient than the
whole-tail path for queries with a *selective* trigram.

**Levers (by impact):**
1. **Field-ordered index for the filtered field — the real fix.** The repo already builds a
   **year-descending secondary index** (`rust/src/secondary.rs`, `RrsSecondaryCursor`), where
   same-year docs are contiguous. Filtering/sorting by year there is a *range*, not a scattered
   AND, so `2020` is cheap. Wire the demo's year facet (and/or a "sort by newest" mode) to it.
   Bigger change; the right investment if date filtering/sorting is a demo headline.
2. **Per-page tail-scan byte budget.** Cap the page-fill scan (e.g. ~8–16 MB), showing what was
   found ("18 of this page · more →") rather than grinding to a full 25. Needs a budget-bounded
   `ensure` (leave `pending_tail` true when the budget trips) + a UI that tolerates variable /
   short pages — a real UX tradeoff against the fixed 25/page pager.
3. **Smallest-first seed-and-shrink inside the window.** Port the `tail_intersect_and` shrink to
   `intersect_key_window`: read the smallest posting's window first, then read others only at
   surviving container keys. Helps queries with a selective trigram (skip common-container reads
   where the rare one is empty); little help for `year + common term` (both dense per bucket).

**Acceptance:** a representative facet-filtered query (e.g. a common term + `year=2020`) fills
its first page in single-digit MB (or is explicitly bounded), without regressing unfiltered
paging or the facet-AND correctness (`posting::tests::tail_scan_with_facets_matches_filtered_and`
and the existing facet tests stay green).
