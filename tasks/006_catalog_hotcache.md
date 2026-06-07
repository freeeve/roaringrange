# Task 006 — roaringrange catalog hotcache (RRHC)

**Status:** scoping (pending). Planned 2026-06-04.

A **new, additive** member of the roaringrange family — not another *index*, but a
**cross-format boot accelerator**. Every roaringrange format already boots a tiny
resident descriptor that front-loads "which byte range to fetch next"; the hotcache is
that per-structure idea generalized **across** formats: one small artifact that boots a
whole *composition* (trigram + terms + facets + records + vector + lookup + embedder) in
**1–2 round trips** instead of N separate cold opens. **It replaces nothing.** The
individual format files are untouched; per-query reads are unchanged. Apps that boot one
index keep doing exactly what they do today; apps that compose several opt into the
hotcache to collapse the boot.

Same ethos as the rest of roaringrange: a static artifact on S3 (here, fronted by a CDN),
booted with a few small range reads, with the bulk range-fetched only as a query needs it.

---

## 0. Positioning — the boot descriptor, one level up

The unifying observation: **every roaringrange format already ships a small resident boot
descriptor whose whole job is to front-load "which byte range comes next."** It is the
same shape in each format, just keyed differently:

| Format | Resident boot region | What it front-loads |
|---|---|---|
| Trigram `RRS` (`FORMAT.md`) | 20 B header + sparse index (tens of KB) | sparse keys → which dict block to range-read |
| **Term `RRTI` (`TERMS.md`)** | 32 B header + **whole FST blob** | **term → `(head_off, head_size)` at 0 fetches** — the purest case |
| Facet `RRSF` (`FACETS.md`) | 24 B header + field/category tables + string blob | `(field, category)` → `(headOff, headSize, tailSize)` |
| Vector `RRVI` (`VECTORS.md`) | 48 B header + centroids + codebooks + cluster directory (a few MB) | centroid match → which cluster's code-list range to read |
| Record `RRSR` (`RECORDS.md`) | 16 B header (+ `.dict` sidecar) | offsets array → `bin[off[d]..off[d+1]]` |
| Lookup `RRIL` | 16 B header + resident map | id → doc |

The RRTI FST is the purest expression: **term → posting offset at zero S3 GETs** — the
boot descriptor *is* the dictionary. The others are the same instinct with a sparse index
or a directory standing in for the full dictionary. In every case the boot region is
**static** metadata: it does not depend on the query, only on the artifact.

**The gap.** Composing N formats means N separate **cold** boots. The OpenAlex demo's
`boot()` (`examples/openalex/web/index.html`, ~line 1690) does exactly this — count them:

1. `RrsIndex.open(RRS_URL)` — trigram header + sparse index
2. `idx.openFacets(RRF_URL)` / `RrfFacets.open` — facet sidecar meta
3. `fetch(DICT_URL)` then `RrsRecords.openWithDict(IDX_URL, BIN_URL, dict)` — record `.dict` + `.idx` offsets header
4. `RrsLookup.open(LOOKUP_URL)` — DOI lookup header + map
5. `RrviIndex.open(RRVI_URL)` — vector centroids + codebooks + directory
6. `Model2vecEmbedder.open(RRM2_URL)` — model2vec matrix (full GET)

That is **~6 separate cold opens**, several of them serialized (records waits on its dict;
the perf bar groups every one under **"Startup · ran once"** — that group renders this
exact multi-boot problem). Each open is at least one round trip to **the far side of a
CDN** — a higher-latency hop than even Quickwit's S3-backed `open`, because edge cache
misses still walk back to origin. Six descriptors, six cold latencies, when the bytes
involved are individually tiny and *known at build time*.

**The hotcache is the cross-format generalization** of the per-structure boot descriptor:
front-load all N boot regions into one resident artifact, so booting a composition costs
the same one-to-two waves a single format costs today.

---

## 1. What can — and cannot — be front-loaded

The boot regions above share one property that makes this safe: they are **static**. A
format's boot descriptor is a function of the *artifact*, not of any query. So it can be
precomputed at build time and concatenated.

**What the hotcache front-loads (all of it static):**
- every member's fixed header,
- every member's resident dictionary / sparse index / directory / FST / facet tables,
- the record store's offset-index header and `.dict` sidecar,
- the vector centroids + codebooks + cluster directory.

