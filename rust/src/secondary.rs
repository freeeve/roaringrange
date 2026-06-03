//! Reader for a *secondary* full index — search/paginate the corpus in a rank
//! order other than the primary static rank.
//!
//! The primary index's "constant-cost top-K" property *is* its doc-ID assignment
//! (docs numbered in descending static rank, so ascending doc ID = top-K). A
//! different sort order therefore needs the postings physically laid out in *that*
//! order: a second `RRS` index whose doc IDs are assigned in the secondary rank
//! (e.g. newest-first). That second index reuses 100% of the head/tail machinery
//! and the lazy-tail cursor.
//!
//! Records, facets, and embeddings stay keyed by the **primary** doc ID, so the
//! only extra artifact is a permutation: an [`RRSC`](crate::sortcols) one-column
//! `u32` store named `"primary"` where `primary[secondary_id] = primary_id`. A
//! [`SecondaryCursor`] runs the query in secondary space, then maps each result
//! page back to primary doc IDs through that permutation — one coalesced gather of
//! ~`page` entries — so the caller fetches primary-keyed records exactly as for a
//! primary-order search. See `SORTCOLS.md`.

use crate::facet::{FacetIndex, Field};
use crate::fetch::RangeFetch;
use crate::index::{Cursor, Index, IndexError};
use crate::sortcols::SortCols;
use roaring::RoaringBitmap;

/// Name of the permutation column mapping secondary doc IDs to primary ones.
const PERM_COLUMN: &str = "primary";

/// A secondary full index: a second `RRS` text index reindexed in an alternate
/// rank order, the permutation column mapping its doc IDs back to the primary
/// space, and an optional secondary-space facet sidecar for filtered search.
///
/// Open it over the secondary `.rrs` and the perm `RRSC`, then optionally attach
/// the secondary `.rrf` with [`SecondaryIndex::with_facets`]. The facet sidecar is
/// the **same** facet postings as the primary one, but with each category's doc IDs
/// remapped into the secondary space (built once, alongside the secondary `.rrs`) —
/// so a roaring AND against them is positionally valid in secondary space. Facet
/// counts are order-independent (a count is a set cardinality, and the remap
/// preserves the set), so they come out identical to a primary-order search; only
/// which docs land on page 0 changes.
pub struct SecondaryIndex<F: RangeFetch + Clone> {
    index: Index<F>,
    perm: SortCols<F>,
    perm_col: usize,
    facets: Option<FacetIndex<F>>,
}

impl<F: RangeFetch + Clone> SecondaryIndex<F> {
    /// Boots a secondary index: opens the secondary text index at `rrs` and the
    /// permutation store at `perm`, validating that the perm carries the `"primary"`
    /// `u32` column. Mirrors [`Index::open`] plus [`SortCols::open`].
    pub async fn open(rrs: F, perm: F) -> Result<Self, IndexError> {
        let index = Index::open(rrs).await?;
        let perm = SortCols::open(perm).await?;
        let perm_col = perm.column_index(PERM_COLUMN).ok_or(IndexError::Malformed(
            "secondary perm store missing the 'primary' column",
        ))?;
        Ok(Self {
            index,
            perm,
            perm_col,
            facets: None,
        })
    }

    /// Opens the secondary-space facet sidecar at `facets` and attaches it, enabling
    /// filtered secondary search and facet counts. Builder style: consumes and
    /// returns `self`. The sidecar must hold the facet postings in **secondary** doc
    /// IDs (the build-time remap of the primary postings); see the type docs.
    pub async fn with_facets(mut self, facets: F) -> Result<Self, IndexError> {
        self.facets = Some(FacetIndex::open(facets).await?);
        Ok(self)
    }

    /// The wrapped secondary text index (doc IDs in secondary rank order).
    pub fn index(&self) -> &Index<F> {
        &self.index
    }

    /// The attached secondary-space facet sidecar, if any.
    pub fn facets(&self) -> Option<&FacetIndex<F>> {
        self.facets.as_ref()
    }

