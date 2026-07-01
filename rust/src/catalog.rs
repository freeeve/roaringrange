//! A one-type facade over the three reader resources.
//!
//! A complete no-backend search needs an [`Index`] (text → ranked doc IDs), and
//! optionally a [`FacetIndex`] (filtering + per-category counts) and a
//! [`RecordStore`] (doc ID → record bytes). [`Catalog`] bundles all three and
//! collapses the common "search → ranked IDs + records + facet counts" flow into
//! a single [`Catalog::search`] call. The underlying types stay public; the
//! facade is purely additive.
//!
//! All three resources share one [`RangeFetch`] type `F` but back distinct URLs:
//! the index, the facet sidecar, and the record store's offset index / blob are
//! independent resources, each opened with its own fetcher.

use crate::facet::{FacetIndex, FilterSel};
use crate::fetch::RangeFetch;
use crate::index::{Index, IndexError};
use crate::records::RecordStore;

/// The result of a [`Catalog::search`]: the ranked doc IDs for the requested
/// page plus, when the corresponding resource is attached, their record bytes
/// and the search-filtered facet counts.
pub struct SearchPage {
    /// Ranked doc IDs for the requested `[offset, offset+limit)` window, most
    /// popular first (ascending doc ID == descending popularity).
    pub ids: Vec<u32>,
    /// Record bytes aligned with `ids`, present only when a record store is
    /// attached. Each entry is `None` for an out-of-range doc ID.
    pub records: Option<Vec<Option<Vec<u8>>>>,
    /// Per-category facet counts over the query's head result (the whole result
    /// set, not just this page), present only when a facet sidecar is attached.
    /// When a `filter` was supplied the counts are over the post-filter head, so
    /// they shrink with the selection. The outer vec is aligned with
    /// [`Catalog::fields`]; the inner with each field's categories.
    pub facet_counts: Option<Vec<Vec<u64>>>,
}

/// A facade bundling a text [`Index`] with an optional [`FacetIndex`] and an
/// optional [`RecordStore`], all over the same [`RangeFetch`] type. Build one
/// with [`Catalog::open`] and attach the optional resources with
/// [`Catalog::load_facets`] / [`Catalog::load_records`].
pub struct Catalog<F: RangeFetch + Clone> {
    index: Index<F>,
    facets: Option<FacetIndex<F>>,
    records: Option<RecordStore<F>>,
}

impl<F: RangeFetch + Clone> Catalog<F> {
    /// Opens a catalog over the text index alone (no facets, no records). Boots
    /// the index header and sparse dictionary; facets and records can be attached
    /// afterward.
    pub async fn open(index: F) -> Result<Self, IndexError> {
        Ok(Self {
            index: Index::open(index).await?,
            facets: None,
            records: None,
        })
    }

    /// Opens the facet sidecar at `facets` and attaches it, enabling filtered
    /// search and facet counts. Builder style: consumes and returns `self`.
    pub async fn load_facets(mut self, facets: F) -> Result<Self, IndexError> {
        self.facets = Some(FacetIndex::open(facets).await?);
        Ok(self)
    }

    /// Opens the record store (`idx` offset index + `bin` record blob) and
    /// attaches it, so [`Catalog::search`] returns record bytes. Builder style:
    /// consumes and returns `self`.
    pub async fn load_records(mut self, idx: F, bin: F) -> Result<Self, IndexError> {
        self.records = Some(RecordStore::open(idx, bin).await?);
        Ok(self)
    }

    /// Opens the record store and attaches the shared zstd `dict` (the `*.dict`
    /// sidecar's bytes), so a version-2 compressed store's records inflate
    /// transparently in [`Catalog::search`]. A raw store ignores the dictionary,
    /// so this is always safe to use. Builder style: consumes and returns `self`.
    pub async fn load_records_dict(
        mut self,
        idx: F,
        bin: F,
        dict: Vec<u8>,
    ) -> Result<Self, IndexError> {
        self.records = Some(RecordStore::open_with_dict(idx, bin, dict).await?);
        Ok(self)
    }

    /// The facet fields and their categories, or an empty slice when no facet
    /// sidecar is attached. The order matches [`SearchPage::facet_counts`].
    pub fn fields(&self) -> &[crate::facet::Field] {
        match &self.facets {
            Some(f) => f.fields(),
            None => &[],
        }
    }

    /// The wrapped text index.
    pub fn index(&self) -> &Index<F> {
        &self.index
    }

    /// The attached facet sidecar, if any.
    pub fn facets(&self) -> Option<&FacetIndex<F>> {
        self.facets.as_ref()
    }

    /// The attached record store, if any.
    pub fn records(&self) -> Option<&RecordStore<F>> {
        self.records.as_ref()
    }

