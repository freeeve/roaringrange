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

/// A range-fetchable facet sidecar. Holds the meta region in memory.
pub struct FacetIndex<F: RangeFetch> {
    fetch: F,
    /// The facet fields, in stored order.
    pub fields: Vec<Field>,
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
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let fields_n = read_u32(&header, 8) as usize;
        let cats_n = read_u32(&header, 12) as usize;
        let str_bytes = read_u32(&header, 16) as usize;

        // Lay out the meta region with checked arithmetic: on wasm32 (usize =
        // 32-bit) attacker-controlled counts could otherwise overflow and wrap to
        // a short read, then drive out-of-bounds indexing below.
        let field_tab = HEADER_SIZE;
        let cat_tab = fields_n
            .checked_mul(FIELD_ENTRY)
            .and_then(|x| x.checked_add(field_tab))
            .ok_or(IndexError::Malformed("facet field table size overflow"))?;
        let str_blob = cats_n
            .checked_mul(CAT_ENTRY)
            .and_then(|x| x.checked_add(cat_tab))
            .ok_or(IndexError::Malformed("facet category table size overflow"))?;
        let meta_len = str_blob
            .checked_add(str_bytes)
            .ok_or(IndexError::Malformed("facet meta size overflow"))?;
        let buf = fetch.read(0, meta_len).await?;
        if buf.len() < meta_len {
            return Err(IndexError::Malformed("facet meta region truncated"));
        }

        // Reads a display name from the string blob, rejecting an out-of-range
        // (off, len) instead of panicking on the slice.
        let read_name = |off: usize, len: usize| -> Result<String, IndexError> {
            let end = off
                .checked_add(len)
                .filter(|&e| e <= str_bytes)
                .ok_or(IndexError::Malformed("facet name out of string blob"))?;
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

        // Load the head postings filtered counts intersect against. Heads are a
        // small fraction of the file; the tails dominate and are fetched later
        // only for filtered tail pagination. A small sidecar's whole region is
        // read once; a large one would pull hundreds of MB of unused tails that
        // way, so fetch only the top-N heads per field instead. Offsets are
        // untrusted, so derive every slice bound with checked math.
        // The region length stays u64 until the eager branch: casting first would
        // truncate a >4 GiB region on wasm32 and route it down the eager path with
        // a wrapped length — a valid oversized sidecar is simply "large" (lazy).
        let region_len: u64 = match (cats.first(), cats.last()) {
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
            // Empty sidecar; nothing to load.
        } else if region_len <= eager_limit as u64 {
            let blob_start = cats[0].range.head_off;
            let blob = fetch.read(blob_start, region_len as usize).await?;
            for c in &mut cats {
                let s = c
                    .range
                    .head_off
                    .checked_sub(blob_start)
                    .ok_or(IndexError::Malformed("facet head offset precedes region"))?
                    as usize;
                let e = s
                    .checked_add(c.range.head_size as usize)
                    .filter(|&e| e <= blob.len())
                    .ok_or(IndexError::Malformed("facet head posting out of region"))?;
                c.head = RoaringBitmap::deserialize_from(&blob[s..e])
                    .map_err(|err| IndexError::Roaring(err.to_string()))?;
            }
        } else {
            let mut reqs: Vec<(usize, u64, usize)> = Vec::new();
            for (_, start, end) in &field_spans {
                let mut idxs: Vec<usize> = (*start..*end).collect();
                idxs.sort_by(|&a, &b| cats[b].count.cmp(&cats[a].count));
                for &j in idxs.iter().take(top_n) {
                    reqs.push((j, cats[j].range.head_off, cats[j].range.head_size as usize));
                }
            }
            // Coalesced: the top heads cluster near each field's region start,
            // so the wave collapses to roughly one request per field.
            let ranges: Vec<(u64, usize)> = reqs.iter().map(|&(_, off, len)| (off, len)).collect();
            let datas =
                crate::fetch::read_coalesced(&fetch, &ranges, crate::fetch::COALESCE_GAP).await?;
            for (&(j, _, _), bytes) in reqs.iter().zip(datas) {
                cats[j].head = RoaringBitmap::deserialize_from(&bytes[..])
                    .map_err(|err| IndexError::Roaring(err.to_string()))?;
            }
        }

        let fields = field_spans
            .into_iter()
            .map(|(name, start, end)| Field {
                name,
                categories: cats[start..end].to_vec(),
            })
            .collect();

        Ok(FacetIndex { fetch, fields })
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
        ResolvedFilter::new(self.fetch.clone(), by_field.into_values().collect())
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
}