    /// The facet fields and their categories, or an empty slice when no facet
    /// sidecar is attached. Same shape as the primary order's fields.
    pub fn fields(&self) -> &[Field] {
        match &self.facets {
            Some(f) => &f.fields,
            None => &[],
        }
    }

    /// Per-category counts within `head` (a cursor's post-filter head bitmap, in
    /// secondary space), aligned with [`SecondaryIndex::fields`]; `None` when no
    /// facet sidecar is attached. Counts equal the primary order's — a count is a
    /// set cardinality, and the secondary remap preserves the set.
    pub fn counts(&self, head: &RoaringBitmap) -> Option<Vec<Vec<u64>>> {
        self.facets.as_ref().map(|f| f.counts(head))
    }

    /// Opens an unfiltered pagination cursor for `query` over the secondary order.
    /// Convenience for [`SecondaryIndex::search_cursor_filtered`] with no filter.
    pub async fn search_cursor(
        &self,
        query: &str,
        max_missing: usize,
    ) -> Result<SecondaryCursor<F>, IndexError> {
        self.search_cursor_filtered(query, max_missing, &[]).await
    }

    /// Opens a pagination cursor for `query` over the secondary order, ANDing the
    /// `filter` (a list of `(field, category)` selections — within-field OR,
    /// across-field AND) when a secondary facet sidecar is attached. The filter is
    /// resolved to **secondary-space** postings and intersected by the underlying
    /// space-agnostic [`Index::search_cursor_filtered`]; an empty filter, or none
    /// attached, is the unfiltered case.
    ///
    /// The cursor's pages come back as **primary** doc IDs (mapped through the
    /// permutation), so the caller fetches primary-keyed records unchanged.
    pub async fn search_cursor_filtered(
        &self,
        query: &str,
        max_missing: usize,
        filter: &[(String, String)],
    ) -> Result<SecondaryCursor<F>, IndexError> {
        let resolved = match &self.facets {
            Some(f) if !filter.is_empty() => Some(f.resolve(filter)),
            _ => None,
        };
        let inner = self
            .index
            .search_cursor_filtered(query, max_missing, resolved)
            .await?;
        Ok(SecondaryCursor {
            inner,
            perm: self.perm.clone(),
            perm_col: self.perm_col,
        })
    }
}

/// A pagination cursor over a secondary-ordered result set whose pages are mapped
/// back to primary doc IDs. Wraps the primary [`Cursor`] (so it inherits the
/// in-memory head paging and single lazy-tail wave) and the permutation store.
pub struct SecondaryCursor<F: RangeFetch + Clone> {
    inner: Cursor<F>,
    perm: SortCols<F>,
    perm_col: usize,
}

impl<F: RangeFetch + Clone> SecondaryCursor<F> {
    /// The page of **primary** doc IDs for the secondary-ordered results
    /// `[offset, offset+limit)`. Resolves the secondary IDs from the inner cursor
    /// (no fetch until the tail is needed), then gathers their primary IDs from the
    /// permutation in one coalesced wave.
    pub async fn page(&mut self, offset: usize, limit: usize) -> Result<Vec<u32>, IndexError> {
        let secondary = self.inner.page(offset, limit).await?;
        self.perm.values_u32(self.perm_col, &secondary).await
    }

    /// The query's post-filter head result as a bitmap (in **secondary** space).
    /// Pass to [`SecondaryIndex::counts`] to compute search-filtered facet counts
    /// without re-running the query.
    pub fn head_bitmap(&self) -> &RoaringBitmap {
        self.inner.head_bitmap()
    }

    /// Number of secondary doc IDs materialized so far (head, plus tail once fetched).
    pub fn loaded(&self) -> usize {
        self.inner.loaded()
    }

    /// Number of head (popular-in-secondary-rank) results — available with no tail
    /// fetch.
    pub fn head_count(&self) -> usize {
        self.inner.head_count()
    }

    /// Whether an unfetched tail intersection could still add results.
    pub fn pending_tail(&self) -> bool {
        self.inner.pending_tail()
    }

