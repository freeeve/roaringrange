# Task 010 — Byte-exact Go port of the `fst` crate (BurntSushi)

**Status:** pending (scoping)

A Go package that builds (and reads) finite-state transducers **byte-identical** to the
Rust `fst` crate (BurntSushi, v0.4.x — the term-dictionary layer behind `terms.rs`/RRTI).
Goal: the Go build side can emit an FST dictionary blob the Rust/wasm reader consumes
unchanged — extending the cross-language guarantee `go/conformance/` already enforces for
n-gram keys and `RRIL` `normalize_id`.

## ⚠️ Read this first — relationship to task 009 (009 has SHIPPED)

Task **009 shipped 2026-06-06** and — contrary to the early assumption here — it **kept an
FST**. RRTI v2 is a blocked, front-coded dictionary whose **resident block index is a small
FST routing over block boundaries** (the Quickwit/`tantivy-sstable` shape, chosen over a
sorted-`u64` array for fidelity to Quickwit). So **RRTI still contains an FST**, and this
byte-exact Go FST port is now **REQUIRED** for full cross-language conformance — 009 did *not*
obviate it (a sorted-array router would have; that was the rejected alternative).

What 009 *did* make trivially Go-reproducible is the rest of the format: the front-coded dict
**blocks** + postings region are plain deterministic serialization (see `rust/src/terms_dict.rs`
and the RRTI v2 layout in `TERMS.md`). The remaining hard part is exactly the **router FST
bytes** — the BurntSushi `fst` serialization this task ports. The Go build-side path is
therefore: port the `terms_dict` block codec (easy) **+** the router FST (this task, hard).

The router FST is *small* (one key per block — O(#blocks), not O(vocab)), so the port only has
to handle map building over a few tens of thousands of keys, but it must still match the crate's
serialization byte-for-byte. The stemmer port (task 011) is needed either way; both are now
live, not conditional.

## Why byte-exact (not just functionally equivalent)

The `.rrt` FST blob is parsed by the Rust/wasm reader (`fst::Map::new` over the bytes). The
Go builder must emit the *exact* bytes that reader expects — same as `go/conformance/`
guards today for trigram keys. "Equivalent FST, different bytes" fails the reader.

## What byte-exactness requires (the hard part)

It's not enough to construct a minimal FST recognizing the same (key → u64) map — the
**serialized layout** must match the crate's, which means reproducing:

- **Construction order:** the crate builds via online (streaming) minimization (Daciuk's
  algorithm) with a register of frozen states; states are written **as they freeze**, so
  byte offsets/addresses depend on the exact freeze order. Any divergence → different
  addresses → byte diff.
- **Node encoding:** the crate's compact per-state format — the one-transition-state
  encoding, the **common-inputs** table for frequent bytes, and the general encoding with a
  packed transition table; per-node **pack sizes** for transition addresses and outputs
  (bytes chosen by the max value in that node).
- **Outputs:** transition outputs summed along the path, with the minimal output pushed
  toward the root (the transducer property); final-state outputs.
- **Header + footer:** version/type prefix and the trailing root address + length +
  **checksum** the crate appends and verifies on read.

Pin the exact crate version (match `rust/Cargo.toml`'s `fst`), treat its serialization
module as the spec, and reproduce it.

## Scope

1. **Builder** (`MapBuilder`/raw `Builder` equivalent): insert sorted `(key: []byte, val:
   uint64)` → minimized FST → bytes identical to Rust. Required for the build side.
2. **Reader** (recommended for round-trip tests): `get` and **range streams** (the RRTI v2
   router resolves a term's block via `range().ge(term)`). No Levenshtein automaton needed —
   009 routes fuzzy to the trigram `RRS` index and dropped the `fst` `levenshtein` feature.
3. **Conformance** in `go/conformance/`: a shared set of `(key → value)` corpora built in
   Rust (`fst`) and Go; assert **byte-identical** output + matching checksum. Fuzz with
   random sorted key sets (varying key lengths, shared prefixes/suffixes, output magnitudes
   to exercise pack-size boundaries).

## Starting points / alternatives

- **`blevesearch/vellum`** is the Go analogue of `fst` (same concepts, FST build+query) but
  its serialization is **not byte-identical** — useful as a structural reference / possible
  fork base, not a drop-in.
- **Shell out to a tiny Rust helper** to build the FST from Go — avoids the port but breaks
  the pure-Go build side (a build-time Rust dependency). Acceptable fallback if the port
  proves too costly and 009 isn't taken.

## Open questions

- Is byte-exact Go conformance for RRTI worth the FST port cost? (009 shipped the router as an
  FST, so the only way to keep the Go-builds-byte-identical guarantee for RRTI is this port; the
  alternative is to accept that Go can't build RRTI, or to shell out to a Rust helper.)
- Builder only, or builder + `get`/range reader for round-trip tests? (No fuzzy automaton.)
- Which `fst` version to pin (and how to track upstream serialization changes).