    /// Runs the full search flow for `query` and returns one [`SearchPage`].
    ///
    /// The `filter` is a list of [`FilterSel`] selections: includes OR within a
    /// field and AND across fields, while excludes (`negate`) union across all
    /// fields and are subtracted (`includes ANDNOT excludes`), so a doc in any
    /// excluded category is dropped. Resolved against the facet sidecar when one is
    /// attached and ignored otherwise. `max_missing` is the fuzzy tolerance
    /// forwarded to the cursor (0 = strict AND of every n-gram). The page covers
    /// ranked doc IDs `[offset, offset+limit)`.
    ///
    /// When a record store is attached the page's record bytes are fetched; when
    /// a facet sidecar is attached the search-filtered facet counts over the
    /// whole query's head result are computed in memory.
    pub async fn search(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
        max_missing: usize,
        filter: &[FilterSel],
    ) -> Result<SearchPage, IndexError> {
        let resolved = match &self.facets {
            Some(f) if !filter.is_empty() => Some(f.resolve_sels(filter)),
            _ => None,
        };
        let mut cursor = self
            .index
            .search_cursor_filtered(query, max_missing, resolved)
            .await?;

        let ids = cursor.page(offset, limit).await?;

        let records = match &self.records {
            Some(store) => Some(store.get_many(&ids).await?),
            None => None,
        };

        let facet_counts = self.facets.as_ref().map(|f| f.counts(cursor.head_bitmap()));

        Ok(SearchPage {
            ids,
            records,
            facet_counts,
        })
    }
}

