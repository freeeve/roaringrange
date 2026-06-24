//! Reader for the optional facet sidecar (`RRSF`). See `FACETS.md`.
//!
//! Boot reads the compact meta region (header + field table + category table +
//! string blob) plus the contiguous head-postings region, keeping both in
//! memory. Listing fields/categories with their full-corpus counts is then free,
//! and [`FacetIndex::counts`] computes search-filtered counts in memory. A
//! selection is resolved to a [`ResolvedFilter`] whose category postings are
//! range-fetched and ANDed into a query (within-field OR, across-field AND).
//!
//! Loading all head postings suits a small facet sidecar (the DeepLibby sidecar
//! is ~9 MB). A very large facet set would instead range-fetch counts per field.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, CatRange, IndexError, ResolvedFilter};
use roaring::RoaringBitmap;
use std::collections::BTreeMap;

/// `RRSF` magic.
const MAGIC: &[u8; 4] = b"RRSF";
/// Header size in bytes.
const HEADER_SIZE: usize = 24;

/// FNV-1a 64-bit offset basis / prime (the facet key derivation).
const FNV_OFFSET64: u64 = 14695981039346656037;
const FNV_PRIME64: u64 = 1099511628211;

/// Derives the facet category key: FNV-1a 64-bit over `lower(field)`, a `0x1f` separator byte,
/// then `lower(category)`. Mirrors Go `FacetKey` (see `FACETS.md`). Used by the build side
/// (`RRSF` category table) and by the split-set reader's facet-presence pruning, so it lives
/// here in the wasm-safe reader rather than the native builder.
pub(crate) fn facet_key(field: &str, category: &str) -> u64 {
    let mut h = FNV_OFFSET64;
    for b in field.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME64);
    }
    h ^= 0x1f;
    h = h.wrapping_mul(FNV_PRIME64);
    for b in category.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME64);
    }
    h
}
/// Field-table entry size: nameOff(4) + nameLen(2) + pad(2) + catStart(4) + catCount(4).
const FIELD_ENTRY: usize = 16;
/// Category-table entry size: key(8) + headOff(8) + headSize(4) + tailSize(4) +
/// cardinality(4) + nameOff(4) + nameLen(2) + pad(2).
const CAT_ENTRY: usize = 36;

/// Postings-region size (bytes) at or below which the whole region is read in one
/// request at boot. Above it the tails (which dominate the file and are only
/// needed for filtered tail pagination) are skipped, and only the top categories'
/// heads per field are fetched — keeping boot small for a large sidecar.
const EAGER_REGION_LIMIT: usize = 24 * 1024 * 1024;
/// Number of highest-count category heads loaded per field for a large sidecar.
/// Covers what a UI shows (the top categories by frequency); the rest report a
/// filtered count of zero (their full-corpus counts still come from the meta).
const LAZY_TOP_N: usize = 128;

/// One category value: its display name, full-corpus document count, posting
/// location, and (for filtered counts) its in-memory head posting.
#[derive(Clone)]
pub struct Category {
    /// Display name (e.g. `"english"`, `"audiobook"`).
    pub name: String,
    /// Full-corpus document count (the unfiltered facet count).
    pub count: u32,
    range: CatRange,
    /// Head posting (docs `[0,65536)`) held in memory so `counts` can intersect
    /// it with a query's head result without further fetches.
    head: RoaringBitmap,
}

/// One facet field with its categories.
pub struct Field {
    /// Field name (e.g. `"language"`, `"format"`).
    pub name: String,
    /// The field's categories, in stored order.
    pub categories: Vec<Category>,
}

