# RRSS — roaring range split set (`RRSS`, version 1)

> **Status: implemented** behind the non-default `splits` Cargo feature — manifest
> reader/writer, byte-capped split builder, pruning/merging query path (tiered short-circuit +
> stable-key sort), base+delta cutover with supersession, minor compaction, a side-by-side
> benchmark, a Go builder (manifest + split `RRS` incl. per-split `RRSF` facets, byte-for-byte
> conformance), and a wasm binding. The geometric split sets are the demo's **default**
> client-side trigram/term backends (geometric per-tier byte caps; see Geometric tiering below).
> Remaining: only the per-split **time min/max** summary (tag 5). The Bloom, facet-presence,
> facet-digest (v1 and the opt-in v2, tag 6), and tombstone summaries and the demo split-set
> mode have shipped. This document is the frozen byte layout the readers/writers agree on.

A **split set** — an additive member of the roaringrange family (next to the trigram
`RRS`, term `RRTI`, facet `RRSF`, vector `RRVI`, record `RRSR`, lookup `RRIL`, sort
columns `RRSC`, and the `RRHC` boot accelerator). It is a Quickwit-style manifest naming
many small **immutable splits**, queried with **pruning** (read only the splits that can
match) and a **base + delta + manifest** lifecycle (absorb new docs without a full
rebuild). It replaces nothing: each split is a vanilla `RRS`, so one split is exactly
today's monolith. It is now the demo's **default** client-side trigram/term backend, with
the monolith kept as the fallback and side-by-side comparison.

The lever that cuts per-query bytes is **pruning**, not log-structuredness. The
roaringrange-native prune is **rank tiering**: `RRS` already splits each posting into a
resident head of the top-ranked docs and a range-fetched tail (`headBoundary`); `RRSS`
generalizes "two tiers in one file" to "N byte-capped splits," so a top-K query reads only
a tiny top tier and the cold tail leaves the hot path. Freshness is orthogonal: a small
**delta** of recently-added docs is merged at read time and compacted into the base later.

Only the **manifest** is a genuinely new artifact. A split is an unmodified `RRS`; the
manifest adds the cross-split pruning metadata (rank tier, doc-id range, byte size, and —
reserved for the enrichment step — term Bloom / facet-presence / time summaries) plus the
base/delta boundary and a per-split supersession epoch. All integers little-endian.

## Layout

`[ header ][ split entries ][ string blob ][ summary blob ]`

The split objects themselves live in **separate files**, one per split, each a plain index with
its own header. By default they are trigram `RRS` files (`‹base›-s00000.rrs`, … — `gramSize`,
`headBoundary`, sparse index); when the manifest's `bodyKind` is `1` they are term `RRTI` (FST)
files instead (`‹base›-s00000.rrt`, …), sharing the same rank-ordered doc-ID head/tail layout so
the cross-split machinery is unchanged. The `.rrss` manifest only names them and carries what the
monolith can't: how to prune across them.

