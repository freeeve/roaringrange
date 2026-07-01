# 070: chore: quality cleanups roll-up (docs, error taxonomy, API consistency, writer truncations)

**Severity: LOW individually; one sweep commit (or a few small ones).** Line refs @ 849f9c2.

## Stale/misleading docs + comments

1. `roaringrange.go:29-32` -- `ErrCompressedRecord` doc + message claim the Go reader has no zstd support; `records_zstd.go` `OpenRecordStoreWithDict` has supported it since the klauspost port (task 049). Reword: "store opened without a dictionary -- use OpenRecordStoreWithDict".
2. `transcode.go:66-67` -- comment references nonexistent `parseSplitEntries`; the function is `parseEntries`.
3. `build.rs:74-85` -- the `/// Writes the v3 RRS index...` doc block is attached to `fn u32_len`, leaving `write_index` (:103) undocumented and u32_len misdocumented.
4. `wasm.rs:393-401` -- the doc describing `filtered_ids` is fused onto `FACET_COUNTS_TOP_PER_FIELD`; `filtered_ids` itself undocumented.
5. `hotcache.rs:150-157` -- doc claims "one GET" / module doc "one ranged read of the whole .rrhc", but `open` issues two reads (header then body). Fix the doc (or the code -- speculative single read of header+body is nicer for a format whose purpose is RTT-counting; decide there).

## Error taxonomy + small correctness

6. `secondary.rs:54` -- missing `"primary"` column reported as `BadQuery`; by the crate's taxonomy (`index.rs:59-69`) a mismatched artifact is `Malformed`.
7. `index.rs:1093, 1107` -- `self.pos + n` / `offset + limit` unchecked usize adds on JS-supplied page args (debug panic, release wrap -> short page). `saturating_add`.
8. `monolithbuild.go:43` -- `WriteIndex` promises "caller need not pre-sort" but uses unstable `sort.Slice` and never checks duplicate keys: dup keys -> nondeterministic byte order (breaks the byte-for-byte guarantee) + a dict binary-search resolving to one arbitrary entry. Add an adjacent-duplicate check after sorting (error), and use `sort.SliceStable` or sort by (key) only since dup keys now error.

## Silent writer-side length truncations (inconsistent with siblings that DO check)

9. `terms.go:406-407` -- tail size written `uint32(len(tail))` unchecked while head size is validated (:400). `sortcols.go:110` (`uint16(len(c.Name))`), `sortcols.go:119` (`uint16(len(cols))`), `hotcache.go:74` (`uint32(len(stringBlob))`) -- same. Rust twins: `build.rs:233-237` (facet name `as u16`), `build.rs:696-697` (sortcols blob `as u32`). Siblings that already check: `splitset.go:110-120`, `splitset_build.rs:392-403` (`push_name`), `hotcache_build.rs:69-76`. Add range checks + errors (writer-side; correct inputs byte-identical).

## API consistency (wasm)

10. `wasm.rs:1428` (`RrsSecondaryIndex::search_cursor_filtered`) and `wasm.rs:2114` (`RrssIndex::search_filtered`) parse filters with legacy `filter_pairs` (`[field,category]` only) while the primary `RrsIndex` accepts `filter_sels` (objects + exclude flags). The same JS filter shape works on primary and throws "facet filter category must be a string" on secondary/split. Unify on `filter_sels`. (Cosmetic: `search_cursor(query: &str)` vs `search_cursor_filtered(query: String)`.)

## Shadowed builtins (style)

11. `splitsetbuild.go` / `termsplitsetbuild.go` -- `cap := capFor(...)` in both `Finish`es and `newDictBlockWriter(cap int)` shadow builtin `cap`.

## Acceptance

- Goldens/conformance byte-identical (only invalid inputs newly error).
- `gofmt -s`, sonar clean, docs render correctly (`cargo doc` spot-check items 3-4).
