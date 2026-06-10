# 027 — Catalog-level `.rrhc` boot bundle for the monolith family

**Status:** pending

The demo boots with 4–5 sequential ranged opens (index sparse, facet meta + heads, record
header, lookup header) — each a round trip. The `.rrhc` hotcache format already solves this
for split sets (manifest + inlined member boots in 1–2 RTTs); extend it to the monolith
catalog.

## Design

1. Builder: `write_hotcache` already takes named members; emit `openalex-full.rrhc`
   containing the `.rrs` boot region (header + sparse), the `.rrf` resident region (meta +
   top-category heads), the records `.idx` header, and the `.rril` header.
   (Members are exactly the bytes each reader's `from_boot`/open-resident path needs;
   `rrs_boot_len` and friends already compute the spans.)
2. wasm: `RrsCatalog.openBundle(rrhcUrl, …urls)` — one fetch, then `Index::from_boot` etc.;
   fall back to today's per-file opens when the bundle 404s.
3. Demo boot: try the bundle first; perf bar gets a single "Open bundle" row.
4. deploy.sh: build + upload the bundle alongside the artifacts (it's small — ~2 MB +
   facet heads).

## Acceptance

- Cold boot round trips drop from 4–5 to 1–2 (perf bar / network tab).
- Absent bundle degrades to the current path (poc/47m datasets unaffected).