    /// Forces the lazy tail intersection to be fetched; afterwards `loaded` and
    /// `page` span the full result set.
    pub async fn load_tail(&mut self) -> Result<(), IndexError> {
        self.inner.load_tail().await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::build::{
        split_posting, write_facets, write_index, write_perm, FacetCategory, FacetField,
    };
    use crate::ngram::ngram_keys;
    use crate::MemoryFetch;
    use futures::executor::block_on;
    use roaring::RoaringBitmap;

    const HEAD_BOUNDARY: u32 = 65536;

    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    /// Builds an `RRS` index `MemoryFetch` from `(key, bitmap)` entries.
    fn rrs(entries: &[(u64, RoaringBitmap)]) -> MemoryFetch {
        let posts: Vec<(u64, Vec<u8>, Vec<u8>)> = entries
            .iter()
            .map(|(k, b)| {
                let (h, t) = split_posting(b, HEAD_BOUNDARY);
                (*k, h, t)
            })
            .collect();
        let mut out = Vec::new();
        write_index(&mut out, 3, 2, HEAD_BOUNDARY, posts).unwrap();
        MemoryFetch::new(out)
    }

    fn perm(primary_of_secondary: Vec<u32>) -> MemoryFetch {
        let mut out = Vec::new();
        write_perm(&mut out, primary_of_secondary).unwrap();
        MemoryFetch::new(out)
    }

    /// Builds an `RRSF` facet sidecar `MemoryFetch` from `(field, [(cat, bm)])`. The
    /// bitmaps are in whatever doc-ID space the caller passes — here, secondary.
    fn rrsf(fields: &[(&str, Vec<(&str, RoaringBitmap)>)]) -> MemoryFetch {
        let out_fields: Vec<FacetField> = fields
            .iter()
            .map(|(fname, cats)| FacetField {
                name: fname.to_string(),
                cats: cats
                    .iter()
                    .map(|(cname, b)| {
                        let (head, tail) = split_posting(b, HEAD_BOUNDARY);
                        FacetCategory {
                            name: cname.to_string(),
                            card: b.len() as u32,
                            head,
                            tail,
                        }
                    })
                    .collect(),
            })
            .collect();
        let mut out = Vec::new();
        write_facets(&mut out, out_fields).unwrap();
        MemoryFetch::new(out)
    }

    fn pairs(sel: &[(&str, &str)]) -> Vec<(String, String)> {
        sel.iter()
            .map(|(f, c)| (f.to_string(), c.to_string()))
            .collect()
    }

    /// Looks up the count for `(field, cat)` from a `counts` matrix aligned with
    /// `index.fields()` — the reader stores categories in key (not input) order, so
    /// resolve by name rather than position.
    fn count_of(
        sec: &SecondaryIndex<MemoryFetch>,
        counts: &[Vec<u64>],
        field: &str,
        cat: &str,
    ) -> u64 {
        let (fi, f) = sec
            .fields()
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == field)
            .expect("field present");
        let ci = f
            .categories
            .iter()
            .position(|c| c.name == cat)
            .expect("category present");
        counts[fi][ci]
    }

    /// "newest first" mapping: a query over the secondary order yields the matching
    /// secondary doc IDs in ascending (= secondary rank) order, each mapped to its
    /// primary doc ID for record lookup.
    #[test]
    fn pages_map_secondary_results_to_primary() {
        // 4 docs. Secondary order (e.g. year desc): sec0..sec3 map to primary
        // 1,3,2,0. "abc" matches secondary docs {0,2,3} (i.e. primary {1,2,0}).
        let abc = ngram_keys("abc", 3)[0];
        let index = rrs(&[(abc, bm(&[0, 2, 3]))]);
        let perm = perm(vec![1, 3, 2, 0]);
        let sec = block_on(SecondaryIndex::open(index, perm)).unwrap();

        let mut cur = block_on(sec.search_cursor("abc", 0)).unwrap();
        // Secondary-ascending results {0,2,3} -> primary {1,2,0} (newest first).
        assert_eq!(block_on(cur.page(0, 10)).unwrap(), vec![1, 2, 0]);
        // A sub-page maps the same way.
        assert_eq!(block_on(cur.page(1, 1)).unwrap(), vec![2]);
    }