**What it deliberately does not — and cannot — front-load: data-dependent fetches.** A
record-store read depends on *which doc IDs a query returned*; a posting tail fetch
depends on *whether the head underflowed K*; a probed cluster's code list depends on
*which centroids the query vector landed near*. None of these is knowable at build time, so
none is cacheable in a static boot artifact. **This is the same limit Quickwit has:** its
hotcache front-loads split metadata (the term dictionary's index, postings offsets, fast-
field codecs), not the postings a given query will touch. The hotcache front-loads only
**static boot metadata — which is exactly what all of our boots already are** — and leaves
the per-query reads untouched. That boundary is the whole reason this is sound.

---

## 2. Tier 1 (recommended first) — the manifest bundle (`.rrhc`)

A single small **`.rrhc`** file = a header + a **manifest** + the **inlined small boots**.

- **Manifest** — one entry per member of the composition: a **type tag** (`RRS` / `RRTI` /
  `RRSF` / `RRVI` / `RRSR-idx` / `RRSR-bin` / `RRSR-dict` / `RRIL` / `RRM2`), the **data-
  file name** (or URL) the member's per-query reads still go to, the member's **boot byte-
  range within that data file**, and an **inlined-here vs fetch-by-range** flag.
- **Inlined small boots** — the bytes of the SMALL boot regions, copied into the `.rrhc`
  itself: the headers, the sparse indexes, the **FST**, the facet field/category tables +
  string blob, the record offset-index header, the `.dict`. These come back **free** with
  the single GET that fetches the manifest.
- **Referenced large boots** — for the few members whose boot region is too big to inline
  (the RRVI centroids are ~16–34 MB depending on `nlist`/`D`), the manifest carries only
  the type tag + data-file name + `(offset, len)` of the boot region in that file. The
  reader fetches these in **one parallel wave** after parsing the manifest.

**`Catalog::open_hotcache(url)` boots any composition in 1–2 round trips:**
1. **one GET** of the `.rrhc` → header + manifest + every inlined small boot, all resident
   immediately (trigram sparse index, FST, facet tables, record offsets header, `.dict`,
   lookup map — *all free*);
2. **one parallel wave** of the few LARGE boots referenced by range (e.g. RRVI centroids).
   A composition with no large boot (trigram + terms + facets + records + lookup) finishes
   in step 1 — a **single round trip**.

Crucially, **data stays in the separate format files.** The `.rrhc` holds only boot
metadata + small boot bodies; the `.rrs`, `.rrt`, `.rrf`, `.rrvi`, `.idx`/`.bin`, `.rril`
files are unchanged and **per-query range reads hit them exactly as today**. The hotcache
accelerates *boot only*; it is transparent to the search path. (Members keep their
existing readers; `open_hotcache` just hands each reader its pre-fetched boot bytes instead
of letting it issue its own cold GET.)

### 2.1 The inline-vs-reference threshold is the FST inline-rare-postings instinct, one level up

Tier 1's central knob — *inline a member's boot if it's smaller than the cost of going to
fetch it; otherwise leave a range pointer* — is **exactly** the RRTI move from
`tasks/005` §6.1 (inline a rare term's posting when it's smaller than the `(offset,len)`
pointer to it), lifted one level. There it was "term posting vs pointer to the posting";
here it is "member boot region vs pointer to that boot region." Same head/tail instinct —
*give away the small thing for free, spend a fetch only on the big thing* — applied at the
catalog↔member boundary instead of the dictionary↔posting boundary. Worth naming because
it means the threshold has a known-good precedent and tuning discipline already in the
library.

### 2.2 Browser cache = Quickwit's "hotcache in RAM"

Quickwit keeps each split's hotcache resident in RAM so a warm searcher skips the metadata
fetch entirely. The browser gives us the same thing for free: ship the `.rrhc` with a
**content-hashed, immutable** URL (`catalog-<hash>.rrhc`, `Cache-Control: immutable`). The
first visit GETs it once; **every warm visit boots from the HTTP cache with zero network**
— the entire composition's boot metadata served from local disk/RAM. The content hash also
makes it self-versioning: a rebuilt composition is a new URL, so there is no stale-cache
problem and no invalidation step. (The CDN edge does the same caching for *cold* visitors
who share an edge: one origin fetch warms the edge for the region.)

---

## 3. Tier 2 (endgame) — the Quickwit split (`.rrsplit`)

The manifest bundle still leaves the *data* in N files (N per-query URLs). The endgame is
Quickwit's actual move: **concatenate every member into one immutable object with a footer
hotcache.**

