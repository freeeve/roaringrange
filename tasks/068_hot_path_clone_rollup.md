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