/// One facet selection: a `(field, category)` pair, optionally **negated** to
/// EXCLUDE the matching docs instead of including them. Includes OR within a
/// field and AND across fields (the positive set `P`); excludes union across all
/// fields and are subtracted (`P ANDNOT X`), so a doc in any excluded category is
/// dropped. Build with [`FilterSel::include`] / [`FilterSel::exclude`], or from a
/// `(field, category)` tuple (which defaults to include).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSel {
    /// Facet field name (e.g. `"format"`).
    pub field: String,
    /// Category value within the field (e.g. `"Short Stories"`).
    pub category: String,
    /// When `true`, EXCLUDE docs in this category instead of including them.
    pub negate: bool,
}

impl FilterSel {
    /// An include selection — keep docs in this `(field, category)`.
    pub fn include(field: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            category: category.into(),
            negate: false,
        }
    }

    /// An exclude selection — drop docs in this `(field, category)`.
    pub fn exclude(field: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            category: category.into(),
            negate: true,
        }
    }
}

impl<S: Into<String>, T: Into<String>> From<(S, T)> for FilterSel {
    /// A bare `(field, category)` tuple is an include selection.
    fn from((field, category): (S, T)) -> Self {
        Self::include(field, category)
    }
}

/// A range-fetchable facet sidecar. Holds the meta region in memory.
pub struct FacetIndex<F: RangeFetch> {
    fetch: F,
    /// The facet fields, in stored order.
    fields: Vec<Field>,
}

/// A parsed, fetchless `RRSF` meta region — the bundle-boot intermediate:
/// [`FacetIndex::from_boot`] produces it with zero fetches, and
/// [`attach`](Self::attach) binds the per-query fetcher.
pub struct FacetMeta {
    fields: Vec<Field>,
}

impl FacetMeta {
    /// Parses a **resident** meta region (the `[0, rrsf_boot_len)` bytes: header +
    /// field/category tables + string blob, e.g. an `RRHC` bundle member) with
    /// zero fetches — the boot-bundle path. Full-corpus counts and filtering work
    /// from this alone; call [`FacetIndex::load_heads`] (typically off the boot
    /// critical path) before search-filtered counts report non-zero.
    pub fn parse(meta: Vec<u8>) -> Result<FacetMeta, IndexError> {
        from_meta(meta)
    }

    /// Binds the per-query fetcher, producing a working [`FacetIndex`] (heads
    /// still empty until [`FacetIndex::load_heads`]).
    pub fn attach<F: RangeFetch>(self, fetch: F) -> FacetIndex<F> {
        FacetIndex {
            fetch,
            fields: self.fields,
        }
    }
}

impl<F: RangeFetch> FacetIndex<F> {
    /// Boots the facet index: reads the header + meta region (field/category
    /// tables + string blob), then loads the head postings that filtered counts
    /// intersect against. A small sidecar's whole postings region is read in one
    /// request; a large one's tails (which dwarf the heads and are only needed for
    /// filtered tail pagination) are skipped — only the top categories' heads per
    /// field are fetched, so boot stays small.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        Self::open_tuned(fetch, EAGER_REGION_LIMIT, LAZY_TOP_N).await
    }

    /// Boots the **meta region only** (header + tables + string blob, KBs) —
    /// no head postings. Sufficient for [`resolve`](Self::resolve)/filtering and
    /// full-corpus counts; search-filtered [`counts`](Self::counts) read zero
    /// until [`load_heads`](Self::load_heads). The per-split filtered path uses
    /// this: filtering never touches heads, so eagerly loading a sidecar's whole
    /// postings region per split (the `open` default) is pure waste there.
    pub async fn open_meta(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        let meta_len = rrsf_boot_len(&header)?;
        let buf = fetch.read(0, meta_len).await?;
        Self::from_boot(buf, fetch)
    }

    /// Boots from already-fetched meta-region bytes (`meta`, the boot region a
    /// bundle or hotcache inlines) paired with `fetch` for later head/tail reads —
    /// the zero-extra-fetch constructor, mirroring [`Index::from_boot`](crate::Index::from_boot)
    /// and [`Lookup::from_boot`](crate::Lookup::from_boot). Equivalent to
    /// [`open_meta`](Self::open_meta) without its two header/region reads. The
    /// underlying [`FacetMeta::parse`] + [`FacetMeta::attach`] remain available for
    /// callers that already hold a parsed [`FacetMeta`].
    pub fn from_boot(meta: Vec<u8>, fetch: F) -> Result<Self, IndexError> {
        Ok(FacetMeta::parse(meta)?.attach(fetch))
    }

    /// The facet fields, in stored order. The per-field category counts a search
    /// returns ([`counts`](Self::counts)) align positionally with this slice.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// [`FacetIndex::open`] with explicit tuning: `eager_limit` is the
    /// postings-region byte size at or below which the whole region is read in one
    /// request; above it only the `top_n` highest-count category heads per field
    /// are fetched. Exposed for tests.
    pub(crate) async fn open_tuned(
        fetch: F,
        eager_limit: usize,
        top_n: usize,
    ) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        let meta_len = rrsf_boot_len(&header)?;
        let buf = fetch.read(0, meta_len).await?;
        let mut fi = Self::from_boot(buf, fetch)?;
        fi.load_heads_tuned(eager_limit, top_n).await?;
        Ok(fi)
    }
}