- **`.rrsplit`** = `[ member 1 body ][ member 2 body ]…[ member N body ][ FOOTER hotcache ][ trailer ]`.
- The **trailer** (fixed bytes at end-of-file) gives the FOOTER's `(offset, len)`.
- The **FOOTER hotcache** = the Tier-1 manifest + inlined small boots, but now every
  member's per-query offsets are **rebased** to absolute positions within the single
  `.rrsplit`. There are no member URLs — only member *spans*.
- **Boot:** read the trailer (one tiny ranged read of the last few bytes) → read the
  FOOTER (one ranged read) → resident. **Per-query reads are range reads *within the one
  file*** at rebased offsets. This is exactly Quickwit's `read trailer → read hotcache →
  range-read the split` path.

**Why this is natural for us, where it is heavy for a mutable engine:**
- **One S3 object, atomic.** The whole composition is one immutable artifact — atomic to
  publish, atomic to roll back, one cache entry, one content hash. (Compare today's 6+
  files that must all be consistent.)
- **Offsets rebased per member** is a pure build-time concatenation: each member already
  emits a self-contained body with relative offsets; rebasing is `body_start[i] +
  relative_off`. The member readers don't change — they just receive an absolute base.
- **The immutable-rebuild cost is already paid.** Tier 2's one objection is that any change
  rewrites the whole split. But **roaringrange formats are *already* immutable static
  artifacts** — a content change already means a full rebuild + re-upload of that format
  file. Concatenating them costs nothing extra over what an immutable-artifact pipeline
  already does; there is no in-place update to give up because there never was one. This is
  the asymmetry that makes the Quickwit split *cheaper* for us than for a mutable engine:
  we don't pay the rebuild penalty Quickwit's LSM-style segment merges are designed to
  amortize, because our segments don't merge — they're rebuilt wholesale anyway.

Tier 2 is the destination; Tier 1 is the same win with zero changes to how data files are
laid out, so it ships first and de-risks the FOOTER work.

---

## 4. Reader / builder API sketch

**Reader (wasm-safe, mirrors `Catalog`):**

```rust
// rust/src/hotcache.rs  (behind the `hotcache` feature; reader wasm-safe)

/// A parsed RRHC manifest: which members exist, where each one's data file is,
/// and the inlined boot bytes (resident) vs a range to fetch (large boots).
pub struct Hotcache { /* header + manifest entries + inlined-boot blob */ }

impl Hotcache {
    /// One GET of the `.rrhc`: header + manifest + all inlined small boots.
    pub async fn open<F: RangeFetch>(rrhc: F) -> Result<Hotcache, HotcacheError>;

    /// The members present, in manifest order (type tag + data-file name/url).
    pub fn members(&self) -> &[Member];
    /// The inlined boot bytes for a member, if it was inlined (else None → fetch by range).
    pub fn inlined(&self, m: &Member) -> Option<&[u8]>;
}

impl<F: RangeFetch + Clone> Catalog<F> {
    /// Boot an entire composition from one `.rrhc` in 1–2 round trips.
    /// `data` resolves a member's data-file name → a `RangeFetch` over that file
    /// (so per-query reads keep going to the real `.rrs`/`.rrf`/`.bin`/… files).
    /// Step 1: one GET parses the manifest + hands each member its inlined boot.
    /// Step 2: one parallel wave fetches the few range-referenced large boots.
    pub async fn open_hotcache(
        rrhc: F,
        data: impl Fn(&Member) -> F,
    ) -> Result<Self, IndexError>;
}
```

Each member reader gains a *boot-from-bytes* constructor (e.g. `Index::from_boot(bytes,
data_fetch)`, `FacetIndex::from_boot(...)`, `VectorIndex::from_boot(...)`) so the catalog
can inject pre-fetched boot bytes instead of letting the reader issue its own cold open.
For range-referenced members the catalog fetches the boot range from the member's data
file and feeds the same constructor — uniform path.

**Builder (native, like the other build writers):**

```rust
// rust/src/hotcache_build.rs  (native; behind `hotcache`)

pub struct MemberSpec {
    pub tag: MemberTag,          // RRS | RRTI | RRSF | RRVI | RRSR_IDX | RRSR_BIN | RRSR_DICT | RRIL | RRM2
    pub data_file: String,       // the per-query data file name/url
    pub boot_range: (u64, u32),  // (offset,len) of this member's boot region in data_file
    pub boot_bytes: Vec<u8>,     // the actual boot bytes (so we can decide inline vs reference)
}