// Uses the native-only build writers; gated to native so `wasm-pack test` builds.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::build::{
        serialize_posting, split_posting, write_facets, write_index, write_records, FacetCategory,
        FacetField,
    };
    use crate::ngram::ngram_keys;
    use crate::MemoryFetch;
    use futures::executor::block_on;
    use roaring::RoaringBitmap;

    const HEAD_BOUNDARY: u32 = 65536;

    /// A bitmap from an explicit doc-ID list.
    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    /// Builds an `RRS` index `MemoryFetch` from `(key, bitmap)` entries.
    fn rrs(entries: &[(u64, RoaringBitmap)]) -> MemoryFetch {
        let posts: Vec<(u64, Vec<u8>)> = entries
            .iter()
            .map(|(k, b)| (*k, serialize_posting(b)))
            .collect();
        let mut out = Vec::new();
        write_index(&mut out, 3, 2, posts).unwrap();
        MemoryFetch::new(out)
    }

    /// Builds an `RRSF` facet sidecar `MemoryFetch` from `(field, [(cat, bm)])`.
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

    /// Builds an `RRSR` record store as `(idx, bin)` `MemoryFetch` pair.
    fn records(recs: &[Vec<u8>]) -> (MemoryFetch, MemoryFetch) {
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        write_records(&mut bin, &mut idx, recs).unwrap();
        (MemoryFetch::new(idx), MemoryFetch::new(bin))
    }

    /// A common fixture: "abc" matches docs {1,2,3,4,5} plus one tail doc; a
    /// two-field facet sidecar; and a record per doc ID.
    fn fixture() -> (MemoryFetch, MemoryFetch, (MemoryFetch, MemoryFetch)) {
        let abc = ngram_keys("abc", 3)[0];
        let tail = HEAD_BOUNDARY + 1;
        let index = rrs(&[(abc, bm(&[1, 2, 3, 4, 5, tail]))]);
        let facets = rrsf(&[
            (
                "format",
                vec![("ebook", bm(&[1, 3, 5, tail])), ("audiobook", bm(&[2, 4]))],
            ),
            (
                "language",
                vec![("en", bm(&[1, 2, 3])), ("es", bm(&[4, 5, tail]))],
            ),
        ]);
        // Records for doc IDs 0..=6; doc 0 is unused by the query.
        let recs: Vec<Vec<u8>> = (0..=6u32)
            .map(|d| format!("rec-{d}").into_bytes())
            .collect();
        let store = records(&recs);
        (index, facets, store)
    }

    fn pairs(sel: &[(&str, &str)]) -> Vec<FilterSel> {
        sel.iter().map(|&(f, c)| FilterSel::include(f, c)).collect()
    }

    #[test]
    fn index_only_search_returns_ranked_ids() {
        let (index, _, _) = fixture();
        let cat = block_on(Catalog::open(index)).unwrap();
        assert!(cat.fields().is_empty());

        let page = block_on(cat.search("abc", 0, 3, 0, &[])).unwrap();
        assert_eq!(page.ids, vec![1, 2, 3]);
        assert!(page.records.is_none());
        assert!(page.facet_counts.is_none());

        // Offset paging across the head into the tail.
        let page = block_on(cat.search("abc", 4, 10, 0, &[])).unwrap();
        assert_eq!(page.ids, vec![5, HEAD_BOUNDARY + 1]);
    }

    #[test]
    fn full_catalog_returns_ids_records_and_counts() {
        let (index, facets, (idx, bin)) = fixture();
        let cat = block_on(async {
            Catalog::open(index)
                .await?
                .load_facets(facets)
                .await?
                .load_records(idx, bin)
                .await
        })
        .unwrap();

        // Fields exposed and aligned with facet_counts.
        let names: Vec<&str> = cat.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["format", "language"]);

        let page = block_on(cat.search("abc", 0, 3, 0, &[])).unwrap();
        assert_eq!(page.ids, vec![1, 2, 3]);

        // Records align with the page's doc IDs.
        let recs = page.records.expect("records attached");
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].as_deref().unwrap(), b"rec-1");
        assert_eq!(recs[1].as_deref().unwrap(), b"rec-2");
        assert_eq!(recs[2].as_deref().unwrap(), b"rec-3");

        // Counts are over the whole query head {1,2,3,4,5}, not just this page:
        // format ebook{1,3,5}=3, audiobook{2,4}=2; language en{1,2,3}=3, es{4,5}=2.
        let counts = page.facet_counts.expect("facets attached");
        assert_eq!(counts, vec![vec![3u64, 2], vec![3, 2]]);
    }

    #[test]
    fn filtered_search_narrows_ids_and_keeps_full_counts() {
        let (index, facets, (idx, bin)) = fixture();
        let cat = block_on(async {
            Catalog::open(index)
                .await?
                .load_facets(facets)
                .await?
                .load_records(idx, bin)
                .await
        })
        .unwrap();

        // Across-field AND: ebook{1,3,5,tail} ∩ en{1,2,3} = {1,3}.
        let page = block_on(cat.search(
            "abc",
            0,
            100,
            0,
            &pairs(&[("format", "ebook"), ("language", "en")]),
        ))
        .unwrap();
        assert_eq!(page.ids, vec![1, 3]);
        let recs = page.records.unwrap();
        assert_eq!(recs[0].as_deref().unwrap(), b"rec-1");
        assert_eq!(recs[1].as_deref().unwrap(), b"rec-3");

        // Counts are computed over the cursor's (post-filter) head result {1,3},
        // mirroring the existing wasm binding: ebook∩{1,3}=2, audiobook=0;
        // en∩{1,3}=2, es=0.
        let counts = page.facet_counts.unwrap();
        assert_eq!(counts, vec![vec![2u64, 0], vec![2, 0]]);
    }

    #[test]
    fn filter_ignored_without_facet_sidecar() {
        let (index, _, _) = fixture();
        let cat = block_on(Catalog::open(index)).unwrap();
        // A filter with no sidecar attached is a no-op; the full match is returned.
        let page = block_on(cat.search("abc", 0, 100, 0, &pairs(&[("format", "ebook")]))).unwrap();
        assert_eq!(page.ids, vec![1, 2, 3, 4, 5, HEAD_BOUNDARY + 1]);
        assert!(page.facet_counts.is_none());
    }

    #[test]
    fn records_without_facets() {
        let (index, _, (idx, bin)) = fixture();
        let cat =
            block_on(async { Catalog::open(index).await?.load_records(idx, bin).await }).unwrap();
        let page = block_on(cat.search("abc", 0, 2, 0, &[])).unwrap();
        assert_eq!(page.ids, vec![1, 2]);
        assert_eq!(page.records.unwrap()[1].as_deref().unwrap(), b"rec-2");
        assert!(page.facet_counts.is_none());
    }

    fn full_cat() -> Catalog<MemoryFetch> {
        let (index, facets, (idx, bin)) = fixture();
        block_on(async {
            Catalog::open(index)
                .await?
                .load_facets(facets)
                .await?
                .load_records(idx, bin)
                .await
        })
        .unwrap()
    }

    #[test]
    fn exclude_only_filter_removes_matching_docs() {
        // No include: the full query head {1,2,3,4,5}(+tail) minus audiobook{2,4}.
        let page = block_on(full_cat().search(
            "abc",
            0,
            100,
            0,
            &[FilterSel::exclude("format", "audiobook")],
        ))
        .unwrap();
        assert_eq!(page.ids, vec![1, 3, 5, HEAD_BOUNDARY + 1]);
        // Counts over the post-exclusion survivors {1,3,5}; the excluded category
        // still lists (audiobook = 0) so the UI can offer to un-exclude it.
        assert_eq!(page.facet_counts.unwrap(), vec![vec![3u64, 0], vec![2, 1]]);
    }

    #[test]
    fn include_and_exclude_combine() {
        // ebook{1,3,5,tail} ANDNOT es{4,5,tail} = {1,3}. The excluded `es`
        // category includes the tail doc, so it must be dropped on the tail path
        // too (regression for excludes skipped during incremental tail paging).
        let page = block_on(full_cat().search(
            "abc",
            0,
            100,
            0,
            &[
                FilterSel::include("format", "ebook"),
                FilterSel::exclude("language", "es"),
            ],
        ))
        .unwrap();
        assert_eq!(page.ids, vec![1, 3]);
    }

    #[test]
    fn multiple_excludes_union() {
        // query {1,2,3,4,5,tail} ANDNOT (audiobook{2,4} ∪ es{4,5,tail}) = {1,3}.
        // The union of excludes spans the tail doc, which must be removed on the
        // incremental tail path as well as the head.
        let page = block_on(full_cat().search(
            "abc",
            0,
            100,
            0,
            &[
                FilterSel::exclude("format", "audiobook"),
                FilterSel::exclude("language", "es"),
            ],
        ))
        .unwrap();
        assert_eq!(page.ids, vec![1, 3]);
    }
}
