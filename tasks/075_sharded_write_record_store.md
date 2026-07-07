# 075 -- Sharded-write record store (base+delta RRSR)

Filed by the libcatalog session (2026-07-07) as a cross-repo request. Leave this
file uncommitted; roaringrange owns/adopts it. Another session already has
uncommitted work in this tree (`format.go`, `lookup_read.go`, ...) -- do not
touch it.

## Motivation (the consuming need)

libcatalog is building a serverless (Lambda) writable catalog backend. Its work
index is a **projection** (identity + summaries + barcodes per work-grain) that
must:

1. cold-load fast (one range-fetched artifact, not a corpus scan),
2. absorb writes incrementally (a publish changes a few records; do not rewrite
   the whole store), with cross-container read-your-writes,
3. optionally serve records lazily at Level B (answer a detail/page read by range
   without full residency), into the millions.

Today `splitset` (RRSS) gives base+delta+prune -- but only over **RRS/RRTI
search bodies**. `RRSR` (the record store) has only a whole-store streaming
writer (ascending doc-id), so there is no sharded/incremental **record** write.
libcatalog is hand-rolling it (a JSON snapshot as base + an append log as delta +
an in-memory fold as compaction; see libcatalog tasks/155,156). A first-class
sharded-write record store would replace that hand-rolled machinery with the
tested, byte-pinned, range-served, optionally-compressed family primitive -- and
serve the public catalog's record-details path too.

## What would suit perfectly

A record split set: the RRSR analog of RRSS-over-RRS. Likely cleanest as
**extending the RRSS manifest with a record `bodyKind`** so the existing
prune/base-delta/supersession/compaction machinery applies to record shards --
but the shape is roaringrange's call.

Requirements:

- **Sharded record store.** Many immutable shards, each a vanilla `RRSR`, named by
  a manifest. One shard = today's monolith.
- **Base + delta + compaction.** Add/update/delete records into a small delta
  shard without rewriting the base; the reader merges delta over base; compaction
  folds deltas into the base later (RRSS's base/delta/epoch supersession).
- **Stable-key sharding** (a `PolicyStableKey` analog for records): a record is
  addressed by a stable application key (string id or hash), which maps to a
  stable shard, so changing one record rewrites only its shard -- no dense-doc-id
  reorder that dirties every shard. Point `Get(key)` resolves across base+delta.
- **Tombstones** for deletes (the `SplitFlagHasTombstone` analog), dropped at
  compaction.
- **Reads both ways.** Bulk iterate (full cold-load) and point/range `Get`/
  `GetMany` over the merged (base+delta) set, HTTP-Range-friendly.
- **Opaque payloads + optional zstd shared dict** (keep RRSR's version-2 framing):
  the app picks the record encoding (libcatalog uses JSON projection entries);
  self-similar small records compress well with a trained dict.
- **Byte-for-byte Go build <-> Rust/WASM read parity**, like the rest of the
  family, with golden tests.

Additive: the existing single-file `RRSR` stays valid and unchanged.

## libcatalog adoption

libcatalog ships the hand-rolled JSON snapshot+feed now (its snapshot is a
disposable, rebuildable cache, so swapping the container later is a Save/Load
change, not a data migration). When this lands and is published, libcatalog
tasks/155-156 migrate the admin projection onto it and tasks/158-159 use it for
the public record-details path. Announce the version to bump to; do not re-add a
local `replace`.