/// Tier 1: emit a `.rrhc` — inline boots below `inline_threshold`, reference the rest.
pub fn write_hotcache<W: Write>(
    w: &mut W,
    members: &[MemberSpec],
    inline_threshold: u32,
) -> Result<(), HotcacheBuildError>;

/// Tier 2: concatenate member bodies into one `.rrsplit`, rebase offsets, append
/// the FOOTER hotcache + trailer.
pub fn write_split<W: Write>(
    w: &mut W,
    members: &[(MemberSpec, /* full body */ &[u8])],
    inline_threshold: u32,
) -> Result<(), HotcacheBuildError>;
```

The builder reads each already-built format file, slices its boot region (it knows each
format's boot extent from the per-format docs — e.g. RRS `20 + sparseCount*8`, RRTI `32 +
fstLen`, RRSF `metaLen`, RRVI boot region size from its header), and decides inline vs
reference per `inline_threshold`.

---

## 5. RRHC byte-layout sketch (Tier 1)

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
| reserved | u8[8] | 8 | zero padding |

**Manifest entry — 40 B each**, `memberCount` entries at offset `32`:
| field | type | bytes | notes |
|---|---|---|---|
| tag | u16 | 2 | member type (`RRS`=1, `RRTI`=2, `RRSF`=3, `RRVI`=4, `RRSR_IDX`=5, `RRSR_BIN`=6, `RRSR_DICT`=7, `RRIL`=8, `RRM2`=9) |
| flags | u16 | 2 | `bit0 = inlined-here` (else fetch-by-range) |
| nameOff | u32 | 4 | data-file name → offset into string blob |
| nameLen | u16 | 2 | data-file name length |
| pad | u16 | 2 | `0` |
| bootOff | u64 | 8 | boot region offset **within the data file** (for per-query base + range-referenced fetch) |
| bootLen | u32 | 4 | boot region length |
| inlineOff | u64 | 8 | if inlined: offset into the inlined-boot blob |
| inlineLen | u32 | 4 | if inlined: length in the inlined-boot blob (== bootLen) |
| reserved | u32 | 4 | `0` |

**String blob** — `strBytes` of UTF-8 data-file names, at `32 + memberCount*40`.
**Inlined-boot blob** — `inlineBytes`, at `32 + memberCount*40 + strBytes`: the
concatenated boot bodies of every inlined member, sliced by `(inlineOff, inlineLen)`.

**Tier 2 `.rrsplit`** reuses the same header + manifest as a FOOTER (set `flags bit0`), but
`bootOff`/data-file semantics become **absolute spans within the single file** and a
fixed-size **trailer** (`magic "RRHX"` + `footerOff u64` + `footerLen u32`) is appended at
end-of-file so the reader finds the footer with one tiny ranged read of the last bytes.

---

## 6. Phasing (steps)

1. **Tier-1 reader + builder + `Catalog` wiring.** Freeze RRHC v1 (this doc → a frozen
   `HOTCACHE.md`); `hotcache.rs` reader (`Hotcache::open` = one GET → parse manifest + slice
   inlined boots); `from_boot` constructors on each member reader; `Catalog::open_hotcache`
   (one GET + one parallel wave for large boots); `hotcache_build.rs::write_hotcache`;
   `hotcache` Cargo feature; tests + a hand-built fixture composing 2–3 members.
2. **Demo wiring.** Rebuild the OpenAlex composition's `.rrhc` in the build pipeline; replace
   the demo's 6-step `boot()` with `Catalog.openHotcache(...)` behind the existing dataset
   switch; collapse the perf bar's **"Startup · ran once"** rows into one (or two) hotcache
   rows so the panel *shows* the 6→1–2 collapse. Content-hash the `.rrhc` URL +
   `Cache-Control: immutable` so warm visits boot with zero network (§2.2).
3. **Tier-2 split.** `write_split` (concatenate bodies, rebase offsets, append FOOTER +
   trailer) + `Hotcache::open_split` (read trailer → footer → resident, range-read within
   one file); per-member readers take an absolute base. One `.rrsplit` per composition;
   measure boot vs Tier-1 and vs today.

Mirrors the RRVI/RRTI rollout: format + reader + builder first (behind a feature, reader
wasm-safe / builder native-only), then demo wiring, then the bigger structural step.

---

## 7. Open decisions to lock

- **Magic / extension:** **RRHC / `.rrhc`** for the Tier-1 bundle; **`.rrsplit`** (trailer
  magic `RRHX`) for Tier-2 (proposed).
- **Inline threshold** — the boot-region byte size below which a member is inlined vs
  referenced by range (§2.1). Proposed default: inline anything ≤ ~256 KB (covers every
  header + sparse index + FST + facet tables + record offsets + `.dict`), reference the
  RRVI centroids (MBs). Tune against real composition sizes.
- **Per-member URL vs single file:** Tier 1 keeps **N data-file URLs** (members named in
  the manifest, data unchanged); Tier 2 is **one `.rrsplit`**. Decide whether Tier 1's
  manifest names are relative paths (same-origin/CDN sibling files) or absolute URLs
  (cross-origin data), and whether `Catalog::open_hotcache`'s `data` resolver defaults to
  "sibling of the `.rrhc` URL."
- **Does the `.rrm2` embedder belong in the hotcache?** It's a model matrix, not an
  index boot region, but it's one of the 6 cold opens and is large (full GET). Proposed:
  treat it as a range-referenced member (Tier 1) / a concatenated member (Tier 2) so its
  boot collapses into the same wave — but keep it gated so a text-only composition omits it.
- **Manifest member identity** — tag-only, or tag + a content hash of the member's boot so
  the reader can detect a stale `.rrhc` paired with a freshly rebuilt data file. (Content-
  hashed immutable URLs from §2.2 mostly moot this, but cross-origin data may not be hashed.)
- **Partial / optional members** — a composition where the `.rrs` is "still uploading"
  (the demo's real case) must boot with that member absent. Decide whether the builder emits
  a manifest with optional members flagged, or whether `open_hotcache` tolerates a member
  whose data file 404s and degrades like today's `try/catch` boot.

---

## 8. Risks

- **Staleness across the boundary.** A `.rrhc` inlines a member's boot bytes *and* records
  the member's data-file boot range; if the data file is rebuilt but the `.rrhc` isn't (or
  vice versa), the inlined boot and the data disagree. Mitigate: content-hashed immutable
  `.rrhc` URLs (§2.2) + optional per-member boot-hash in the manifest (§7); Tier 2 removes
  the risk entirely (one atomic object).
- **Inlining too much → a fat first GET.** Over-inlining (e.g. a huge FST, or the RVVI
  centroids) turns the "one small GET" into a multi-MB blocking download — worse than the
  parallel wave it replaced. The inline threshold (§2.1) is the guard; measure the real
  inlined size per composition and keep large boots referenced.
- **CDN range-read support.** Tier 2 leans on HTTP Range reads of one large object through
  the CDN; some edge configs don't honor `Range` or collapse it to full-object fetches.
  Verify Range passthrough on the target CDN before committing Tier 2 (Tier 1's small boots
  are full-GET-friendly and don't depend on it).
- **Member-reader coupling.** Adding `from_boot` constructors touches every member reader's
  API; keep them purely additive (the existing `open` stays) so nothing else regresses, and
  gate the whole thing behind the `hotcache` feature so default/`vector`/`terms` builds are
  unaffected.
- **Build-pipeline ordering.** The hotcache builder must run *after* every member file is
  built (it slices their boot regions). A composition that ships members incrementally (the
  "`.rrs` still uploading" case) needs the optional-member handling of §7 or the `.rrhc`
  gets rebuilt as members land.

---

*Net:* every roaringrange format already front-loads a tiny resident "which byte range
next" descriptor — the RRTI FST is the purest (term → offset at zero GETs), the others the
same instinct with a sparse index or directory. The cost we haven't addressed is that
*composing* N formats means N separate cold boots over a CDN, which the demo's 6-step
`boot()` makes painfully literal under "Startup · ran once." The hotcache is that
per-structure boot descriptor generalized across formats: a small `.rrhc` (Tier 1) that
inlines the small boots and range-references the few large ones, booting any composition in
1–2 round trips with data left in the unchanged format files and per-query reads identical
— and, content-hashed and immutable, served from the browser cache with zero network on
warm visits (Quickwit's "hotcache in RAM," for free). Tier 2 is the endgame: concatenate
every member into one immutable `.rrsplit` with a FOOTER hotcache — one atomic S3 object,
offsets rebased per member, and the immutable-rebuild cost is one we already pay because
the formats are already immutable. The inline-vs-reference threshold is the FST's
inline-rare-postings instinct one level up, and the one thing the hotcache can't
front-load — data-dependent reads — is the same limit Quickwit has, because we only ever
front-load the static boot metadata our boots already are.

---

## Progress

### 2026-06-04 — Tier-1 RRHC format module DONE (reader + builder), behind `hotcache`
Pure-Rust, no new dependency, behind a non-default `hotcache` Cargo feature.
- **`rust/src/hotcache.rs`** (reader, wasm-safe): `Hotcache::open` does ONE ranged read
  of the whole `.rrhc` and parses header + manifest + string blob + inlined-boot blob
  resident per §5; `members()` + `inlined(&Member)` (resident boot bytes vs fetch-by-range).
  `MemberTag` enum (RRS=1..RRM2=9). Bad magic/version/truncation → IndexError.
- **`rust/src/hotcache_build.rs`** (native): `MemberSpec` + `write_hotcache(w, members,
  inline_threshold)` — inline boots ≤ threshold, reference the rest.
- **`HOTCACHE.md`** freezes the RRHC v1 layout. 7 tests (mixed inline/reference round-trip,
  threshold boundary, non-zero boot_off, malformed no-panic, bad magic, tag round-trip).
- Gates green across `terms`+`hotcache`+`vector` (72 lib tests); default unaffected;
  pre-push hook lints `hotcache` too.
- **DEFERRED** (next): `Catalog::open_hotcache` + per-format `from_boot` constructors (the
  invasive wiring that boots every member from the inlined bytes), demo wiring, and Tier-2
  `.rrsplit` (`write_split` + footer). The format + read/write substrate is in place.

### 2026-06-06 — first reader-path application SHIPPED: the split-set boot bundle (2 round trips)
The split set was the lowest-risk application of the hotcache because the Rust query path
already had the boot hook (`SplitFetcher::boot` → `Index::from_boot`); this pass took it end to
end through the wasm reader and the standalone demo. 2-round-trip design (the manifest keeps its
own GET; the bundle serves only the split boots), full vertical slice.
- **Emitter — `rust/src/splitset_bundle.rs`** (native, `splits`+`hotcache`):
  `write_splitset_bundle(w, &BuiltSplitSet, max_splits, inline_threshold)` inlines each split's
  boot region (`[0, boot_len)`) as an `RRS` member keyed by the split's data-file name; reuses
  `write_hotcache`. `max_splits` caps which top tiers are inlined (`0` = all); an over-threshold
  split is referenced and just cold-opens (graceful). New pub helper
  `index::rrs_boot_len(header)` computes a split's boot length from its 20-byte header alone (no
  full open). 3 unit tests (all-inlined query == cold; `max_splits` cap leaves the tail cold-
  opening; empty set).
- **Reader — `rust/src/wasm.rs`**: `WasmSplitResolver` now carries the `Hotcache` (`Rc`, so the
  per-search resolver clones a handle not the blob) and implements `boot()` from
  `inlined_by_name`; new `RrssIndex.openBundle(manifestUrl, baseUrl, rrhcUrl)` fetches manifest +
  `.rrhc` in **one parallel wave** (`futures::future::join`); `hasBundle()` / `bundledBootCount()`
  introspection. All hotcache bits cfg-gated so `wasm`+`splits` (no hotcache) is unchanged.
- **Demo — `examples/splitset-demo`**: `splitset_demo_data.rs` emits `index.rrhc` (under
  `hotcache`); `index.html` boots via the bundle with a `?nobundle` A/B lever and reports the
  collapse in the status line. README + build commands updated to `"splits hotcache"` /
  `"wasm splits hotcache"`.
- **Verified end-to-end** driving the real wasm over a Range-honoring server (the 400-doc demo,
  4 splits): bundle boot = manifest×2 + rrhc×2 in parallel; a query opening all 4 splits issued
  **28 requests with the bundle vs 36 without** — the bundle eliminates the 2 boot reads/split
  (header + sparse index), `0` per-split header reads vs `4` — with **identical results**.
  (Note: Python's `http.server` does NOT honor `Range`, so local verification needs a Range-
  capable server; the README now flags this.)
- Gates green: fmt; clippy on `wasm32` for `wasm splits hotcache` and `wasm splits`; lib tests
  under `splits hotcache` (90, +3 new); default build unaffected (`rrs_boot_len` is pub, no
  dead-code warning).
- **STILL DEFERRED**: `Catalog::open_hotcache` + the other members' `from_boot` (the OpenAlex
  6-cold-open demo — the bigger, more visible application), true 1-RT (inline the manifest +
  `SplitSet::from_bytes`), and Tier-2 `.rrsplit`.
