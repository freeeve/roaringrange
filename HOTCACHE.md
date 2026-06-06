# RRHC — roaring range catalog hotcache (`RRHC`, version 1)

A cross-format **boot accelerator** — an additive member of the roaringrange family (next
to the trigram `RRS`, term `RRTI`, facet `RRSF`, vector `RRVI`, record `RRSR`, and lookup
`RRIL`). It is **not** another index: it front-loads the small *boot regions* of a whole
composition into one small artifact, so booting a composition costs **one** ranged read
instead of N separate cold opens. It replaces nothing — the per-query data files are
untouched and per-query reads are unchanged.

Every roaringrange format already ships a tiny resident boot descriptor whose whole job is
to front-load "which byte range comes next" (the RRTI FST is the purest: term → posting
offset at zero fetches). Composing N formats means N cold boots over a CDN. The hotcache is
that per-structure boot descriptor generalized **across** formats: one `.rrhc` that inlines
the small boots and range-references the few large ones (the RRVI centroids), booting any
composition in 1–2 round trips. Content-hashed and immutable, it is served from the browser
cache with zero network on warm visits. See `tasks/006_catalog_hotcache.md` for the design.

All integers little-endian.

## Layout

`[ header ][ manifest entries ][ string blob ][ inlined-boot blob ]`

**Header — 32 B**
| field | type | bytes | notes |
|---|---|---|---|
| magic | char[4] | 4 | `"RRHC"` |
| version | u16 | 2 | `1` |
| flags | u16 | 2 | reserved (`0`) — future: `bit0 = split-footer` (Tier 2 reuse) |
| memberCount | u32 | 4 | number of members in the composition |
| strBytes | u32 | 4 | length of the string blob (data-file names) |
| inlineBytes | u64 | 8 | length of the inlined-boot blob |
| reserved | u8[8] | 8 | zero padding to 32 B |

**Manifest entry — 40 B each**, `memberCount` entries at offset `32`:
| field | type | bytes | notes |
|---|---|---|---|
| tag | u16 | 2 | member type (`RRS`=1, `RRTI`=2, `RRSF`=3, `RRVI`=4, `RRSR_IDX`=5, `RRSR_BIN`=6, `RRSR_DICT`=7, `RRIL`=8, `RRM2`=9, `RRSS`=10) |
| flags | u16 | 2 | `bit0 = inlined-here` (else fetch-by-range) |
| nameOff | u32 | 4 | data-file name → offset into the string blob |
| nameLen | u16 | 2 | data-file name length in bytes |
| pad | u16 | 2 | `0` |
| bootOff | u64 | 8 | boot region offset **within the data file** (per-query base + range-referenced fetch) |
| bootLen | u32 | 4 | boot region length in bytes |
| inlineOff | u64 | 8 | if inlined: offset into the inlined-boot blob (else `0`) |
| inlineLen | u32 | 4 | if inlined: length in the inlined-boot blob (`== bootLen`; else `0`) |
| reserved | u32 | 4 | `0` |

**String blob** — `strBytes` of UTF-8 data-file names, at `32 + memberCount*40`. Each
member's name is `stringBlob[nameOff .. nameOff + nameLen]`.

**Inlined-boot blob** — `inlineBytes` bytes, at `32 + memberCount*40 + strBytes`: the
concatenated boot bodies of every inlined member. An inlined member's boot bytes are
`inlinedBlob[inlineOff .. inlineOff + inlineLen]`.

## Inline vs reference

A member is **inlined** when its boot region is `bootLen <= inlineThreshold`: the entry's
`bit0` flag is set and the boot bytes are copied into the inlined-boot blob, so they come
back **free** with the single GET that fetches the `.rrhc`. A member whose boot is larger
is **referenced**: the entry carries only the type tag, data-file name, and
`(bootOff, bootLen)` of the boot region in that file, and the reader fetches it in a later
parallel wave from the member's own data file. This is the RRTI inline-rare-postings
instinct one level up — give away the small boot for free, spend a fetch only on the big
one. Proposed default threshold: ~256 KB (covers every header + sparse index + FST + facet
tables + record offsets + `.dict`), referencing the RRVI centroids (MBs).

## Reader
- **boot:** one ranged read of the header (32 B) pins the section sizes, then one ranged
  read of the remaining `memberCount*40 + strBytes + inlineBytes` bytes makes the whole
  `.rrhc` resident — manifest, string blob, and inlined-boot blob in memory.
- **members():** the parsed manifest entries, in order — each with its tag, data-file name,
  `(bootOff, bootLen)`, and inlined flag.
- **inlined(member):** for an inlined member, the resident boot bytes
  `inlinedBlob[inlineOff .. inlineOff + inlineLen]` (exactly `bootLen` bytes); for a
  referenced member, `None` — the caller fetches `(bootOff, bootLen)` from the member's
  data file.

The hotcache itself issues only the one GET. Range-referenced large boots are fetched by
the caller (a future `Catalog::open_hotcache`) from each member's data file, not from the
`.rrhc`; per-query reads keep going to the unchanged `.rrs` / `.rrt` / `.rrf` / `.rrvi` /
`.idx`/`.bin` / `.rril` files.

## Tier 2 — `.rrsplit` (deferred)

The Tier-2 endgame concatenates every member body into one immutable `.rrsplit` with a
**FOOTER** hotcache (the same header + manifest, with `flags bit0` set) whose `bootOff`
values become **absolute spans within the single file**, plus a fixed **trailer**
(`magic "RRHX"` + `footerOff u64` + `footerLen u32`) appended at end-of-file so the reader
finds the footer with one tiny ranged read of the last bytes. Not implemented in v1 — see
`tasks/006_catalog_hotcache.md` §3.

## Status
v1: the Tier-1 manifest bundle. Native builder `hotcache_build::write_hotcache`; reader
`hotcache::Hotcache` (wasm-safe), both behind the non-default `hotcache` Cargo feature
(pure Rust, no new dependency). Deferred: `Catalog::open_hotcache` wiring and the
per-member `from_boot` constructors, and Tier 2's `write_split` / `.rrsplit` footer. See
`tasks/006_catalog_hotcache.md`.