/// Parses the resident meta region from `buf` (validating it from scratch — the
/// bytes may come from a bundle rather than this file) into a fetchless
/// [`FacetMeta`]; heads are empty until [`FacetIndex::load_heads`].
fn from_meta(buf: Vec<u8>) -> Result<FacetMeta, IndexError> {
    if buf.len() < HEADER_SIZE {
        return Err(IndexError::Malformed("RRSF meta region truncated"));
    }
    let meta_len = rrsf_boot_len(&buf[..HEADER_SIZE])?;
    if buf.len() < meta_len {
        return Err(IndexError::Malformed("RRSF meta region truncated"));
    }
    let fields_n = read_u32(&buf, 8) as usize;
    let cats_n = read_u32(&buf, 12) as usize;
    let str_bytes = read_u32(&buf, 16) as usize;
    let field_tab = HEADER_SIZE;
    let cat_tab = field_tab + fields_n * FIELD_ENTRY; // checked in rrsf_boot_len
    let str_blob = cat_tab + cats_n * CAT_ENTRY;

    // Reads a display name from the string blob, rejecting an out-of-range
    // (off, len) instead of panicking on the slice.
    let read_name = |off: usize, len: usize| -> Result<String, IndexError> {
        let end = off
            .checked_add(len)
            .filter(|&e| e <= str_bytes)
            .ok_or(IndexError::Malformed("RRSF name out of string blob"))?;
        Ok(String::from_utf8_lossy(&buf[str_blob + off..str_blob + end]).into_owned())
    };

    let mut cats: Vec<Category> = Vec::with_capacity(cats_n);
    for i in 0..cats_n {
        let b = cat_tab + i * CAT_ENTRY;
        let head_off = read_u64(&buf, b + 8);
        let head_size = read_u32(&buf, b + 16);
        let tail_size = read_u32(&buf, b + 20);
        let count = read_u32(&buf, b + 24);
        let name_off = read_u32(&buf, b + 28) as usize;
        let name_len = read_u16(&buf, b + 32) as usize;
        cats.push(Category {
            name: read_name(name_off, name_len)?,
            count,
            range: CatRange {
                head_off,
                head_size,
                tail_off: head_off.saturating_add(head_size as u64),
                tail_size,
            },
            head: RoaringBitmap::new(),
        });
    }

    // Parse the field table up front; its per-field category ranges drive the
    // top-N head selection when a large sidecar is loaded.
    let mut field_spans: Vec<(String, usize, usize)> = Vec::with_capacity(fields_n);
    for i in 0..fields_n {
        let b = field_tab + i * FIELD_ENTRY;
        let name_off = read_u32(&buf, b) as usize;
        let name_len = read_u16(&buf, b + 4) as usize;
        let cat_start = read_u32(&buf, b + 8) as usize;
        let cat_count = read_u32(&buf, b + 12) as usize;
        let cat_end = cat_start
            .checked_add(cat_count)
            .filter(|&e| e <= cats_n)
            .ok_or(IndexError::Malformed(
                "facet field category range out of bounds",
            ))?;
        field_spans.push((read_name(name_off, name_len)?, cat_start, cat_end));
    }

    let fields = field_spans
        .into_iter()
        .map(|(name, start, end)| Field {
            name,
            categories: cats[start..end].to_vec(),
        })
        .collect();

    Ok(FacetMeta { fields })
}