**Header — 64 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRSS"` |
| version | u16 | 2 | `1` |
| flags | u16 | 2 | `bit0`=Bloom summaries present, `bit1`=facet summaries, `bit2`=time summaries, `bit3`=tombstones, `bit4`=case-sensitive (n-gram/facet keys not lowercased — queries derive keys without folding, and the splits are case-sensitive `RRS`/`RRTI`/`RRSF`); rest reserved (`0`) |
| policy | u8 | 1 | `0`=rank-tiered, `1`=stable-key (see [Policies](#policies)) |
| bodyKind | u8 | 1 | how each split data file is encoded: `0`=trigram `RRS`, `1`=term `RRTI` (FST). `0` keeps pre-bodyKind manifests valid; a term-bodied set needs the `terms` reader feature |
| tierCount | u16 | 2 | number of rank tiers (tiered policy); `0` for stable-key |
| splitCount | u32 | 4 | total splits (base + delta) |
| baseCount | u32 | 4 | splits `[0, baseCount)` are **base**, `[baseCount, splitCount)` are **delta** |
| strBytes | u32 | 4 | length of the string blob (split data-file names) |
| summaryBytes | u64 | 8 | length of the summary blob |
| byteCap | u64 | 8 | configured per-split byte cap the builder sealed at (informational) |
| sortcolNameOff | u32 | 4 | stable-key rank: `RRSC` data-file name → string blob (`nameLen 0` when none) |
| sortcolNameLen | u16 | 2 | rank `RRSC` name length in bytes |
| sortcolColumn | u16 | 2 | rank column index within that `RRSC` |
| sortcolFlags | u8 | 1 | `bit0`=descending (higher value = better rank) |
| gramSize | u16 | 2 | n-gram window the splits were built with — lets the reader derive a query's keys for Bloom pruning without opening a split. `0` for a term-bodied (`bodyKind=1`) set |
| pad1 | u8[5] | 5 | `0` |
| reserved | u8[8] | 8 | zero padding to 64 B |

**Split entry — 56 B each**, `splitCount` entries at offset `64`. Base entries
`[0, baseCount)` come first, then delta entries; within the base, tiered splits are ordered
`(tier, docIdLo)` so the reader walks tiers in rank order:
| field | type | bytes | notes |
|---|---|---|---|
| nameOff | u32 | 4 | split `.rrs` data-file name → offset into the string blob |
| nameLen | u16 | 2 | name length in bytes |
| tier | u16 | 2 | rank tier (tiered policy; `0` for stable-key / delta) |
| docCount | u32 | 4 | docs in the split |
| docIdLo | u32 | 4 | min **global** doc id present (inclusive); the local-id base (see below) |
| docIdHi | u32 | 4 | max **global** doc id present (inclusive) |
| flags | u16 | 2 | `bit0`=has-tombstone summary, `bit1`=absolute-ids; rest reserved (`0`) |
| pad | u16 | 2 | `0` |
| byteSize | u64 | 8 | the split `.rrs` file size in bytes (byte-cap assert + total-size accounting) |
| epoch | u64 | 8 | flush/build epoch — supersession ordering (`0` for an additions-only base) |
| summaryOff | u64 | 8 | offset into the summary blob for this split's summaries (`0` when none) |
| summaryLen | u32 | 4 | summary length in bytes (`0` in v1) |
| reserved | u32 | 4 | `0` |

**Doc IDs — local vs global.** A split's `RRS` stores **local 0-based** doc ids, and `docIdLo`
is the split's **global base**, so `global = docIdLo + local`. This keeps every split
structurally identical to the monolith (a working head/tail, same default `headBoundary`), so
one split is exactly today's `RRS`. For the tiered policy `global` is the rank. The `bit1`
**absolute-ids** flag flips this to `global = local` (the split stores global ids directly):
[compaction](#writer--builder-api-surface) sets it when surviving ids are gappy and must stay
stable (no renumber), and then `docIdLo`/`docIdHi` bound the global range present.

**String blob** — `strBytes` of UTF-8, at `64 + splitCount*56`. Each split's data-file name
is `stringBlob[nameOff .. nameOff + nameLen]`; the stable-key rank `RRSC` name is sliced the
same way by the header's `sortcolNameOff`/`sortcolNameLen`.

**Summary blob** — `summaryBytes` bytes, at `64 + splitCount*56 + strBytes`: per-split
pruning summaries, each split's region at `[summaryOff, summaryOff + summaryLen)`. Within a
split's region the summaries are **TLV** records `[tag u8][len u32 LE][bytes]`:
| tag | summary | bytes | status |
|---|---|---|---|
| `1` | term Bloom filter | `[k u32][nbits u32][⌈nbits/8⌉ bytes]` — skip a split whose vocabulary can't contain a query n-gram | **implemented** |
| `2` | facet-presence list | `[count u32][key u64]*` (sorted `facet_key`s) — skip a split holding none of a filtered field's categories | **implemented** |
| `3` | facet digest | `[k u16][fieldCount u16]`, per field `[nameLen u16][name][catCount u16]`, per category `[nameLen u16][name][count u32][headOff u64][headSize u32][tailSize u32]` — the top-`k` categories per field by full-corpus count, so facet pricing boots from the resident manifest with no sidecar meta read | **implemented** |
| `4` | tombstone posting | a portable RoaringBitmap of superseded base doc IDs (delta-over-base) | **implemented** |
| `5` | time min/max | two `i64` (or the app's epoch unit) for time-range pruning | reserved |
| `6` | facet digest v2 | the tag-3 layout with each category extended by `[containerCount u16]` then per container `[key u16][cardMinus1 u16][start u32]` — the category's tail container directory (`start` relative to the tail posting, last length implied by `tailSize`), so pricing skips the per-category tail-header read (facet wave A) for a large tail. A reader prefers tag 6, else tag 3, else the sidecar meta. **Opt-in** (`with_facet_digest_v2` / Go `SetFacetDigestV2`): a conditional win (only tail-only priced categories benefit) and larger, so v1 stays default | **implemented** |

The **term Bloom filter** (tag 1) is the biggest fan-out reducer: built over the split's
n-gram vocabulary with `bloom_bits_per_key` bits per key (`~10` ≈ 1% false positives), it lets
a query skip any split that definitely lacks one of its n-grams (Bloom filters have no false
negatives) — *without a fetch*. It is most decisive for rare/absent terms: a tiered query that
can't fill its page would otherwise descend through every split. The filter uses double
hashing over two `splitmix64` derivations of each `u64` key, so it is deterministic and
reproducible across languages. The time summary (tag 5) remains reserved; the blob is empty
when no summaries are written, so adding new tags stays a purely additive write.

## Policies

The builder picks one policy and records it in `policy`; the reader adapts. Both reuse
existing machinery — a split is always a plain `RRS`.

- **Rank-tiered** (`policy = 0`). Docs are assigned to splits by rank: the top-cited docs go
  to tier 0, the next band to tier 1, and so on (a tier wider than the byte cap spans
  several splits, recorded by repeating `tier`). Because doc IDs are assigned in descending
  popularity (ascending doc id == descending rank, the `RRS` invariant), a split's **rank
  range is exactly its `[docIdLo, docIdHi]`** — no separate rank field is needed. Pruning:
  read tier 0 first, descend only when the page under-fills. Cost: rank drift migrates docs
  between tiers, so compaction must **re-tier** on the rebuild cadence.

- **Stable-key** (`policy = 1`). Docs are assigned by ingest order / stable id; rank is a
  **query-time fast field** in the `RRSC` named by the header's sortcol descriptor
  (`rank[doc_id]`, sorted by `SortCols::topk`). Splits never re-sort on drift (LSM-clean),
  but the top-K prune is gone — every non-pruned split is read and globally sorted (Bloom /
  facet / time pruning still applies). `tier` is `0` throughout.

**Delta splits are always stable-key / ingest-ordered**, regardless of base policy: a fresh
doc has no rank and a stream can't be rank-ordered, so the writer hands out monotonic
**high-range doc ids** and rank-tiering happens only when `compact()` folds a delta into the
base. (Doc ids are `u32` across the whole family; the reserved high range is reclaimed at
base compaction, which renumbers delta docs into rank order.)

## Reader

- **boot:** one ranged read of the header (64 B) pins the section sizes, then one ranged read
  of the remaining `splitCount*56 + strBytes + summaryBytes` bytes makes the whole manifest
  resident. The per-split `RRS` objects are opened **lazily**. To keep boot at 1–2 round
  trips despite N splits, an `RRHC` boot bundle inlines the manifest plus the **top tier's**
  split boot regions (header + sparse index per tier-0 split); the cold tail's boots are
  fetched only if pagination descends into them. The `RRHC` boot bundle is load-bearing, not
  optional — N split headers must not reintroduce the cost pruning saves.
- **splits():** the parsed entries, in manifest order, each with its data-file name, `tier`,
  `[docIdLo, docIdHi]`, `byteSize`, `epoch`, and (when present) its summary region.
- **prune(query):** drop splits that cannot match — by tier (tiered short-circuit), by
  doc-id / rank range, and (enrichment step) by term Bloom, facet-presence, and time min/max.
- **open(i):** construct an `Index<F>` over split `i`'s data file (a plain `RRS::open`),
  reusing its boot bytes from the `RRHC` bundle when inlined.

## Query

1. **prune** the split list for the query.
2. **search + merge**:
   - **tiered** → search tier 0's surviving splits, merge their hits in rank (== doc-id)
     order, fill the page, and **stop** unless under-filled; descend to the next tier on
     demand / pagination. ← the bandwidth win (top-K reads only the top tier).
   - **stable-key** → search every surviving split, merge, and take the top-K by the sortcol
     descriptor's `RRSC` rank column (`SortCols::topk`, tie-broken by doc id).
3. **supersession** (base + delta): walk delta splits (higher `epoch`) ahead of base; a doc
   in a delta's tombstone posting (summary tag `4`) masks the base copy. For an
   additions-only corpus (`baseCount == splitCount`, no tombstones) this degenerates to a
   plain union (tiered) or merge-sort (stable-key).
4. **head/tail** is per split: each split's `RRS` keeps its own `headBoundary`; a small tiered
   split often degenerates to a single resident head region.

## Writer / builder API surface

Two build paths, both pure (bytes in, bytes out — no I/O, threads, or scheduler), mirroring
the existing native writers:

- **Batch build** (`splitset_build`): `write_splitset(manifest_w, splits, config)` emits the
  `.rrss` manifest given the already-built split `.rrs` files' metadata; the byte-capped
  `SplitSetBuilder` does the **greedy seal** — add docs in policy order (rank for tiered,
  ingest for stable-key), estimate the open split's serialized size from accumulated
  postings, seal when the estimate nears `byteCap`, start the next. A tier wider than the cap
  becomes several same-`tier` splits. Post-build, every split is asserted `≤ byteCap`; a
  single doc whose postings exceed the cap fails loudly (degenerate corpus). Nothing is
  dropped — pruning, not truncation, keeps per-query bytes down; total grows with the corpus.
- **Ingestion writer** (`SplitSetWriter`, the lifecycle step): a long-lived builder exposing
  `open(manifest, config)` / `add(text, record, facets) -> doc_id` / `delete(doc_id)` /
  `memtable_bytes()` / `flush() -> { split_bytes, new_manifest }` / `compact(splits) -> {
  bytes, new_manifest }`. The memtable is a long-lived `TermIndexBuilder`-style
  `BTreeMap<term, bitmap>`; `flush()` seals it to an immutable **L0 `RRS` delta** plus an
  updated manifest, returned **as bytes** — the client does the I/O (PUT the split, then PUT
  the manifest = atomic cutover) and owns transport, durability, scheduling, and
  single-writer discipline. `compact()` is the pure merge for L0→L1→base re-tiering. The
  library owns mechanism; the client owns policy. The browser stays read-only; freshness =
  the client's flush cadence.

## Conformance

A Go build side reproduces split assignment + the manifest **byte-for-byte** (same discipline
as `conformance/`'s n-gram keys), so a split set built by either language reads in either.
Shipped: `splitset.go` + `splitsetbuild.go` reproduce the manifest and every split `RRS`
(incl. per-split `RRSF` facet sidecars and term Bloom filters) byte-for-byte against a shared
golden — see the Status section.

## Open questions

- **Boot fan-out.** The `RRHC` bundle for the top tier is load-bearing; without it N split
  headers re-add the cost pruning removes.
- **Bloom sizing.** False-positive rate vs manifest size — too-fat summaries eat the win.
- **Cross-split IDF / RRF.** Term stats are per split; global ranking/IDF needs either
  manifest-level term totals or accepting per-split scoring. Decide before stable-key scoring
  matters.
- **Rank churn (tiered).** How often must compaction re-tier before pruning degrades? The
  operational invariant is delta size + tier-boundary crossings, enforced by the rebuild
  schedule, not a live compactor.
- **`u32` doc-id ceiling.** The reserved delta high range shares the `u32` space; a
  long-running continuous ingest must compact (renumber) before it nears `2^32`.

## Status

Implemented behind the non-default `splits` Cargo feature (pure Rust, no new dependency; a
split is an `Index`, stable-key rank is `SortCols`, the manifest evolves `RRHC`):

- **Reader** `splitset::SplitSet` (wasm-safe) — two-read manifest boot; exposes the splits,
  the base/delta partition, the sort-column descriptor, and the summary regions. The query
  opens each split via the caller's `SplitFetcher`, which may supply a split's resident boot
  bytes (`SplitFetcher::boot`) so the split opens with `Index::from_boot` — **no boot fetch** —
  the mechanism a tier-0 `RRHC` boot bundle uses to fold split opens into a 1–2 round-trip boot.
- **Query** `SplitSet::search` / `search_filtered` over a caller `SplitFetcher` — tiered
  short-circuit (read only the tiers that fill the page), stable-key `SortCols::topk`,
  **term-Bloom pruning** (skip a split that can't contain a query n-gram), **facet-filtered
  search** (each split's own `‹split›.rrf` resolves the filter) with **facet-presence pruning**
  (skip a split holding none of a selected field's categories), and a base+delta merge with
  tombstone supersession when deltas are present — all without fetching the pruned splits.
- **Batch builder** `splitset_build::SplitSetBuilder` — the byte-capped greedy seal for both
  policies, emitting the split `RRS` blobs + the manifest, with optional per-split term Bloom
  filters (`bloom_bits_per_key`) and, when documents carry facets (`add_faceted`), a per-split
  `RRSF` sidecar + a facet-presence summary.
- **Ingestion writer** `splitset_write::SplitSetWriter` — pure `new`/`resume`/`add`/`delete`/
  `memtable_bytes`/`flush`/`compact` (bytes in, bytes out); flush seals an L0 delta + a
  cutover manifest, compact merges deltas into one absolute-id split dropping tombstoned docs.
- **Conformance** — the Go build side (`splitset.go` manifest writer + `splitsetbuild.go`
  `SplitSetBuilder`) reproduces the manifest **and** every split `RRS` (split assignment,
  head/tail roaring serialization, and term Bloom filters) **byte-for-byte**, proven by a shared
  golden (`testdata/rrss_build_golden.txt`) asserted on both sides (`splitset_build::tests`
  ⇄ `splitsetbuild_test.go`).
- **Benchmark** `rust/examples/splitset_bench.rs` — the monolith-vs-`RRSS` byte/request table.
- **Wasm** `RrssIndex` (`wasm` + `splits`) — `open(manifestUrl, baseUrl)` + `search` +
  `searchFiltered(query, filters, limit)` (facet filter entries `"field\tcategory"`).
- **Python** (`python/`, PyO3) — `SplitSetBuilder` (batch, writes the manifest + split files)
  and `SplitSetWriter` (`new`/`resume`/`add`/`delete`/`memtable_bytes`/`flush`/`compact`, bytes
  in/out), mirroring the Rust API.
- **Boot bundle** (`splits` + `hotcache`) — an `RRHC` (`MemberTag::Rrss`) inlines the manifest +
  the top tier's split boots; the reader feeds them to splits via `SplitFetcher::boot` /
  `Index::from_boot` (no per-split header fetch). End-to-end test `tests/splitset_bundle.rs`.
- **Examples** — `rust/examples/splitset_bench.rs` (the byte/request table) and
  `rust/examples/splitset_ingest.rs` (a worked queue-as-WAL ingestion client over `SplitSetWriter`).

**Deferred**: the per-split **time min/max** summary (tag 5 — for time-range pruning; the blob
carries it but the builder doesn't write it yet), and the **Python facet** build surface (Rust
and Go builders emit per-split `RRSF`; the Python builder doesn't yet). The demo "Split set"
mode shipped (and the geometric trigram/term split sets are the demo's default client-side
backends).

## Geometric tiering

The live split sets size their tiers **geometrically**: the per-tier byte cap doubles down the
rank order (`byte_cap` base → `byte_cap_max` ceiling; Rust/Go/Python share the `cap_for`/`capFor`
golden), so the top of the corpus is cut into small splits (fine pruning where common queries
concentrate) while the tail doubles up to a handful of large splits. A full worst-case descent
then costs ~log-many split visits instead of one-per-fixed-size: the live **trigram** set is
**19 tiers** (`openalex-trigram-geo`, 498 MB top doubling to ~8 GiB, ≤32M docs/split) and the
**term** set is **12 tiers** (`openalex-term-geo`, 240 MB doubling to ~10 GB) — versus the
earlier flat 389-trigram / 243-term uniform-cap sets. The geometric sets are built fastest by
**slicing an existing monolith by doc range** in one sequential pass (`rust/examples/slice_trigram_monolith.rs`,
`slice_term_monolith.rs`), byte-identical to what the greedy builder would emit for the same
ranges; the manifest's `byteCap` is then informational (0 for doc-range slices).
