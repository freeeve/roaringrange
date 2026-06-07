# 017 — Surface the secondary index in the OpenAlex demo + Lambda

**Status:** pending (spun off from task 003 Stage 3)

The secondary-index substrate is done in task 003: the `RRSC` format + reader
(`rust/src/secondary.rs`, `rust/src/sortcols.rs`), the wasm bindings
(`RrsSecondaryIndex` / `RrsSecondaryCursor` / `RrsSortCols`), and the OpenAlex
builder integration (`examples/openalex/builder/src/secondary.rs` —
`build_secondary` emits a date-desc second `.rrs` + perm column + remapped
secondary `.rrf`). What remains is making it usable in the live demo.

## Scope

1. **Date-desc secondary artifact in the deployed build.** Wire `build_secondary`
   into the full-corpus build (`examples/openalex/ec2-full-build.sh` /
   `builder/src/phased.rs`) so the deploy ships the secondary `.rrs` + perm + `.rrf`
   alongside the primary artifacts.
2. **Relevance / Newest toggle in the web app.** `examples/openalex/web/index.html`
   currently has no secondary wiring (verified 2026-06-07). Open the secondary
   index + perm, add a sort toggle, and route paging/facet counts through
   `RrsSecondaryCursor` for the Newest path (records/facets stay keyed by primary ID
   via the perm map).
3. **Lambda server-mode** newest+filters path for the server-backed corpus.

## Acceptance

The deployed demo offers a Relevance/Newest toggle that pages newest-first with
facet filters intact, served from the secondary artifacts, with no regression to
the primary Relevance path.