impl<F: RangeFetch> FacetIndex<F> {
    /// Loads the head postings that **search-filtered** counts intersect against
    /// ([`counts`](Self::counts)); until it runs those counts read as zero, while
    /// full-corpus counts and filtering work from the meta alone. A small
    /// sidecar's whole postings region is read once; a large one fetches only the
    /// `top_n` highest-count heads per field — on the 484M sidecar that is
    /// hundreds of scattered small reads, which is why a bundle boot runs this
    /// off the critical path. Offsets are untrusted, so every slice bound uses
    /// checked math.
    pub async fn load_heads(&mut self) -> Result<(), IndexError> {
        self.load_heads_tuned(EAGER_REGION_LIMIT, LAZY_TOP_N).await
    }

    /// [`load_heads`](Self::load_heads) with explicit tuning (see
    /// [`open_tuned`](Self::open_tuned)).
    pub(crate) async fn load_heads_tuned(
        &mut self,
        eager_limit: usize,
        top_n: usize,
    ) -> Result<(), IndexError> {
        let first = self.fields.iter().flat_map(|f| f.categories.first()).next();
        let last = self
            .fields
            .iter()
            .rev()
            .flat_map(|f| f.categories.last())
            .next();
        // The region length stays u64 until the eager branch: casting first would
        // truncate a >4 GiB region on wasm32 and route it down the eager path with
        // a wrapped length — a valid oversized sidecar is simply "large" (lazy).
        let region_len: u64 = match (first, last) {
            (Some(first), Some(last)) => last
                .range
                .tail_off
                .saturating_add(last.range.tail_size as u64)
                .checked_sub(first.range.head_off)
                .ok_or(IndexError::Malformed(
                    "facet postings region has end < start",
                ))?,
            _ => 0,
        };
        if region_len == 0 {
            return Ok(()); // empty sidecar; nothing to load
        }
        if region_len <= eager_limit as u64 {
            let blob_start = first.unwrap().range.head_off;
            let blob = self.fetch.read(blob_start, region_len as usize).await?;
            for f in &mut self.fields {
                for c in &mut f.categories {
                    let s = c
                        .range
                        .head_off
                        .checked_sub(blob_start)
                        .ok_or(IndexError::Malformed("RRSF head offset precedes region"))?
                        as usize;
                    let e = s
                        .checked_add(c.range.head_size as usize)
                        .filter(|&e| e <= blob.len())
                        .ok_or(IndexError::Malformed("RRSF head posting out of region"))?;
                    c.head = RoaringBitmap::deserialize_from(&blob[s..e])
                        .map_err(|err| IndexError::Roaring(err.to_string()))?;
                }
            }
            return Ok(());
        }
        // Lazy top-N: per field, the highest-count heads only.
        let mut reqs: Vec<(usize, usize, u64, usize)> = Vec::new();
        for (fi, f) in self.fields.iter().enumerate() {
            let mut idxs: Vec<usize> = (0..f.categories.len()).collect();
            idxs.sort_by(|&a, &b| f.categories[b].count.cmp(&f.categories[a].count));
            for &ci in idxs.iter().take(top_n) {
                let r = &f.categories[ci].range;
                reqs.push((fi, ci, r.head_off, r.head_size as usize));
            }
        }
        let ranges: Vec<(u64, usize)> = reqs.iter().map(|&(_, _, off, len)| (off, len)).collect();
        let datas =
            crate::fetch::read_coalesced(&self.fetch, &ranges, crate::fetch::COALESCE_GAP).await?;
        for (&(fi, ci, _, _), bytes) in reqs.iter().zip(datas) {
            self.fields[fi].categories[ci].head = RoaringBitmap::deserialize_from(&bytes[..])
                .map_err(|err| IndexError::Roaring(err.to_string()))?;
        }
        Ok(())
    }

