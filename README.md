# roaringrange

Static, range-fetchable full-text search built on roaring bitmaps — query a
multi-million-document trigram index **in the browser, over HTTP Range requests,
with no backend.**

The index is one static file on object storage (S3/CDN). The browser fetches a
tiny sparse view once (~tens of KB), then each query pulls a few small byte
ranges — independent of corpus size. The multi-GB index is never downloaded
whole. Postings are portable RoaringBitmaps produced by
[roaringsearch](https://github.com/freeeve/roaringsearch) and copied verbatim, so
the Go writer and the Rust/WASM reader interoperate with zero re-encoding.

## Live demos

- **OpenAlex** — [openalex.evefreeman.com](https://openalex.evefreeman.com): ~9.5M
  scholarly works, citation-ranked, faceted (year/type/open-access/language/topic),
  searched entirely client-side. (Reproducible — see [`examples/openalex/`](examples/openalex).)
- **DeepLibby** — [dev.deeplibby.com](https://dev.deeplibby.com): ~9.6M library
  titles. (Proprietary data; code not in this repo.)

## How it fits together

```
build (Go):    corpus ─roaringsearch─▶ FTSR ─roaringrange.Transcode─▶ RRS (.rrs) ─▶ S3/CDN
               (optional) facets ─────────────roaringrange.WriteFacets─▶ RRSF (.rrf) ─▶ S3/CDN
browser (Rust/WASM): .rrs on CDN ─HTTP Range─▶ reader ─▶ ranked doc IDs ─▶ record store
```

Doc IDs are assigned at build time in descending popularity (citations, holdings,
…), so ascending doc ID = rank. Each posting is split into a **head** (the 65,536
most-popular docs) and a **tail** (the rest); a query ANDs the small heads to get
the ranked top-K and only fetches a tail when paginating past it. See
[`docs/format.svg`](docs/format.svg) and [`docs/search.svg`](docs/search.svg).

This is the design trade-off: ranking is baked in up front (no query-time
relevance scoring), in exchange for near-constant per-query fetch cost.

## Repository layout

| path | what |
|---|---|
| `*.go` | core Go module: `Transcode` (FTSR→RRS), `Open`/`Index` reference reader, `WriteFacets`, `NgramKeys` |
| `FORMAT.md`, `FACETS.md` | the frozen on-disk specs (`RRSI` index, `RRSF` facet sidecar) |
| `reader/` | Rust crate `roaringrange_reader` → WASM browser reader (`wasm-pack`) |
| `conformance/` | cross-library test: roaringsearch build ⇄ roaringrange read must agree |
| `examples/openalex/` | the OpenAlex demo: loader + `download.sh` + static web UI |
| `docs/` | architecture diagrams (SVG) |

The core Go module has **no dependency on roaringsearch** — it parses the `FTSR`
byte format directly. The n-gram key derivation is reproduced independently in Go
(here), Go (roaringsearch), and Rust (the reader); the `conformance/` module is
the guard that keeps all three byte-compatible.

## Quick start

**Build an index (Go):** build a roaringsearch index, save its `FTSR`, then
```go
rr.Transcode(ftsrReader, rrsWriter)            // → .rrs
rr.WriteFacets(rrfWriter, []rr.FacetField{...}) // → .rrf (optional)
```
Assign doc IDs in descending popularity before indexing so top-K is free.

**Build the browser reader (Rust → WASM):**
```sh
cd reader && wasm-pack build --target web --features wasm
```
```js
import init, { RrsIndex } from "./roaringrange_reader.js";
await init();
const idx = await RrsIndex.open("index.rrs");
await idx.openFacets("index.rrf");                  // optional
const cur = await idx.searchCursorFiltered("query", 0, ["type\tarticle"]);
const ids = await cur.page(0, 25);                  // ranked doc IDs
```

Host the `.rrs`/`.rrf` (+ your record store) on anything that supports HTTP Range
(S3 + CloudFront works well); point the reader at the URLs.

## Measured (full corpora, range-fetched)

| | DeepLibby 9.6M | OpenAlex 9.5M (with abstracts) |
|---|---|---|
| index size | 1.4 GB | 4.25 GB |
| one-time boot | ~52 KB | ~210 KB |
| typical query | tens–hundreds of KB | ~1–6 MB head+tail (less head-only) |
| compute | 2–14 µs | — |

Boot and per-query cost stay ~constant as the corpus grows; size lives in the
postings (≈0.4 bytes per trigram-document incidence — roaring is near-optimal),
so the lever for a smaller index is indexing less text per doc, not the encoding.

## License

MIT — see [LICENSE](LICENSE).
