# 068: perf(rust): hot-path clone/copy roll-up

**Severity: LOW individually; worth one sweep.** CPU/alloc wins, no fetch-count changes, no behavior changes. Line refs @ 849f9c2.

## Findings

1. `index.rs:340` (`read_dict_blocks`) -- `fetched[i].clone()` per n-gram: every n-gram sharing a block gets a full copy of the stride-sized block (stride 512 -> ~10 KB x n-grams, per keystroke via query_cost). Return `(unique_blocks, which_index_per_key)` or slice by index.
2. `index.rs:1280-1291` (fuzzy `threshold` cascade) -- `c[k-1].clone()` per posting x level: ~n*t clones of growing accumulator bitmaps on the whole-tail fuzzy path (a 30-trigram `max_missing=2` query does ~800 bitmap clones). Restructure to move/`|=` in place where possible.
3. `records.rs:135` (v2 decode, TAG_RAW) -- `payload.to_vec()` copies every record just to strip one tag byte; a 25-record page = 25 full-record copies. In-place `rotate_left(1)` + `truncate(len-1)` on the owned Vec, or return an offset.
4. `terms.rs:567, 587` -- covered in task 063 (listed there; skip here if 063 lands first).
5. Go twin, fold in or do alongside: `transcode.go:110` -- full defensive copy of every posting (`bm.FromBuffer(append([]byte(nil), payload...))`) across a 100+ GB transcode; the bitmap is reserialized and discarded in-function while `data` stays alive unmutated, so the copy looks avoidable -- VERIFY roaring v2's `FromBuffer` aliasing contract before removing. Also `vector.go:92-99` (`WriteRRVI`) allocates a per-cluster id buffer inside the nlist loop -- reuse one buffer sized to the largest list.

## Acceptance

- Identical outputs/results everywhere (goldens + differential tests).
- Alloc deltas noted in the commit message (cargo bench or a quick heaptrack/pprof number is enough).

## Outcome (DONE)

No format/output-byte changes; Rust/Go conformance (goldens) unchanged. All wins are alloc/CPU only.

- **Item 1 (`index.rs` `read_dict_blocks`)** -- returns `(unique_block_bytes, which)` instead of a per-key `Vec<Vec<u8>>`; keys sharing a block now index the same buffer. Removes one ~stride-sized block copy per n-gram that shares a block (stride 512 => ~10 KB x n-grams, on the per-keystroke `query_cost`/`search` path). Four callers (`lookup_many`, `query_cost`/`count_estimate` via it, `search`, `search_candidates`, fuzzy cursor) updated to index by `which[i]`.
- **Item 2 (`index.rs` fuzzy `threshold` cascade)** -- `let mut inc = c[k-1].clone(); inc &= b` became `let inc = &c[k-1] & b`, allocating only the (smaller) intersection rather than cloning the full growing accumulator then shrinking it. A 30-trigram `max_missing=2` query did ~800 full-bitmap clones; now each step allocates just its intersection. Result identical (c[k-1] is read before its own step mutates it).
- **Item 3 (`records.rs` `decode`)** -- TAG_RAW now strips the tag byte in place (`raw.remove(0)`) reusing the owned buffer, instead of `payload.to_vec()` allocating a second full-record buffer per record of a page.
- **Item 4** -- already done in task 063 (`terms.rs` `and_locs`); skipped here.
- **Item 5 (Go)** -- `transcode.go` `serializePosting` drops the `append([]byte(nil), payload...)` defensive copy: verified against roaring/v2 v2.14.4's `FromBuffer` contract ("bitmap references buf; broken only if buf becomes unavailable") -- here `bm` is read-only (`ToBytes`) and discarded while `payload` (a slice of the live index bytes) outlives it and is never mutated, so the copy is unnecessary. Saves a posting-sized alloc per posting across a 100+ GB transcode (a comment records the invariant). `vector.go` `WriteRRVI` reuses one id buffer grown to the largest cluster instead of allocating one per cluster in the nlist loop (`dst.Write` copies, so reuse is safe).

### Verification

- Rust: `cargo test --lib` matrix green -- default (94), splits+terms+vector+hotcache (203), zstd (95). `cargo fmt --check` clean; `cargo clippy --all-targets --features "splits terms"` (CI gate) clean; lib+tests clippy clean under all features incl. zstd. (Two pre-existing `is_multiple_of`/complex-type lints exist in `zstd`-gated examples I did not touch; CI does not run clippy with `zstd`, so they do not gate.)
- Go: `gofmt -s` clean, `go vet` clean, root + conformance modules green; vector/transcode/record goldens unchanged.

### Alloc deltas (qualitative -- no bench harness wired)

Per operation, allocations removed: dict-block dedup drops one stride-sized copy per block-sharing n-gram (per keystroke); fuzzy threshold replaces ~n*t full-accumulator clones with intersection-sized results; record decode drops one full-record copy per record of a page; transcode drops one posting-sized copy per posting; WriteRRVI drops (nlist - 1) id-buffer allocations per index write.