    /// Resolves `(field, category)` selections into a [`ResolvedFilter`].
    /// Selections for the same field are grouped so they OR together; distinct
    /// fields AND. A selected field whose categories all fail to resolve (an
    /// unknown field, or categories this sidecar doesn't carry — common for a
    /// per-split sidecar that lacks a globally-selected category) contributes an
    /// **empty** arm that matches nothing: the user asked for docs in that
    /// category and none exist here, which must not degrade to "unfiltered".
    pub fn resolve(&self, pairs: &[(String, String)]) -> ResolvedFilter<F>
    where
        F: Clone,
    {
        let mut by_field: BTreeMap<&str, Vec<CatRange>> = BTreeMap::new();
        for (fname, cname) in pairs {
            let ranges = by_field.entry(fname.as_str()).or_default();
            if let Some(field) = self.fields.iter().find(|f| &f.name == fname) {
                if let Some(c) = field.categories.iter().find(|c| &c.name == cname) {
                    ranges.push(c.range);
                }
            }
        }
        ResolvedFilter::new(
            self.fetch.clone(),
            by_field.into_values().collect(),
            Vec::new(),
        )
    }

    /// Resolves [`FilterSel`] selections (includes and/or excludes) into a
    /// [`ResolvedFilter`]. Include selections behave exactly like
    /// [`resolve`](Self::resolve) — grouped by field so categories within a field
    /// OR and distinct fields AND (the positive set `P`). Exclude selections
    /// (`negate`) are collected into one flat union `X` across all fields; the
    /// resolved filter yields `P ANDNOT X`, so a doc in ANY excluded category is
    /// dropped. An exclude whose category fails to resolve here simply contributes
    /// nothing to `X` (nothing to remove) — unlike a failed include arm, it does
    /// NOT make the filter match-nothing. With no includes, `P` is the whole
    /// corpus and the result is "everything except `X`".
    pub fn resolve_sels(&self, sels: &[FilterSel]) -> ResolvedFilter<F>
    where
        F: Clone,
    {
        let mut by_field: BTreeMap<&str, Vec<CatRange>> = BTreeMap::new();
        let mut excludes: Vec<CatRange> = Vec::new();
        for sel in sels {
            let resolved = self
                .fields
                .iter()
                .find(|f| f.name == sel.field)
                .and_then(|field| field.categories.iter().find(|c| c.name == sel.category))
                .map(|c| c.range);
            if sel.negate {
                if let Some(r) = resolved {
                    excludes.push(r);
                }
            } else {
                // Create the field's arm even when unresolved: a selected field
                // with no resolvable category is an empty arm that matches nothing.
                let ranges = by_field.entry(sel.field.as_str()).or_default();
                if let Some(r) = resolved {
                    ranges.push(r);
                }
            }
        }
        ResolvedFilter::new(
            self.fetch.clone(),
            by_field.into_values().collect(),
            excludes,
        )
    }

