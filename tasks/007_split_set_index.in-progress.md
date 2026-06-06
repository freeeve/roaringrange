# Task 007 — Split-set index (`RRSS`): byte-capped immutable splits + write delta

**Status:** in progress — **steps 1–7 implemented** behind the non-default `splits` Cargo
feature: manifest reader/writer + format (`SPLITSET.md`), byte-capped `SplitSetBuilder`,
pruning/merging `SplitSet::search` (tiered short-circuit + stable-key sort), base+delta
cutover with tombstone supersession + minor `compact`, ingestion `SplitSetWriter` (pure
flush/compact), **per-split term Bloom-filter pruning** (summary TLV tag 1 + `gramSize` header
field — skip a split that can't contain a query n-gram without a fetch), Go manifest
byte-for-byte conformance, the monolith-vs-`RRSS` benchmark, and a wasm `RrssIndex`, and a
**Python** binding (`SplitSetBuilder` + `SplitSetWriter`). 18 Rust splitset tests + Go golden +
5 pytest green; fmt+clippy+`go test`+maturin/pytest gated in CI and the pre-push hook.
Also added the **boot bundle** (`Index::from_boot`/`boot_len` + `SplitFetcher::boot` hook;
`MemberTag::Rrss` + `Hotcache::inlined_by_name`; end-to-end `tests/splitset_bundle.rs` under
`splits`+`hotcache`, gated in CI + pre-push) and a worked **queue-as-WAL ingestion client**
(`examples/splitset_ingest.rs`), and the **Go split-assignment builder** (`go/splitsetbuild.go`)
with byte-for-byte split conformance (shared golden, asserted on both sides). 19 splitset + 1
bundle Rust test + Go builder/conformance tests. **Remaining** (see per-step notes): the
per-split **facet-presence + time** summaries (tags 2-3, blob reserved — facet pruning also
needs a facet-aware split-set search path), and the **demo "Split set" UI** mode.

A *new, additive* index that sits alongside `RRS`/`RRTI`/`RRVI`/`RRIL`, so we can
compare it **side by side** with the monolithic `RRS`. It is a Quickwit-style
**split set**: a manifest naming many small immutable splits, queried with
**pruning** (for bandwidth) and a **base + delta + manifest** lifecycle (for
freshness). Nothing about the existing indexes changes.

## Why (and the distinction that shapes everything)

Two motivations that are commonly conflated under "LSM/Quickwit" but are *orthogonal*:

1. **Bandwidth + a size cap (read-side).** Today one monolithic `RRS` (107 GB for
   the full corpus) is on the hot path. The lever that actually cuts per-query bytes
   is **pruning** — read only the splits that can match. This is Quickwit's real
   speedup (it prunes by timestamp), *not* its log-structuredness.
2. **Freshness (write-side).** Absorb new/changed docs without a full rebuild via a
   small **delta** flushed on a cadence, merged at read time, compacted later. This
   is what `RRIL`'s base+delta discussion was about. It does **not** reduce bandwidth
   — splitting *adds* read fan-out (N headers/dictionaries) unless pruning offsets it.

Decision (this task does **both**): build the split set so pruning gives the
bandwidth win **and** a delta gives freshness, accepting that "both" makes the
rank-drift-vs-compaction tension real (see §Rank).

The roaringrange-native prune is **rank tiering**. `build.rs::split_posting` +
`DEFAULT_HEAD_BOUNDARY` is already a *two-tier* prune inside one file (resident head
of top-cited docs + range-fetched tail). We generalize "2 tiers in 1 object" to "N
byte-capped splits," so a top-K query reads only a tiny top tier and the cold tail
leaves the hot path.

## Format (provisional names)

- **Split** = a vanilla `RRS` over a doc subset, built by the *existing* builder.
  Reused unmodified, so the side-by-side comparison is apples-to-apples (1 split =
  today's monolith). Named `‹base›-s00000.rrs`, `…-s00001.rrs`, …
- **`RRSS` manifest** (`‹base›.rrss`, magic `RRSS`) = the only genuinely new artifact.
  It lists splits and carries the per-split **pruning summaries** so the split objects
  stay plain `RRS`. Layout sketch:
  - Header: magic, version, `policy` (tiered | stable-key), split count, base/delta
    boundary (splits `[0, baseCount)` are base, `[baseCount, end)` are delta),
    optional global `sortcol` descriptor (for stable-key rank), flags.
  - Per-split entry: URL/suffix, doc count, **rank range `[rank_lo, rank_hi]`** (or
    tier id), **doc-id range**, byte size, and *optional* summaries: a **term Bloom
    filter** (skip a split whose vocabulary can't contain a query term — the biggest
    fan-out reducer), facet/category presence bitset, time min/max. Supersession/
    tombstone epoch for delta-over-base.
- Splits and facets compose unchanged: `vector_id == doc_id == rank` still holds
  per split, so `RRF`/`RRVI`/`RRTI` can each become split sets independently later.

## Build-time policy (manifest records which; reader adapts)

Both supported; chosen at build (the "decide at build" answer). Each reuses existing
machinery:

- **Rank-tiered** — docs assigned to splits by rank (top-cited → split 0). Within the
  byte cap a tier may span several splits; the manifest maps tier→splits. Pruning:
  read tier 0 first, descend only if the page isn't filled. Cost: rank drift moves
  docs between tiers → **compaction must re-tier** on the rebuild cadence. Reuses the
  existing global rank ordering.
- **Stable-key** — docs assigned by ingest order / stable id; **rank is a query-time
  fast-field** via `sortcols.rs` (`SortCols`). Splits never re-sort on drift (LSM-
  clean), but every non-pruned split must be read and globally sorted → **loses the
  top-K prune** (Bloom/facet pruning still applies). Reuses `SortCols`.

## Byte cap (the "cap bytes per split" answer — lossless)

- Each split sealed at a configurable cap (e.g. 16/32/64 MB). Total grows with the
  corpus; **nothing is dropped**. Pruning (not truncation) keeps per-query bytes down.
- Mechanics: greedily add docs (in policy order) to the open split, estimating
  serialized size from accumulated postings; seal when the estimate nears the cap,
  start the next. A tier larger than the cap → multiple splits (manifest records it).
  Post-build assert each split ≤ cap; if a single doc's postings exceed the cap, fail
  loudly (degenerate corpus).

## Lifecycle: base + delta + manifest + compaction

- **Base** = the bulk split set, rebuilt on the existing full-build cadence.
- **Delta** = splits for docs added/changed since the base, flushed small + often.
- **Manifest** = atomic cutover point: a rebuild writes new immutable base splits, a
  flush writes a new immutable delta split, **one manifest write** flips the pointer so
  a reader never mixes a base from one build with a delta from another. Single-writer
  discipline on the build/flush job. (This is `hotcache.rs`'s manifest, evolved.)
- **Supersession** (updates/deletes): delta-over-base by doc-key. The merge consults
  delta splits first; a doc present in the delta masks the base copy (tombstone epoch
  in the manifest entry). For an additions-only corpus (OpenAlex works rarely change
  identity) this degenerates to a plain union — but the format carries the rule.
- **Compaction**: merge delta into base (and merge small splits to bound fan-out /
  re-tier under drift). Piggybacks the existing base rebuild. The "base + ONE delta"
  stance holds for *batch/slow* updates; **continuous ingestion overturns it** and needs
  a light L0→L1→base leveling — see §Ingestion.

## Ingestion / write path — a *builder API*, not a service

roaringrange stays a **library, not a service**: it exposes a builder-style writer in
Rust (+ a Python binding) and the embedding app owns transport, durability, scheduling,
and deployment. The writer is **pure** — bytes in, bytes out, no I/O, no threads, no
queue, no scheduler — exactly like `TermIndexBuilder` (new/add/finish), extended to
*multi-flush + manifest + compaction*. A "memtable container" is then just one way a
client *deploys* this builder; a periodic batch job is another.

**API surface** (`SplitSetWriter`, native build-side, mirrored in Python):
- `open(manifest, config) -> Writer` — resume over an existing split set (or fresh).
- `add(text, record, facets) -> doc_id` — append to the in-RAM memtable (a long-lived
  `TermIndexBuilder`'s `BTreeMap<term,bitmap>` + record buffer + facet maps); the writer
  allocates a monotonic id in the reserved high range, so callers never manage ids.
- `delete(doc_id)` — record a tombstone (supersession).
- `memtable_bytes() / doc_count()` — so the **caller** decides when to flush (size
  trigger); the interval trigger is just the caller's own timer.
- `flush() -> { split_bytes, split_meta, new_manifest }` — seal the memtable into an
  immutable **L0 `RRS` delta** + an updated manifest, **returned as bytes**. The caller
  does the I/O (PUT the split, then PUT the manifest = atomic cutover) and its own
  durability (ack its queue / truncate its WAL) only after both succeed. Resets the memtable.
- `compact(splits) -> { bytes, new_manifest }` — pure merge for L0→L1 and L1→base
  re-tiering; the caller fetches the input split bytes, decides *when*, and PUTs the result.

**Library owns mechanism; client owns policy.** Mechanism = memtable accumulation,
immutable-split serialization, doc-id allocation, manifest-cutover format, merge/
compaction, supersession. Policy (all client-side) = where writes originate, how durable
an ack must be, when to flush/compact, where bytes land, how the process is deployed, and
single-writer discipline. The browser stays read-only; a client sees a write only after
the split+manifest land in S3 and it re-reads the manifest, so **freshness = the client's
flush cadence**.

A client's loop is tiny — the library is the two pure calls:
```
let mut w = SplitSetWriter::open(load_manifest(), cfg);
loop {
    for msg in my_source.poll() { w.add(msg.text, msg.rec, msg.facets); }   // any source
    if w.memtable_bytes() > CAP || timer.elapsed() > INTERVAL {
        let f = w.flush();                  // pure
        put(split_key(&f), f.split_bytes);  // client I/O
        put(MANIFEST_KEY, f.new_manifest);  // client I/O — atomic cutover
        my_source.ack();                    // client durability (queue-as-WAL, etc.)
    }
}
```
We *document* client durability patterns — queue-as-WAL (pull from SQS/Kinesis; the queue
is the WAL), push + local WAL, or fire-and-forget — as examples, not as library code.

**Doc IDs — the crux (unchanged).** A fresh doc has no stable rank and a stream can't be
rank-ordered, so **delta splits are always ingest-ordered (stable-key)**: the writer hands
out monotonic high-range ids, ranked by a query-time `sortcols` fast-field (or appended
low). **Rank-tiering happens only at `compact()` into the base.** So the live path is
always the simple stable-key policy; the tiered/stable-key fork applies only to the base.

**Forced leveling — the client schedules it.** Flushing every 5 min is ~288 L0 deltas/day;
reading them all per query (even pruned) reintroduces fan-out. So a short-interval client
calls `compact()` on its own cadence: **L0** flush deltas → minor compaction → **L1** →
major compaction → re-tiered **base**. The library provides the merge; the client picks
the trigger (L0-count / time / bytes). Pruning still pays across the stack (L0/L1 carry
time + term-Bloom summaries), so a query on an established topic skips the recent deltas.

**Single writer.** Manifest cutover assumes one live `SplitSetWriter` per index; scale-out
needs a client-supplied writer lease/epoch. Keep single-writer for v1.

## Reader (new `splitset.rs`, behind a `splits` feature; reuses `Index`)

1. Open manifest → construct one `Index` per split (lazy; boot only reads the manifest
   + the top tier's headers, via an `RRHC`-style bundle to keep boot at 1–2 round trips).
2. **Prune** the split list for the query: drop splits by rank range (tiered short-
   circuit), term Bloom (no query term → skip), facet/time min-max.
3. **Search + merge**:
   - tiered → read tier 0, fill the page in rank order, **stop** unless under-filled;
     descend tiers on demand / pagination. ← the bandwidth win.
   - stable-key → search all surviving splits, merge, sort top-K by the `SortCols`
     rank column.
   - apply supersession (delta masks base) during the merge.
4. Head/tail becomes **per-split** (each `RRS` keeps its own); for small tiered splits
   it often degenerates to a single resident region — fine.

## Side-by-side benchmark (the point of the task)

Reuse the corpus workload generator + the demo's byte/request instrumentation to
report, over the *same* corpus and query log:

| metric | monolithic `RRS` | `RRSS` tiered | `RRSS` stable-key |
|---|---|---|---|
| boot bytes / reqs | | | |
| per-query bytes / reqs (top-K, no filter) | | | |
| per-query bytes / reqs (facet-filtered) | | | |
| freshness (build→visible) | full rebuild | flush cadence | flush cadence |
| total on-S3 size | | | |

Hypothesis: tiered `RRSS` cuts top-K per-query bytes sharply (top tier only) and boot
(RRHC bundle), at a modest total-size overhead (per-split headers + Bloom summaries);
stable-key trades the top-K prune for drift-immunity.

## Steps

1. **DONE.** `RRSS` manifest format + reader/writer (`splitset.rs` / `splitset_build.rs`),
   splits as reused `RRS`. Per-split rank/doc-id ranges done; Bloom/facet summaries are a
   reserved TLV blob (v1 writes none). Byte layout frozen in `SPLITSET.md` (64 B header w/
   policy + base/delta boundary + stable-key sortcol descriptor; 56 B per-split entry w/
   tier + doc-id range + byte size + epoch). Reader `splitset::SplitSet` (wasm-safe,
   two-read boot, `Policy`/`Split`/`SortColDescriptor`, base/delta partition + `summary()`)
   + native writer `splitset_build::write_splitset` (`SplitSpec`/`SplitSetConfig`/
   `SortColSpec`, metadata-only). Behind the `splits` feature; 9 tests; CI + pre-push gate
   `--features splits`.
2. **DONE.** Byte-capped split builder (greedy seal) for both policies — `SplitSetBuilder`
   (`add_text`/`add_keys`/`finish`), upper-bound size estimate seals at `byte_cap`, emits
   split `RRS` blobs + manifest; splits store **local 0-based ids** with `docIdLo` the global
   base (so 1 split == monolith). Degenerate single-doc-over-cap fails loudly.
3. **DONE.** Merging/pruning reader — `SplitSet::search` over a caller `SplitFetcher`: tiered
   short-circuit (open only the tiers that fill the page), stable-key `SortCols::topk`,
   remap local→global. Facet compose deferred with the summary blob.
4. **DONE.** Base+delta cutover + supersession + minor compaction — reader merges base+delta
   (delta appended in ingest order, tombstones mask base via the summary TLV); presence of
   deltas drops the fast path (the incentive to compact). `compact()` merges deltas into one
   **absolute-id** split (ids stable, no renumber), dropping tombstoned docs. Major re-tier =
   a full rebuild via `SplitSetBuilder`.
5. **DONE.** Go build side reproduces split assignment **and** the manifest byte-for-byte:
   `go/splitset.go` (`WriteSplitSet` manifest writer) + `go/splitsetbuild.go`
   (`SplitSetBuilder` — greedy seal, local-0-based ids, tiers, splitmix64 term Bloom). A shared
   golden `go/testdata/rrss_build_golden.txt` is asserted on **both** sides
   (`splitset_build::conformance_full_build` ⇄ `go/splitsetbuild_test.go`), covering split
   assignment, head/tail roaring serialization, and Bloom bytes. Confirmed Go roaring `ToBytes`
   == Rust `serialize_into` byte-for-byte. (`go/splitset.go` also gained `splitBitmapHB`/
   `writeIndexHB` to mirror the parametrized Rust `split_posting`/`write_index`.)
6. **DONE.** Benchmark `examples/splitset_bench.rs` (monolith-vs-`RRSS` byte/request table) +
   wasm `RrssIndex` (`open`/`search`/`searchFiltered`). **Demo DONE**: standalone harness
   `examples/splitset-demo/` (sample built by `splitset_demo_data.rs` + splits-wasm) AND the
   OpenAlex integration — builder `-split-set` subcommand (streaming, `drain_sealed`),
   comparison page `examples/openalex/web/splitset.html` (monolith vs split set: ms + bytes/reqs
   + result agreement), wasm bundle rebuilt with splits, `deploy.sh --splits`. The user builds +
   deploys the real 47M artifacts.
7. **DONE.** `SplitSetWriter` (`splitset_write.rs`): pure `new`/`resume`/`add`/`delete`/
   `memtable_bytes`/`doc_count`/`flush`/`compact` returning **bytes, no I/O**; high-range
   doc-id allocation; manifest-cutover format; L0 delta + minor compaction. **Python binding
   DONE** — `SplitSetBuilder` + `SplitSetWriter` PyO3 classes (`python/src/lib.rs`, behind the
   core `splits` feature), `SplitSet::from_bytes` added for sync resume; 5 pytest cases green
   via maturin. *Remaining:* a documented example client (queue-as-WAL container).
8. **DONE (2026-06-05).** **Dual-body: trigram (`RRS`) vs term/FST (`RRTI`) splits** — "try both."
   Manifest header byte 9 is now `bodyKind u8` (0=trigram default → golden byte-identical, 1=term).
   Reader `SplitBody` enum dispatches per split; `TermSplitSetBuilder`/`TermSplitBuildConfig` next
   to the trigram builder (seal via a factored `terms_build::write_term_index_from_postings`); RRTI
   reuses the rank-ordered head/tail so tiering/short-circuit/supersession are unchanged (no
   post-sort). OpenAlex `-term-splits` (+`-stem`/`-stopwords`) via a `SplitBuilder` enum sharing the
   chunked re-stream. Bench `term_vs_trigram_bench` shows **term ~2.5× smaller / fewer splits**,
   top-K overlap 1.00. `splits terms` gated in CI + pre-push. *Deferred:* term Bloom / bit-sliced
   split-presence sidecar (the real scalable term-prune), facet-filtered term search, Go/Python
   term-split builders.

## Open questions / risks

- **Boot fan-out**: N split headers must not reintroduce the cost pruning saves →
  the `RRHC` bundle for the top tier is load-bearing, not optional.
- **Bloom sizing**: false-positive rate vs manifest size; too-fat manifests eat the win.
- **Cross-split RRF/IDF**: term stats are per-split; global ranking/IDF needs either
  manifest-level term totals or accept per-split scoring. Decide before stable-key
  scoring matters.
- **Rank churn cost** (tiered): how often must compaction re-tier before pruning
  degrades? Monitor delta-size and tier-boundary crossings (the operational invariant,
  enforced by the rebuild schedule, not a compactor).
- **Ingest model + durability** (§Ingestion): **client's choice** — the builder is pure,
  so transport/durability/scheduling live in the embedding app. We provide the `flush`/
  `compact` bytes-in/bytes-out API and *document* patterns (queue-as-WAL, push+local WAL,
  fire-and-forget). Open: which example client(s) to ship, and whether the builder offers a
  `should_flush(policy)` convenience or leaves the trigger entirely to the caller.
- **Minor-compaction trigger**: L0-count threshold vs time vs bytes; and whether v1 ships
  the L0→L1 compactor or accepts a capped L0 stack until base rebuild.
- **Manifest read cadence**: how clients discover new deltas — re-fetch per search (simple,
  freshness = flush interval) vs a short TTL poll; bounds end-to-end visibility.
