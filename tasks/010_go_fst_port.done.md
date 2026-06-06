# Task 010 — Byte-exact Go port of the `fst` crate (BurntSushi)

**Status:** done 2026-06-06 (FST port + reader + fuzz + cross-language conformance).
Integration into roaringrange's `go/` + the full RRTI `.rrt` Go writer remain (see Outcome).

## Outcome (2026-06-06)

A standalone pure-Go module at **`~/fst-go`** (module path `github.com/freeeve/fst`,
package `fst`) builds and reads FSTs **byte-identical** to the Rust `fst` crate, pinned to
**v0.4.7** (the version in `rust/Cargo.lock`). `Builder.Finish()` reproduces
`fst::MapBuilder` bytes exactly — header, online-minimized nodes, footer, and trailing
masked CRC32C. **Zero third-party Go deps** (only stdlib `hash/crc32` for the Castagnoli
checksum).

**What shipped (builder + reader + fuzz + conformance, the scope chosen):**
- **Builder** (`builder.go`, `unfinished.go`, `node.go`, `registry.go`, `bytes.go`,
  `common_inputs.go`, `crc32.go`): faithful port of `raw::Builder` — streaming Daciuk
  minimization, the **bounded** LRU register (`10000×2`, FNV-1a) that decides dedup (not
  perfect minimization), the three node encodings (OneTransNext / OneTrans / AnyTrans), the
  common-inputs table, per-node pack sizes, reverse-order trans/output/input layout, the
  256-byte index past 32 transitions, and `prefix=min`/`cat=add`/`sub=sub` output math.
- **Reader** (`reader.go`): `New` (header/footer parse + checksum verify), `Get`, `Iter`
  (in-order), and `Ge` (the router's `range().ge(term)` successor query). No automaton/fuzzy
  path (RRTI sends fuzzy to the trigram `RRS`).
- **Conformance** (`conformance_test.go` + `rustgen/`): corpora authored once in
  `conformanceCorpora()` → `testdata/corpora.txt`; the Rust generator (`fst = "=0.4.7"`)
  builds them → `testdata/fst_golden.txt`; the Go builder must match every golden byte. 13
  corpora pass, incl. a 5000-key corpus (heavy dedup), `many_trans` (256-index), all
  output pack-size boundaries, high bytes, UTF-8, and the real `router_like` RRTI shape.
- **Fuzz** (`fuzz_test.go`): 400+ seeded round-trips, a native `FuzzRoundtrip` target (ran
  clean, 27k+ execs), and `TestCrossLangFuzz` — **300 random corpora matched the Rust crate
  byte-for-byte**. `go test ./...` green; gofmt + `cargo fmt --check` clean.

**Resolved open questions:** worth the port? — yes, and done. builder-only vs reader? —
builder **+** `get`/`range` reader. version pin? — **0.4.7** (track via `rust/Cargo.lock`;
the golden is regenerated from the pinned crate).

**Perf pass (allocation reduction + pprof, all output-byte-preserving — conformance still
green):** `bench_test.go` benchmarks build + read with `ReportAllocs`. Builder allocs/op cut
~97% (terms_20k 189124 → 4599) via an inline pending transition (was a heap `*lastTransition`
per suffix byte), a transition-slice free list, and a pre-warmed minimization-register arena;
`Get` is now **0 allocs/op** (reused stack scratch). Added `Builder.Reset()` for reuse (the
fixed ~1 MB register is allocated once, not per build) → reused terms_20k build is **35
allocs / 15.8 KB**, ~30% faster. The ~1 MB register floor on a fresh build is inherent (the
`hash % 10000` bucketing fixes the table size). Profile via `-memprofile`/`-cpuprofile` +
`go tool pprof` (see README Performance).

**Remaining (NOT this pass — the "Full RRTI `.rrt` writer" option):** move/vendor `~/fst-go`
into roaringrange's `go/` (e.g. `go/fst/`) and wire the cross-language test into
`go/conformance/`; then port the `terms_dict` front-coded block codec + RRTI v2 header +
roaring postings so Go emits a complete `.rrt`. The tokenizer/stemmer is task **011**.

---

### Original scoping notes (for reference)

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
