# `RRIL` — identifier exact-match index

A tiny, range-fetchable sidecar that maps a normalized **identifier string** (a DOI, ISBN, SKU,
slug, …) to the doc ID it was built with — the "look up this exact key" companion to the trigram
`RRS` "search this text" index. Part of roaringrange's à-la-carte family over one shared
rank-ordered doc-ID space (see [README](README.md#on-disk-formats)); it replaces nothing and is
opt-in.

File: `*.rril` · magic `RRIL` · reader [`lookup::Lookup`](rust/src/lookup.rs) (wasm: `RrsLookup`)
· builder [`build::write_lookup`](rust/src/build.rs) / `write_lookup_streaming`.

## Layout

All integers little-endian.

| section | bytes | contents |
|---|---|---|
| header | 16 | magic `"RRIL"`; version `u16` = 1; reserved `u16`; `count u32` = `N` records; reserved2 `u32` |
| records | `N × 16` | `[hash u64][verify u32][doc u32]`, sorted by `(hash, doc)` |

Each `(identifier, doc)` pair is normalized with `lookup::normalize_id` and **double-hashed**
(FNV-1a primary `hash` + a second `verify` hash), so the identifier strings themselves are never
stored — only the fixed-width triples. An empty normalized identifier is dropped (it can never be
looked up).

## Reader

`Lookup::open(fetch)` makes the small record table resident in one ranged read, then `get(id)`
normalizes + hashes the query the same way, **binary-searches** the `hash`, and scans the matching
run, confirming each candidate with `verify` (so a primary-hash collision can't return a wrong
doc). Matches come back in ascending doc (== rank) order. A single identifier may map to several
docs (the run); `get` returns all of them.

## Build

`write_lookup(w, &[(identifier, doc)])` for an in-memory slice, or
`write_lookup_streaming(w, iter)` for a full-corpus build — the streaming form **drops each
identifier `String` as it is consumed**, so peak memory is the `N × 16`-byte triple table, never
the strings. Both produce byte-for-byte identical output for the same pairs in the same order
(normalize → double-hash → drop empties → sort by `(hash, doc)`).

## Status

Shipped (Rust reader + builder, wasm `RrsLookup`). No Go/Python builder yet.