    /// An upper bound on how many docs can satisfy the facet filter, from the
    /// **resident** category counts alone (no fetch): within a field the
    /// selected categories OR (bound = sum of their counts), across fields they
    /// AND (bound = min over fields). A selected field whose categories all fail
    /// to resolve bounds the filter at 0 (it matches nothing). `None` when
    /// `pairs` is empty (no constraint).
    pub fn filter_count_bound(&self, pairs: &[(String, String)]) -> Option<u64> {
        if pairs.is_empty() {
            return None;
        }
        let mut by_field: BTreeMap<&str, u64> = BTreeMap::new();
        for (fname, cname) in pairs {
            let sum = by_field.entry(fname.as_str()).or_insert(0);
            if let Some(field) = self.fields.iter().find(|f| &f.name == fname) {
                if let Some(c) = field.categories.iter().find(|c| &c.name == cname) {
                    *sum += c.count as u64;
                }
            }
        }
        by_field.into_values().min()
    }

    /// Total byte size of the selected categories' postings (head + tail) — the
    /// client-side cost a facet filter adds to a query, priced from the resident
    /// category table with **no fetch**. Unknown selections contribute 0 (they
    /// resolve to a match-nothing arm, so nothing is fetched for them either).
    pub fn filter_cost(&self, pairs: &[(String, String)]) -> u64 {
        let mut total = 0u64;
        for (fname, cname) in pairs {
            if let Some(field) = self.fields.iter().find(|f| &f.name == fname) {
                if let Some(c) = field.categories.iter().find(|c| &c.name == cname) {
                    total += c.range.head_size as u64 + c.range.tail_size as u64;
                }
            }
        }
        total
    }

    /// Computes the per-category document counts within `result` — i.e. how many
    /// of the query's (head) results fall in each category. Returned as a vector
    /// per field, aligned with `self.fields[i].categories`. In-memory; no fetches.
    ///
    /// `result` is whatever the caller passes — for a facet-filtered search that is
    /// the post-filter head, so the counts reflect the **post-exclusion** survivors
    /// (an excluded category contributes ~0, since its docs were removed). Every
    /// category still appears in its field's list regardless of selection, so the
    /// UI can offer to toggle (un-exclude) any of them.
    pub fn counts(&self, result: &RoaringBitmap) -> Vec<Vec<u64>> {
        self.fields
            .iter()
            .map(|f| {
                f.categories
                    .iter()
                    .map(|c| result.intersection_len(&c.head))
                    .collect()
            })
            .collect()
    }

