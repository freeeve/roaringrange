# roaringrange

[![CI](https://github.com/freeeve/roaringrange/actions/workflows/ci.yml/badge.svg)](https://github.com/freeeve/roaringrange/actions/workflows/ci.yml)
[![Coverage Status](https://coveralls.io/repos/github/freeeve/roaringrange/badge.svg?branch=main)](https://coveralls.io/github/freeeve/roaringrange?branch=main)
[![Go Report Card](https://goreportcard.com/badge/github.com/freeeve/roaringrange)](https://goreportcard.com/report/github.com/freeeve/roaringrange)
[![Go Reference](https://pkg.go.dev/badge/github.com/freeeve/roaringrange.svg)](https://pkg.go.dev/github.com/freeeve/roaringrange)
[![Maintainability Rating](https://sonarcloud.io/api/project_badges/measure?project=freeeve_roaringrange&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=freeeve_roaringrange)
[![Reliability Rating](https://sonarcloud.io/api/project_badges/measure?project=freeeve_roaringrange&metric=reliability_rating)](https://sonarcloud.io/summary/new_code?id=freeeve_roaringrange)
[![Security Rating](https://sonarcloud.io/api/project_badges/measure?project=freeeve_roaringrange&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=freeeve_roaringrange)
[![Snyk](https://snyk.io/test/github/freeeve/roaringrange/badge.svg)](https://snyk.io/test/github/freeeve/roaringrange)

Static, range-fetchable full-text search built on roaring bitmaps — query a
multi-million-document trigram index **in the browser, over HTTP Range requests,
with no backend.**

The index is one static file on object storage (S3/CDN). The browser fetches a
tiny sparse view once (~tens of KB), then each query pulls a few small byte
ranges — independent of corpus size. The multi-GB index is never downloaded
whole. Postings are portable RoaringBitmaps, so writers and readers interoperate
byte-for-byte with zero re-encoding: build an index **directly** with the Rust
`build` module, or **transcode** an existing
[roaringsearch](https://github.com/freeeve/roaringsearch) index with the Go
writer — both emit the same files the Rust/WASM (or Go) reader reads.

## Live demos

- **OpenAlex** — [openalex.evefreeman.com](https://openalex.evefreeman.com): **484M**
  scholarly works, citation-ranked, faceted (year/type/open-access/language/topic),
  with trigram, whole-word (term, with default-on BM25 relevance), semantic (IVFPQ),
  and hybrid search. Trigram defaults to a regional `/search` Lambda for speed; the
  client-side range-read modes (split-set, monolith, term, semantic — *no backend*)
  are one toggle away. (Reproducible — see [`examples/openalex/`](examples/openalex).)

## How it fits together

```
build (Go):   corpus ─roaringsearch─▶ FTSR ─roaringrange.Transcode─▶ RRS (.rrs) ─▶ S3/CDN
              (optional) facets ──────────────roaringrange.WriteFacets─▶ RRSF (.rrf) ─▶ S3/CDN
build (Rust): corpus ──build::write_index / write_facets / write_records──▶ .rrs + .rrf + records
browser (Rust/WASM): .rrs/.rrf/records on CDN ─HTTP Range─▶ Catalog (= Index + FacetIndex + RecordStore)
```

Doc IDs are assigned at build time in descending static rank (citations, holdings,
…), so ascending doc ID = rank. Each posting is one roaring bitmap read in rank-ordered
**buckets**: a query ANDs the small top bucket (the 65,536 top-ranked docs) to get the
ranked top-K and only pages deeper buckets when paginating past it. See
[`docs/format.svg`](docs/format.svg) and [`docs/search.svg`](docs/search.svg).

This is the core design trade-off: ranking is baked in up front as a static rank in
doc-ID order, in exchange for near-constant per-query fetch cost. (Term and hybrid modes
add optional, default-on BM25 lexical relevance via the RRSB `.rrb` sidecar.)

## How it compares

Inspired by [lunr.js](https://lunrjs.com) and [Pagefind](https://pagefind.app) —
both deliver full-text search to static sites with no backend. lunr loads the
whole index into memory; Pagefind pioneered fetching only the index shards a
query needs. roaringrange pushes that "fetch only what you query" idea to
*millions* of records by HTTP-Range-reading a single roaring-bitmap index file,
trading a general query-time relevance pipeline for a baked-in static rank (with
optional default-on BM25 on the term/hybrid modes).

| | lunr.js | Pagefind | roaringrange |
|---|---|---|---|
| backend | none | none | none |
| index transport | whole index in memory | many shard files, per query | one file, HTTP Range |
| typical scale | hundreds–few thousand docs | up to ~100k+ pages | millions–100M+ records |
| per-query bytes | 0 after full load (load can be MBs+) | ~tens–hundreds KB | ~KB–few MB (≈ constant) |
| matching | stemmed terms; wildcard, fuzzy, boosts | stemmed words; partial | trigram substring; fuzzy (tolerate-N) |
| ranking | TF-IDF / BM25 relevance | BM25-like relevance | **static rank** (query-independent) in doc-ID order, *plus* default-on **BM25** on term/hybrid (RRSB sidecar) |
| facets / filters | fielded search (no facet counts) | filters + facet counts | facets + live counts (sidecar) |
| build input | JS objects / prebuilt JSON | crawls built HTML pages | any records (Go via roaringsearch, or the Rust builder) |
| sweet spot | embedding a search library in a JS app (you own the index) | static sites & docs, small to large | very large catalogs & datasets |

In short: lunr.js when you want to embed a search *library* and control indexing
in code; Pagefind for static-site search from small to large; roaringrange when
you have *a lot* of records and want a single range-fetched file with static-rank
ordering and facets.

## Repository layout

| path | what |
|---|---|
| `*.go` (root) | core Go module (`github.com/freeeve/roaringrange` — `go get github.com/freeeve/roaringrange`): `Transcode` (FTSR→RRS), `Open`/`Index` reference reader, `WriteFacets`, `NgramKeys` |
| `*.md` (root) | the frozen on-disk format specs — see the [On-disk formats](#on-disk-formats) table below |
| `rust/` | Rust crate `roaringrange`: reader (`Catalog` over `Index`/`FacetIndex`/`RecordStore`) + native `build` writers; both exposed to WASM (`wasm-pack`). Optional `vector` feature adds the `RRVI` similarity-search reader + IVFPQ trainer |
| `rust/examples/` | runnable examples, each with a `//!` header stating its purpose and exact `cargo run … --example … [--features …]` command |
| `python/` | PyO3 bindings (`pip install roaringrange`): `Builder` (text index) + `VectorBuilder` (similarity index) over the core build writers |
| `conformance/` | cross-library test: roaringsearch build ⇄ roaringrange(go) read must agree |
| `examples/openalex/` | the OpenAlex demo: Go loader, parallel Rust `builder/`, `download.sh`, static web UI |
| `docs/` | architecture diagrams (SVG) |

## On-disk formats

roaringrange is an à-la-carte family of composable index files over **one shared rank-ordered
doc-ID space**. Each has a 4-char magic; **the magic and the file extension are intentionally
not always identical** (records and the re-rank sidecar use app-facing names). Every format's
reader and builder are listed below.

| Magic | Ext | Spec | Reader | Builder | What |
|---|---|---|---|---|---|
| `RRSI` | `.rrs` | [FORMAT.md](FORMAT.md) | `Index` / `RrsIndex` | Rust `write_index`, Go `Transcode` | trigram text index |
| `RRSF` | `.rrf` | [FACETS.md](FACETS.md) | `FacetIndex` / `RrfFacets` | Rust `write_facets`, Go `WriteFacets` | facet sidecar |
| `RRSR` | `.idx`+`.bin`(+`.dict`) | [RECORDS.md](RECORDS.md) | `RecordStore` / `RrsRecords` | Rust `write_records`, Go `WriteRecords` | record store (paired files; optional zstd dict) |
| `RRIL` | `.rril` | [LOOKUP.md](LOOKUP.md) | `Lookup` / `RrsLookup` | Rust `write_lookup` | identifier exact-match index |
| `RRSC` | `.rrsc` | [SORTCOLS.md](SORTCOLS.md) | `SortCols` / `RrsSortCols` | Rust `write_sortcols` | static sort/rank columns |
| `RRTI` | `.rrt` | [TERMS.md](TERMS.md) | `TermIndex` / `RrtIndex` | Rust `write_term_index`, Python | term index (blocked, range-fetched dict) |
| `RRSB` | `.rrb` | [BM25.md](BM25.md) | `ImpactIndex` / `RrbIndex` | Rust `write_impacts` | BM25 impact sidecar for the term index (lexical relevance) |
| `RRVI` | `.rrvi` | [VECTORS.md](VECTORS.md) | `VectorIndex` / `RrviIndex` | Rust `build_ivfpq`, Python | IVFPQ vector index |
| `RRVR` | `.rrvi.rerank` | [VECTORS.md](VECTORS.md#re-rank-sidecar-rrvr-optional) | `RerankStore` | Rust `write_rerank` | bf16 re-rank sidecar |
| `RRM2` | `.rrm2` | [VECTORS.md](VECTORS.md#model2vec-embedder-rrm2) | `Model2vec` / `Model2vecEmbedder` | `python/scripts/model2vec_export.py` | in-browser query embedder |
| `RRHC` | `.rrhc` | [HOTCACHE.md](HOTCACHE.md) | `Hotcache` | Rust `write_hotcache` | boot bundle (manifest + inlined members) |
| `RRSS` | `.rrss` | [SPLITSET.md](SPLITSET.md) | `SplitSet` / `RrssIndex` | Rust `SplitSetBuilder`, Go, Python | split-set manifest (splits are `.rrs`/`.rrt`) |

### Which builder/reader exists in which language

Readers ship as wasm/JS (the browser is the only reader runtime); builders run server-side in
Rust, Go, or Python. Rust is the reference — Go and Python expose deliberate subsets.

| Capability (format) | Rust | Go | Python | JS (read) |
|---|---|---|---|---|
| Trigram index `RRSI` | build + read | build | build | read |
| Facets `RRSF` | build + read | build | build | read |
| Records `RRSR` | build + read | build | build | read |
| Lookup `RRIL` | build + read | — | — | read |
| Sort columns `RRSC` | build + read | — | — | read |
| Terms `RRTI` | build + read | — | build | read |
| BM25 impacts `RRSB` | build + read | — | — | read |
| Vectors `RRVI` | build + read | — | build | read |
| Model2vec `RRM2` | read | — | export¹ | read |
| Hotcache `RRHC` | build + read | — | — | —² |
| Split set `RRSS` (trigram) | build + read | build³ | build | read |
| Split set `RRSS` (term) | build + read | — | build | read |

¹ via `python/scripts/model2vec_export.py`. &nbsp;² no JS reader yet (server-side only). &nbsp;³ trigram bodies only (Go builds the split set + per-split facet sidecars, but not RRTI term bodies).

The core Go module has **no dependency on roaringsearch** — it parses the `FTSR`
byte format directly. The n-gram key derivation is reproduced independently in Go
(here), Go (roaringsearch), and Rust (the reader); the `go/conformance/` module is
the guard that keeps all three byte-compatible.

## Quick start

**Build an index** — two paths to the same files. Assign doc IDs in descending
static rank first, so the head holds the top-K.

*Rust (direct):* split each posting into head/tail, then write the index + an
optional facet sidecar + record store:
```rust
use roaringrange::build::{write_index, write_facets, write_records, split_posting};
let entries = postings.iter()
    .map(|(k, bm)| { let (h, t) = split_posting(bm); (*k, h, t) })
    .collect();
write_index(rrs_w, 3, 0, entries)?;            // → .rrs
write_facets(rrf_w, facet_fields)?;            // → .rrf  (optional)
write_records(bin_w, idx_w, &records)?;        // → record store (optional)
```
For a corpus whose index exceeds RAM, build it in doc-ID-range chunks and fold
them into one standard `.rrs` with `build::chunk::{write_partial, merge_partials_to_rrs}`.

*Go (transcode a roaringsearch index):*
```go
rr.Transcode(ftsrReader, rrsWriter)             // FTSR → .rrs
rr.WriteFacets(rrfWriter, []rr.FacetField{...}) // → .rrf  (optional)
rr.WriteRecords(binW, idxW, records)            // → record store (optional)
```

**Read it (Rust/WASM):** build the reader, then open a `Catalog` and search:
```sh
cd rust && wasm-pack build --target web --features wasm
```
```js
import init, { RrsCatalog } from "./roaringrange.js";
await init();
const cat = await RrsCatalog.openAll("index.rrs", "index.rrf", "records.idx", "records.bin");
const page = await cat.search("query", 0, 25, 0, '[["type","article"]]');
// page.ids = ranked doc IDs · page.records · page.facetCounts
```
`Catalog`/`Index`/`FacetIndex`/`RecordStore` (and `RrsIndex`/`RrsRecords`) stay
available for advanced use. Host the files on anything that supports HTTP Range
(S3 + CloudFront works well) and point the reader at the URLs.

## Measured (full corpora, range-fetched)

| | library catalog 9.6M | OpenAlex 47.8M (with abstracts) |
|---|---|---|
| text index (`.rrs`) | 1.4 GB | 11.5 GB |
| boot — index sparse | ~52 KB | ~0.5 MB |
| typical query | tens–hundreds of KB | ~1–6 MB head+tail (less head-only) |
| compute | 2–14 µs | — |

Boot and per-query cost stay ~constant as the corpus grows; size lives in the
postings (≈0.4 bytes per trigram-document incidence — roaring is near-optimal),
so the lever for a smaller index is indexing less text per doc, not the encoding.

The Rust builder reaches the **47.8M-work** OpenAlex corpus that backed the
earlier demo — an 11.5 GB index of 30.3M unique trigrams, built in ~57 min at
~52 GB peak RAM with no swap — and **sublinearly** (≈half the naive linear
projection), as the trigram vocabulary saturates and roaring absorbs the added
postings. The current **484M**-work demo index is 113 GB over 114.6M trigrams,
built by the chunked/phased builder. With facets and records attached, a full
boot is a few MB: the index sparse, the facet metadata + top-category heads
(the facet *tails* stay range-fetched, not loaded up front), and the
record-store header.

## Costs — server default vs. client-side range reads

The unit of cost is **bytes moved per query** — the demo's link is bandwidth-bound
(measured ~1–3.5 MB/s down, ~150–200 ms RTT), so per-query bytes, not CPU, set wall
time. Storage is nearly free: the full 484M index family — trigram monolith + term +
vector + records + facet sidecars + the geometric trigram (19-tier) and term (12-tier)
split sets + the BM25 `.rrb` sidecar (plus the legacy flat split set still parked on S3)
— is ~550 GB ≈ **~$13/month**, and there is **no idle cost**: nothing runs when nobody
searches. The demo **defaults to a regional `/search` Lambda** for trigram (~1–3 KB,
~0.66 s, faceting included); the client-side range-read modes — searchable with *no
backend at all* — are one toggle away, and that's where bytes climb:

| mode (484M, warm) | per query | one-time resident boot |
|---|---|---|
| **trigram — server (default)** | **~1–3 KB** | none |
| trigram — client geo split | ~240–300 KB | ~1.5 KB manifest |
| trigram — client monolith | ~0.87–1.07 MB | ~1.7 MB |
| term — client geo split | ~40–270 KB | ~1 KB manifest |
| term — client monolith | ~15–20 KB | **~76 MB** (resident dict) |
| semantic (8 IVFPQ probes) | ~2–9.6 MB | **~64 MB** (vector + embedder) |
| records page (25 cards) | ~25–50 KB | — |
| client facet filter (membership) | ~tens of KB | — |
| server facet filter | ~2 KB (+ exact counts) | none |

Client bytes drop via **pruning** (read only the tiers that can match) and geometric
tiering caps a worst-case descent at ~log-many visits. Faceting is cheap client-side too:
the demo post-filters the ranked candidates with a **membership** read of the selected
category — only the 64K-doc buckets the candidates occupy (container-granularity seeks on
the `.rrf`, the same offset-table seek the trigram tail scan uses), not the whole category
bitmap — so a category that runs to tens of MB whole costs ~tens of KB here, and counts
come from the resident facet heads with no fetch. The server path's facet edge is **exact
totals + counts**, not bytes. Behind a CDN with a real free tier (CloudFront:
1 TB + 10M req/mo), the monthly bill — baselined on the **server default** (~2 KB/query)
and a representative **client** query (geo split, ~0.3 MB / ~30 range-GETs):

| queries/mo | server default (this) | client range reads | always-on box | managed search |
|---|---|---|---|---|
| 10k–100k | **~$13** (inside free tier) | ~$13 | ~$100–150 flat | $700+ |
| 1M | **~$20** | ~$30–50 | ~$100–150 | $700+ |
| 10M | **~$110–150** | ~$300–400 | ~$150+ | $1,500+ |

Honest conclusions:

- **Below a few hundred thousand queries a month, nothing is cheaper — or lower-ops.**
  Server or client, you're inside the CDN free tier with no box to babysit, and the
  artifacts are immutable (the demo still works untouched years later). In the in-browser
  semantic mode the query text never leaves the browser at all.
- **For interactive latency on a modest connection, the server path wins on the heavy
  modes** — KB instead of the client trigram-monolith's ~1 MB or semantic's MBs, and no
  64–76 MB resident boots; faceting is cheap either way (membership reads), so the server's
  facet edge is exact counts, not bytes. The client-side range-read modes
  ([`examples/search-lambda`](examples/search-lambda) is the server side) stay to
  demonstrate the *no-backend* story and its tradeoffs, not as the speed default.
- **The sweet spot is large corpus × modest traffic, and it widens as the corpus shrinks.**
  Per-query bytes scale with the corpus, so a smaller index makes the client-side path cheaper
  and the CDN free tier go further; the demo runs the full 484M corpus.

The two paths compose — the same artifacts serve both — so the demo's production shape is
now **server-side by default** with the client-side range-read modes as the no-backend
option. The dictionary records every posting's byte size, so a client can estimate a
query's cost *before fetching anything* and auto-route the expensive ones to the Lambda.

## Tried and shelved

Experiments the data argued against — recorded so we don't relitigate them.

### Inverting common-trigram postings

Idea: a very common trigram has a huge posting, but its *complement* (the docs
that lack it) is small — so store the complement plus a one-bit "inverted" flag
and `ANDNOT` it during intersection, cutting the bytes a query fetches.

Why it didn't pan out: roaring stores each 65,536-doc block as either an array
(≤4096 docs, 2 B each) or a flat **8 KB bitmap** for any cardinality in
(4096, 61440]. A common trigram's complement usually lands in that same band, so
it is *also* an 8 KB bitmap — inversion only shrinks a block denser than ~94%
(complement becomes a small array, or empty). Measured on the earlier 47.8M index
(`rust/examples/density`, e.g. `cargo run --release --example density -- "machine learning" "posthuman became"`):
the hottest trigram, `the`, is only ~52% dense — OpenAlex lacks an abstract for
roughly half its works, so half the corpus is title-only text — and just
**0.0–0.6%** of posting bytes sit in >94%-dense blocks. Net saving across those
queries: **~0.1%**. Not worth a format-version bump + reindex.

It *would* pay on a corpus where common trigrams are near-universal (full text
with an abstract on every doc); here it's the partial abstract coverage that
flattens the density. The `density` example re-runs the analysis on any
index/query.

### Rarest-trigram candidates + verify

Idea: skip the common trigrams' multi-MB postings entirely — seed candidates by
intersecting only the *rarest* trigrams, then verify each against its record's
stored text (title + abstract + authors + venue), keeping the true matches.

Why it didn't pan out: client-side, egress is floored by the **result-set size**.
The candidate set can't shrink below the number of results, and verifying that
many records costs ~`result_count × record_size`. Measured on the earlier 47.8M
index (`rust/examples/candidates`): `machine learning` (171k results) still has
~195k candidates after seeding the 4 rarest trigrams → ~190 MB of record
verification, *worse* than the 53 MB full intersection. It helps only sparse
results (`posthuman became`, 317 → ~12 MB vs 30 MB), which the lazy tail already
gates behind an explicit load. `Index::search_candidates` and the `candidates`
example stay for re-measuring.

Returning *just result IDs* needs the intersection to run server-side (a thin
Lambda@Edge over in-region postings) — the one lever that beats the result-set
floor.

## Vector / similarity search (optional)

Beyond trigram text search, the crate has a **range-fetchable similarity index**
in the same ethos: an `RRVI` (IVFPQ) file whose coarse centroids and PQ codebooks
boot once, after which each query range-fetches only the `nprobe` nearest
clusters' codes and scans them with asymmetric distance computation — top-k
nearest vectors in ~constant bytes, independent of corpus size. `vector_id ==
doc_id`, so a hit reuses the same record store and can hybridize with the trigram
result set.

Build it from Rust (`build_ivfpq`, behind the `vector` feature) or Python
(`VectorBuilder`), or train at scale with FAISS and export the same layout
(`build_ivfpq_from_parts` / `roaringrange.write_rrvi_from_faiss`, verified against
the reader at recall@10 ≈ 0.9995). The reader [`VectorIndex`](rust/src/vector.rs)
is pure Rust with a browser binding (`RrviIndex`, `wasm-pack build --features
"wasm vector"`). See [`VECTORS.md`](VECTORS.md). Live on the demo: the in-browser
model2vec query *embedder* (`RrviIndex` + `Model2vecEmbedder`) and term/trigram hybrid
(reciprocal-rank fusion); an optional EmbeddingGemma Lambda embedder is parked
([`tasks/004_vector_search`](tasks)).

## Development

Enable the formatting pre-commit hook (runs `gofmt` + `cargo fmt --check` on
staged changes, matching CI):

```sh
git config core.hooksPath .githooks
```

CI runs `go test ./...` in `go/`, the `go/conformance/` module, `go vet` on the
example, `cargo test` + `fmt` + `clippy` for the reader (a second pass with
`--features vector`), and builds + tests the Python extension on CPython
3.12–3.14.

## License

MIT — see [LICENSE](LICENSE).
