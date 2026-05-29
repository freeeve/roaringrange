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
/// Field-table entry size: nameOff(4) + nameLen(2) + pad(2) + catStart(4) + catCount(4).
const FIELD_ENTRY: usize = 16;
/// Category-table entry size: key(8) + headOff(8) + headSize(4) + tailSize(4) +
/// cardinality(4) + nameOff(4) + nameLen(2) + pad(2).
const CAT_ENTRY: usize = 36;

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
    /// Boots the facet index: reads the header, then the full meta region, and
    /// parses the field table, category table, and string blob into memory.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
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

        let field_tab = HEADER_SIZE;
        let cat_tab = field_tab + fields_n * FIELD_ENTRY;
        let str_blob = cat_tab + cats_n * CAT_ENTRY;
        let meta_len = str_blob + str_bytes;
        let buf = fetch.read(0, meta_len).await?;

        let name = |off: usize, len: usize| -> String {
            String::from_utf8_lossy(&buf[str_blob + off..str_blob + off + len]).into_owned()
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
                name: name(name_off, name_len),
                count,
                range: CatRange {
                    head_off,
                    head_size,
                    tail_off: head_off + head_size as u64,
                    tail_size,
                },
                head: RoaringBitmap::new(),
            });
        }

        // Load every category's head posting in one ranged read of the contiguous
        // postings region, so filtered counts are computed in memory. Categories
        // are stored in ascending head-offset order, so the region spans from the
        // first head to the last tail.
        if let (Some(first), Some(last)) = (cats.first(), cats.last()) {
            let blob_start = first.range.head_off;
            let blob_end = last.range.tail_off + last.range.tail_size as u64;
            let blob = fetch
                .read(blob_start, (blob_end - blob_start) as usize)
                .await?;
            for c in &mut cats {
                let s = (c.range.head_off - blob_start) as usize;
                let e = s + c.range.head_size as usize;
                c.head = RoaringBitmap::deserialize_from(&blob[s..e])
                    .map_err(|err| IndexError::Roaring(err.to_string()))?;
            }
        }

        let mut fields = Vec::with_capacity(fields_n);
        for i in 0..fields_n {
            let b = field_tab + i * FIELD_ENTRY;
            let name_off = read_u32(&buf, b) as usize;
            let name_len = read_u16(&buf, b + 4) as usize;
            let cat_start = read_u32(&buf, b + 8) as usize;
            let cat_count = read_u32(&buf, b + 12) as usize;
            fields.push(Field {
                name: name(name_off, name_len),
                categories: cats[cat_start..cat_start + cat_count].to_vec(),
            });
        }

        Ok(FacetIndex { fetch, fields })
    }

    /// Resolves `(field, category)` selections into a [`ResolvedFilter`]. Unknown
    /// fields or categories are skipped. Selections for the same field are grouped
    /// so they OR together; distinct fields AND.
    pub fn resolve(&self, pairs: &[(String, String)]) -> ResolvedFilter<F>
    where
        F: Clone,
    {
        let mut by_field: BTreeMap<usize, Vec<CatRange>> = BTreeMap::new();
        for (fname, cname) in pairs {
            if let Some((fi, field)) = self
                .fields
                .iter()
                .enumerate()
                .find(|(_, f)| &f.name == fname)
            {
                if let Some(c) = field.categories.iter().find(|c| &c.name == cname) {
                    by_field.entry(fi).or_default().push(c.range);
                }
            }
        }
        ResolvedFilter::new(self.fetch.clone(), by_field.into_values().collect())
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
