# Task 009 — Range-fetchable RRTI term dictionary (blocked-FST router)

**Status:** implemented (2026-06-06) — Rust core + wasm + Python; Go deferred (tasks 010/011).

Revise the `RRTI` term index so its **dictionary is range-fetched like everything else in
roaringrange**, instead of loaded whole into memory. Task 005 built `RRTI` on a monolithic
**FST** dictionary (`terms.rs`); that works for the 1M-doc head but does **not** scale: the
reader loads the *entire* FST into RAM on `open()`, so a full-corpus (484M-doc, ~10–100M+
term) index would be a multi-GB resident blob the browser can't load.

## Outcome (what was built — diverges from the original scoping below)

The dictionary became a **blocked, front-coded sorted-string table with a small resident FST
routing over block boundaries** — the **Quickwit/`tantivy-sstable` shape**, *not* the
sorted-`u64`-array sparse index of `RRS`. Decisions (confirmed with the user mid-implementation):

- **Router = blocked FST**, keyed on each block's *last* term → `(blockOff<<24)|blockLen`;
  located with `range().ge(term)`. Resident size is O(#blocks); a new `resident_len()` reports
  it. Keeps the `fst` crate (used correctly, as a small router — not the whole vocabulary).
- **v1 dropped entirely.** The monolithic-FST reader is gone (it was unused); the builder emits
  v2 only, `open()` rejects other versions. Existing v1 `.rrt` files (the demo's
  `openalex-1m-stem.rrt`) must be **rebuilt** as v2; the demo also needs a **wasm rebuild**.
- **Fuzzy → trigram `RRS`.** `search_fuzzy` removed (and the `fst` `levenshtein` feature
  dropped); exact + prefix + autocomplete remain, all natural on the sorted blocked dict.
- **Block layout:** byte-capped (default 4 KB) + front-coded; linear in-block scan. Pure codec
  in `terms_dict.rs` (no I/O / `fst` / async), shared by builder and reader, Go-port-ready.
- **`complete()` is now async** (v2 range-fetches the prefix's blocks); wasm `complete` too.
- **Go conformance consequence:** because the router is an FST, byte-for-byte Go conformance
  still needs the HARD `fst` Go port (task 010) — it is **not** obviated (a sorted-array router
  would have obviated it; rejected in favour of Quickwit fidelity).

Files: `terms_dict.rs` (new codec), `terms_build.rs` (v2 writer), `terms.rs` (v2 reader),
`wasm.rs`/`python` bindings, `splitset_build.rs` (size estimate), `TERMS.md`, `README.md`.
Format spec: see the rewritten `TERMS.md` (RRTI v2). Tests: codec round-trips, multi-block
router/scan, resident-footprint scaling, plus the carried-over query tests on v2.

---

_Original scoping (superseded where it conflicts with the Outcome above):_ this task replaces
the FST with a **blocked, sparse-indexed sorted dictionary** mirroring the trigram `RRS`.

005 is partly done (FST format/reader/builder, stemming, streaming, Python+wasm bindings).
This task changes **only the dictionary representation**; tokenizer, stemming, filtering,
and the postings region carry over unchanged.

## The inconsistency this fixes

The trigram `RRS` (`index.rs`) keeps only a tiny resident **sparse index** —
`sparse_keys[i] == dict[i*stride].key`, one key per `stride` entries — and **range-fetches
the one dictionary block** a lookup needs (the block's byte range is computed from the
resident sparse keys with no fetch). The full dictionary never enters RAM. `RRIL`
(`lookup.rs`) does the same with interpolation search.

`RRTI` breaks this: an FST is a monolithic automaton whose traversal does scattered random
access across the whole byte array, so it **can't** be range-fetched in blocks (a single
lookup would need thousands of tiny GETs) — it must be resident. The FST bought cheap
prefix + fuzzy automata at the cost of "whole dictionary in RAM," which is off-pattern and
doesn't scale. Filtering (task 005 follow-up) shrinks the vocabulary but does **not** fix
this — even a filtered FST is resident. The dictionary representation is the real blocker.

## Design — a blocked sorted dictionary (model on `index.rs`)

- **Dictionary blocks**: terms sorted lexicographically, partitioned into blocks sized for
  one cheap GET (target ~4–16 KB, ~256–1024 terms/block). Each entry is
  `(term, posting-location)` where the posting-location is the existing FST output value
  (`(head_off << 24) | head_size`). Within a block, **front-code** the sorted terms
  (store shared-prefix length + suffix) to compress — scholarly vocab shares long prefixes.
- **Resident sparse/boundary index**: the **first term of each block** + the block's byte
  offset (the string analogue of `sparse_keys`, since term keys are variable-length, not
  `u64`). Resident size is O(#blocks), not O(#terms) — e.g. 20M terms / 512 per block ≈ 40K
  boundary terms (a few MB) vs a multi-GB FST.
- **Postings region: UNCHANGED** — keep the RRS-identical `[head][tail]` roaring blocks and
  the existing two-wave fetch (`head_block`/`tail`). Only the dictionary changes.

## Lookup paths

- **Exact term** ✅ — binary-search the resident boundary terms (string compare) → the one
  block that could contain it → range-fetch that block → in-block search (front-coded scan
  or secondary offsets) → posting location. One ranged read for the block, then the posting
  reads exactly as today.
- **Prefix / autocomplete** ✅ — sorted terms ⇒ a prefix's matches are a contiguous range;
  find the first block via the boundary index, range-fetch + scan forward across blocks
  until the prefix stops matching (blocks fetched on demand, not all resident).
- **Fuzzy (Levenshtein)** ✴️ — the one capability the FST gave elegantly and this loses.
  Options to decide here: (a) generate candidates from the query term's **trigrams** and
  verify edit distance against a few fetched blocks; (b) **lean on the existing trigram
  `RRS`** for fuzzy/substring (it already covers that need); (c) a small *optional* resident
  FST built over only frequent terms for fuzzy — but that partly reintroduces the resident
  problem, so likely deprioritize. Recommend (b) for v1, revisit (a).

## Format / versioning

- Bump `RRTI` to **version 2** with a header flag for the dictionary kind (0 = v1 FST,
  1 = blocked). Keep the v1 FST reader so the existing small `.rrt` files still open; build
  large corpora as v2. (Or migrate fully — decide; keeping both is cheap and low-risk.)
- Header gains: boundary-index length/count, block size/stride, front-coding flag. The
  postings-region offset/layout is unchanged.

## Relationship to other tasks

- **005** (FST `RRTI`): the base; unchanged except the dictionary path. Noise **filtering**
  (min-DF + drop numeric/overlong) stays a 005 concern — now a *size/quality* optimization,
  not the memory fix. The two compose: range-fetch makes a big vocab *loadable*; filtering
  makes the file *smaller*.
- **007** (split set): that session refactored `terms_build.rs` to a shared
  `write_term_index_from_postings`, and its split-set term builder writes `RRTI` bodies too.
  This task changes that serialization → **coordinate with 007 / build on its merged
  refactor** rather than racing it; both builders should emit the blocked dictionary.

## Steps

1. Blocked-dictionary byte layout: header v2 + boundary index + front-coded dict blocks +
   (unchanged) postings region. Spec it in `TERMS.md` like the RRS dict in `index.rs`.
2. Builder: drain the sorted `BTreeMap` into blocks (front-coded) + emit the boundary index,
   replacing the `MapBuilder` FST path in the shared `write_term_index_from_postings`.
3. Reader (`terms.rs`): boot = header + boundary index resident; `locate(term)` =
   binary-search boundaries → range-fetch block → in-block lookup. Keep the posting path.
4. Prefix/autocomplete over the blocked dictionary (contiguous range scan across blocks).
5. Fuzzy decision (recommend: route to the trigram index for v1).
6. Python + wasm bindings: API unchanged (`search`/`complete`/`search_prefix`); internal
   format swap only.
7. Go conformance: build side reproduces the blocked layout + front-coding byte-for-byte.
8. Benchmark: **resident footprint** + per-query fetches, FST-v1 vs blocked-v2, at 1M and
   full corpus — the headline being resident RAM going from O(vocab) to O(#blocks).

## Open questions

- Block size / terms-per-block, and front-coding scheme (Lucene-style shared-prefix?).
- In-block lookup: linear front-coded scan vs per-block secondary offset array.
- Fuzzy strategy (the main capability tradeoff) — trigram fallback vs trigram-candidate-gen.
- v2-alongside-v1 vs full migration.
- Boundary-index size vs block size tradeoff (mirror the RRS `stride` tuning).