    /// Filtered secondary search: the facet sidecar's postings are in secondary
    /// space (the build-time remap), so a facet AND is positionally valid there. The
    /// narrowed result still maps back to primary IDs, and counts over the
    /// post-filter head are the order-independent set cardinalities.
    #[test]
    fn filtered_secondary_search_narrows_and_maps() {
        let abc = ngram_keys("abc", 3)[0];
        // "abc" matches all four secondary docs; perm maps sec 0,1,2,3 -> primary
        // 1,3,2,0. Facet "kind" (secondary-space): article={0,2}, dataset={1,3}.
        let index = rrs(&[(abc, bm(&[0, 1, 2, 3]))]);
        let facets = rrsf(&[(
            "kind",
            vec![("article", bm(&[0, 2])), ("dataset", bm(&[1, 3]))],
        )]);
        let sec = block_on(async {
            SecondaryIndex::open(index, perm(vec![1, 3, 2, 0]))
                .await?
                .with_facets(facets)
                .await
        })
        .unwrap();

        let mut cur =
            block_on(sec.search_cursor_filtered("abc", 0, &pairs(&[("kind", "article")]))).unwrap();
        // kind=article keeps secondary {0,2} (ascending) -> primary {1,2}.
        assert_eq!(block_on(cur.page(0, 10)).unwrap(), vec![1, 2]);

        // Counts over the post-filter head {0,2}: article=2, dataset=0.
        let counts = sec.counts(cur.head_bitmap()).expect("facets attached");
        assert_eq!(count_of(&sec, &counts, "kind", "article"), 2);
        assert_eq!(count_of(&sec, &counts, "kind", "dataset"), 0);

        // Unfiltered counts over the whole head {0,1,2,3}: article=2, dataset=2.
        let mut all = block_on(sec.search_cursor("abc", 0)).unwrap();
        let _ = block_on(all.page(0, 10)).unwrap();
        let counts = sec.counts(all.head_bitmap()).expect("facets attached");
        assert_eq!(count_of(&sec, &counts, "kind", "article"), 2);
        assert_eq!(count_of(&sec, &counts, "kind", "dataset"), 2);
    }

    /// A secondary result that spans head into tail maps both halves to primary IDs;
    /// the tail is fetched lazily on the deep page.
    #[test]
    fn lazy_tail_page_maps_through_perm() {
        let abc = ngram_keys("abc", 3)[0];
        let tail_sec = HEAD_BOUNDARY + 1;
        let index = rrs(&[(abc, bm(&[0, 2, tail_sec]))]);
        // perm sized to cover the tail secondary id; map sec 0->10, 2->20, tail->30.
        let mut p = vec![0u32; (tail_sec + 1) as usize];
        p[0] = 10;
        p[2] = 20;
        p[tail_sec as usize] = 30;
        let sec = block_on(SecondaryIndex::open(index, perm(p))).unwrap();

        let mut cur = block_on(sec.search_cursor("abc", 0)).unwrap();
        assert_eq!(cur.head_count(), 2);
        assert!(cur.pending_tail());
        // Head-only page maps the two popular results.
        assert_eq!(block_on(cur.page(0, 2)).unwrap(), vec![10, 20]);
        // Crossing into the tail fetches it once, then maps all three.
        assert_eq!(block_on(cur.page(0, 10)).unwrap(), vec![10, 20, 30]);
        assert_eq!(cur.loaded(), 3);
    }

    #[test]
    fn rejects_perm_without_primary_column() {
        use crate::build::{write_sortcols, ColumnValues, SortColumn};
        let abc = ngram_keys("abc", 3)[0];
        let index = rrs(&[(abc, bm(&[0]))]);
        let mut bad = Vec::new();
        write_sortcols(
            &mut bad,
            vec![SortColumn {
                name: "wrongname".to_string(),
                values: ColumnValues::U32(vec![0]),
            }],
        )
        .unwrap();
        assert!(matches!(
            block_on(SecondaryIndex::open(index, MemoryFetch::new(bad))),
            Err(IndexError::Malformed(_))
        ));
    }
}