    /// Per-category document counts within `result` over each category's **full**
    /// posting — head (docs `[0, 65536)`) **and** tail (`>= 65536`). Unlike the
    /// in-memory [`counts`](Self::counts), which intersects only the resident head
    /// and so undercounts whenever `result` spans tail buckets (the search path is
    /// fine — a query's head *is* its top results — but an arbitrary corpus-spanning
    /// filtered id list is not), this fetches the tail at container granularity (only
    /// the buckets `result` spans, like the filter path's
    /// [`read_posting_subset`](crate::posting::read_posting_subset)).
    ///
    /// Because a wide sidecar can hold *hundreds of thousands* of categories (one
    /// fetch each would be ruinous over HTTP), only the top `top_per_field`
    /// categories per field — ranked by the free head-only count, a good filtered
    /// proxy since the head holds the top-ranked docs — are priced exactly; the rest
    /// (the long tail a facet UI never shows) keep their head-only count.
    /// `top_per_field == 0` prices **every** category (exact for all; for small
    /// sidecars / tests). Returned per field, aligned with
    /// `self.fields[i].categories`. Async because the tails are range-fetched.
    pub async fn counts_full(
        &self,
        result: &RoaringBitmap,
        top_per_field: usize,
    ) -> Result<Vec<Vec<u64>>, IndexError> {
        // Free head-only counts: the baseline for the unshown long tail, and the
        // rank key for which categories are worth an exact (fetched) head+tail count.
        let mut counts = self.counts(result);

        // Distinct tail buckets the result spans; bucket 0 is the head.
        let mut tail_keys: Vec<u16> = Vec::new();
        let mut head_needed = false;
        for id in result.iter() {
            let k = (id >> 16) as u16;
            if k == 0 {
                head_needed = true;
            } else if tail_keys.last() != Some(&k) {
                tail_keys.push(k);
            }
        }

        // The categories to price exactly: top `top_per_field` per field by head
        // count (all of them when `top_per_field == 0` or the field is small).
        let mut targets: Vec<(usize, usize)> = Vec::new();
        for (fi, f) in self.fields.iter().enumerate() {
            let m = f.categories.len();
            if top_per_field == 0 || m <= top_per_field {
                targets.extend((0..m).map(|ci| (fi, ci)));
            } else {
                let mut idx: Vec<usize> = (0..m).collect();
                idx.select_nth_unstable_by(top_per_field - 1, |&a, &b| {
                    counts[fi][b].cmp(&counts[fi][a])
                });
                targets.extend(idx[..top_per_field].iter().map(|&ci| (fi, ci)));
            }
        }

        let fetch = &self.fetch;
        let tail_keys = &tail_keys;
        let futs = targets.iter().map(|&(fi, ci)| {
            let c = &self.fields[fi].categories[ci];
            async move {
                // Head: the resident posting, or fetched if a sized head was not
                // loaded (large-sidecar boot keeps only top categories' heads).
                let head_n = if head_needed && c.head.is_empty() && c.range.head_size > 0 {
                    let bytes = fetch
                        .read(c.range.head_off, c.range.head_size as usize)
                        .await?;
                    let h = RoaringBitmap::deserialize_from(&bytes[..])
                        .map_err(|_| IndexError::Malformed("RRSF head posting"))?;
                    result.intersection_len(&h)
                } else {
                    result.intersection_len(&c.head)
                };
                // Tail: only the buckets the result spans (container granularity).
                let tail_n = if !tail_keys.is_empty() && c.range.tail_size > 0 {
                    let t = crate::posting::read_posting_subset(
                        fetch,
                        c.range.tail_off,
                        c.range.tail_size as usize,
                        tail_keys,
                    )
                    .await?;
                    result.intersection_len(&t)
                } else {
                    0
                };
                Ok::<(usize, usize, u64), IndexError>((fi, ci, head_n + tail_n))
            }
        });

        for r in futures::future::join_all(futs).await {
            let (fi, ci, n) = r?;
            counts[fi][ci] = n;
        }
        Ok(counts)
    }
}

/// The byte length of an `RRSF` sidecar's **boot region** — the resident meta
/// region (`[0, metaLen)`: header + field table + category table + string blob)
/// that [`FacetIndex::from_boot`] consumes. Validates the 24-byte header and
/// computes the extent with checked arithmetic (attacker-controlled counts on
/// wasm32 could otherwise wrap to a short read). A bundle builder calls this to
/// slice the boot region without opening the sidecar.
pub fn rrsf_boot_len(header: &[u8]) -> Result<usize, IndexError> {
    if header.len() < HEADER_SIZE {
        return Err(IndexError::Malformed("short RRSF header"));
    }
    if &header[0..4] != MAGIC {
        let mut m = [0u8; 4];
        m.copy_from_slice(&header[0..4]);
        return Err(IndexError::BadMagic(m));
    }
    let version = read_u16(header, 4);
    if version != 1 {
        return Err(IndexError::BadVersion(version));
    }
    let fields_n = read_u32(header, 8) as usize;
    let cats_n = read_u32(header, 12) as usize;
    let str_bytes = read_u32(header, 16) as usize;
    fields_n
        .checked_mul(FIELD_ENTRY)
        .and_then(|x| x.checked_add(HEADER_SIZE))
        .and_then(|ft| {
            cats_n
                .checked_mul(CAT_ENTRY)
                .and_then(|c| c.checked_add(ft))
        })
        .and_then(|sb| sb.checked_add(str_bytes))
        .ok_or(IndexError::Malformed("RRSF meta size overflow"))
}
