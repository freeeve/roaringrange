//! WASM bindings (behind the `wasm` feature).
//!
//! [`RangeFetch`] is asynchronous, so the browser side issues real `fetch()`
//! HTTP Range requests and awaits them. Because every read is async, no Web
//! Worker or synchronous `XMLHttpRequest` is needed, and a query's independent
//! reads run concurrently (see [`crate::index::Index::search`]).
//!
//! `fetch` is resolved off the global scope via `js_sys::global()` + `Reflect`,
//! so the same code path works on both the main thread (`Window`) and inside a
//! Web Worker (`WorkerGlobalScope`).
//!
//! Build: `wasm-pack build --target web --features wasm`. To also inflate
//! zstd-compressed (version-2) records in the browser, add the `zstd` feature:
//! `wasm-pack build --target web --features "wasm zstd"` — the decode path is the
//! pure-Rust `ruzstd` decoder, so it builds for wasm. The host serving the index
//! must support HTTP Range requests; pass the index URL to [`RrsIndex::open`].

use crate::catalog::Catalog;
use crate::facet::{FacetIndex, Field};
use crate::fetch::{FetchError, RangeFetch};
use crate::index::{Cursor, Index};
use crate::lookup::Lookup;
use crate::range_cache::RangeCache;
use crate::records::RecordStore;
use crate::secondary::{SecondaryCursor, SecondaryIndex};
use crate::sortcols::SortCols;
#[cfg(feature = "terms")]
use crate::terms::TermIndex;
#[cfg(feature = "vector")]
use crate::vector::VectorIndex;
use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint32Array, Uint8Array};
use roaring::RoaringBitmap;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, RequestMode, Response};

thread_local! {
    /// Process-wide range cache shared by every [`WasmFetch`]. `None` until the client opts in via
    /// [`set_range_cache_mb`]; resized or cleared by the same call. The wasm runtime is
    /// single-threaded, so a thread-local `Rc<RefCell<_>>` is all the sharing this needs.
    static RANGE_CACHE: RefCell<Option<Rc<RefCell<RangeCache>>>> = const { RefCell::new(None) };
}

/// A handle to the shared range cache, if the client has enabled one.
fn current_range_cache() -> Option<Rc<RefCell<RangeCache>>> {
    RANGE_CACHE.with(|c| c.borrow().clone())
}

/// Sets the shared range-cache budget in **mebibytes**, enabling caching for every range read
/// across every index type (trigram, term, facet, vector, records, split-set, ...). `0` or negative
/// disables and clears the cache. Resizing keeps warm entries, evicting LRU-first if the new budget
/// is smaller. Already-open indexes are affected too: each read resolves the cache live.
#[wasm_bindgen(js_name = setRangeCacheMb)]
pub fn set_range_cache_mb(mb: f64) {
    let max_bytes = if mb <= 0.0 {
        0
    } else {
        (mb * 1024.0 * 1024.0) as usize
    };
    RANGE_CACHE.with(|c| {
        let mut slot = c.borrow_mut();
        match (max_bytes, slot.as_ref()) {
            (0, _) => *slot = None,
            (_, Some(existing)) => existing.borrow_mut().set_max_bytes(max_bytes),
            (_, None) => *slot = Some(Rc::new(RefCell::new(RangeCache::new(max_bytes)))),
        }
    });
}

/// `[payloadBytes, entryCount, hits, misses]` for the shared range cache, or `[0, 0, 0, 0]` when no
/// cache is enabled — a JS-side readout of cache effectiveness.
#[wasm_bindgen(js_name = rangeCacheStats)]
pub fn range_cache_stats() -> Vec<f64> {
    RANGE_CACHE.with(|c| match c.borrow().as_ref() {
        Some(cache) => {
            let (bytes, entries, hits, misses) = cache.borrow().stats();
            vec![bytes as f64, entries as f64, hits as f64, misses as f64]
        }
        None => vec![0.0, 0.0, 0.0, 0.0],
    })
}

/// Default shared range-cache budget (MiB), installed on module start so caching is on by default;
/// clients resize or disable it at runtime via [`set_range_cache_mb`].
const DEFAULT_CACHE_MB: f64 = 128.0;

/// Runs once when the wasm module is instantiated: installs the default range cache so every index
/// type benefits without any JS setup. `setRangeCacheMb(0)` disables it; `setRangeCacheMb(n)` resizes.
#[wasm_bindgen(start)]
pub fn wasm_start() {
    set_range_cache_mb(DEFAULT_CACHE_MB);
}

/// A [`RangeFetch`] backed by the browser `fetch()` API, reading byte ranges of
/// a single index URL via the `Range` request header.
#[derive(Clone)]
struct WasmFetch {
    url: String,
}

impl WasmFetch {
    /// Creates a fetcher for the index object at `url`.
    fn new(url: String) -> Self {
        Self { url }
    }

    /// Invokes the global `fetch(request)` from either a `Window` or a
    /// `WorkerGlobalScope`, returning the resulting promise.
    fn global_fetch(request: &Request) -> Result<js_sys::Promise, FetchError> {
        let global = js_sys::global();
        let fetch_fn = Reflect::get(&global, &JsValue::from_str("fetch"))
            .map_err(|_| FetchError::Transport("global fetch is unavailable".into()))?;
        let fetch_fn: js_sys::Function = fetch_fn
            .dyn_into()
            .map_err(|_| FetchError::Transport("global fetch is not a function".into()))?;
        let promise = fetch_fn
            .call1(&global, request.as_ref())
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        promise
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| FetchError::Transport("fetch did not return a promise".into()))
    }

    /// Downloads the entire object at `url` (a plain GET, no `Range`) — used for
    /// the one-time model2vec `RRM2` artifact, which must be held whole in memory.
    #[cfg(feature = "vector")]
    async fn get_all(url: &str) -> Result<Vec<u8>, FetchError> {
        let opts = RequestInit::new();
        opts.set_method("GET");
        opts.set_mode(RequestMode::Cors);
        let request = Request::new_with_str_and_init(url, &opts)
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let promise = WasmFetch::global_fetch(&request)?;
        let resp_value = JsFuture::from(promise)
            .await
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let response: Response = resp_value
            .dyn_into()
            .map_err(|_| FetchError::Transport("fetch returned a non-Response".into()))?;
        if !response.ok() {
            return Err(FetchError::Transport(format!(
                "HTTP {} for {url}",
                response.status()
            )));
        }
        let buf_promise = response
            .array_buffer()
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let buf_value = JsFuture::from(buf_promise)
            .await
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let array_buffer: ArrayBuffer = buf_value
            .dyn_into()
            .map_err(|_| FetchError::Transport("array_buffer was not an ArrayBuffer".into()))?;
        Ok(Uint8Array::new(&array_buffer).to_vec())
    }
}

/// Formats a `JsValue` error into a human-readable string.
fn js_err(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "unknown JS error".into())
}

impl RangeFetch for WasmFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        // Serve from the shared range cache when the client enabled one; a re-typed query
        // re-requests the same (url, offset, len), so this skips the network round-trip. The
        // borrow is released before any await (RefCell is not held across `.await`).
        let cache = current_range_cache();
        if let Some(cache) = &cache {
            if let Some(bytes) = cache.borrow_mut().get(&self.url, offset, len) {
                return Ok(bytes);
            }
        }
        let end = match offset.checked_add(len as u64) {
            Some(sum) => sum - 1,
            None => {
                return Err(FetchError::Transport(format!(
                    "range {offset}+{len} overflows u64"
                )))
            }
        };
        let range = format!("bytes={offset}-{end}");

        let headers = Headers::new().map_err(|e| FetchError::Transport(js_err(&e)))?;
        headers
            .set("Range", &range)
            .map_err(|e| FetchError::Transport(js_err(&e)))?;

        let opts = RequestInit::new();
        opts.set_method("GET");
        opts.set_mode(RequestMode::Cors);
        opts.set_headers(headers.as_ref());

        let request = Request::new_with_str_and_init(&self.url, &opts)
            .map_err(|e| FetchError::Transport(js_err(&e)))?;

        let promise = WasmFetch::global_fetch(&request)?;
        let resp_value = JsFuture::from(promise)
            .await
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let response: Response = resp_value
            .dyn_into()
            .map_err(|_| FetchError::Transport("fetch returned a non-Response".into()))?;

        if !response.ok() {
            return Err(FetchError::Transport(format!(
                "HTTP {} for range {range}",
                response.status()
            )));
        }

        let buf_promise = response
            .array_buffer()
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let buf_value = JsFuture::from(buf_promise)
            .await
            .map_err(|e| FetchError::Transport(js_err(&e)))?;
        let array_buffer: ArrayBuffer = buf_value
            .dyn_into()
            .map_err(|_| FetchError::Transport("array_buffer was not an ArrayBuffer".into()))?;
        let bytes = Uint8Array::new(&array_buffer).to_vec();
        // The reader trusts RangeFetch to return exactly `len` bytes — every
        // header/offset parser slices on that guarantee. A CDN/origin that ignores
        // the Range header (200 full body, or a 206 that clamps/coalesces) would
        // otherwise feed a wrong-length buffer into those parsers. Enforce it here.
        if bytes.len() != len {
            return Err(FetchError::Transport(format!(
                "range {range} returned {} bytes, expected {len} (origin may not honor Range requests)",
                bytes.len()
            )));
        }
        // Populate the cache (clone is a memcpy, far cheaper than the network read it spares).
        if let Some(cache) = &cache {
            cache
                .borrow_mut()
                .insert(&self.url, offset, len, bytes.clone());
        }
        Ok(bytes)
    }
}

/// The single object shape behind every facet accessor: a JS `Array` of
/// `{ field, cats: [{ name, count }, ...] }` for `fields`, with each category's `count` taken from
/// `counts[fi][ci]`. The `*Json`-string builders were replaced by this so JS callers get structured
/// objects, not strings they must `JSON.parse`.
fn facets_array_js(fields: &[Field], counts: &[Vec<u64>]) -> Array {
    let out = Array::new_with_length(fields.len() as u32);
    for (fi, field) in fields.iter().enumerate() {
        let cats = Array::new_with_length(field.categories.len() as u32);
        for (ci, cat) in field.categories.iter().enumerate() {
            let obj = Object::new();
            let _ = Reflect::set(&obj, &"name".into(), &cat.name.as_str().into());
            let _ = Reflect::set(&obj, &"count".into(), &(counts[fi][ci] as f64).into());
            cats.set(ci as u32, obj.into());
        }
        let obj = Object::new();
        let _ = Reflect::set(&obj, &"field".into(), &field.name.as_str().into());
        let _ = Reflect::set(&obj, &"cats".into(), &cats.into());
        out.set(fi as u32, obj.into());
    }
    out
}

/// The search-filtered facet counts over `head` as a JS `Array` of `{ field, cats: [{ name, count
/// }] }` (counts computed in memory); an empty array when no facet sidecar is open.
fn facet_counts_js(facets: Option<&FacetIndex<WasmFetch>>, head: &RoaringBitmap) -> JsValue {
    match facets {
        Some(facets) => facets_array_js(&facets.fields, &facets.counts(head)).into(),
        None => Array::new().into(),
    }
}

/// The facet `fields` with their **full-corpus** counts (from the in-memory meta region) as a JS
/// `Array` of `{ field, cats: [{ name, count }] }`. Shared by the index accessors and `RrsCatalog`.
fn facets_meta_array(fields: &[Field]) -> Array {
    let counts: Vec<Vec<u64>> = fields
        .iter()
        .map(|f| f.categories.iter().map(|c| c.count as u64).collect())
        .collect();
    facets_array_js(fields, &counts)
}

/// [`facets_meta_array`] over an optional sidecar; an empty array when no sidecar is present.
fn facets_meta_js(facets: Option<&FacetIndex<WasmFetch>>) -> JsValue {
    match facets {
        Some(facets) => facets_meta_array(&facets.fields).into(),
        None => Array::new().into(),
    }
}

/// Parses the JS facet filter — an array of `[field, category]` pairs (e.g.
/// `[["year","2020"],["type","article"]]`; within a field categories OR, across fields AND) —
/// into `(field, category)` tuples. Throws if any entry is not a two-string array, so a malformed
/// filter fails loudly rather than being silently dropped. An empty array means "no filter".
fn filter_pairs(filters: &Array) -> Result<Vec<(String, String)>, JsError> {
    let mut pairs = Vec::with_capacity(filters.length() as usize);
    for entry in filters.iter() {
        let pair: Array = entry
            .dyn_into()
            .map_err(|_| JsError::new("each facet filter must be a [field, category] array"))?;
        let field = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsError::new("facet filter field must be a string"))?;
        let category = pair
            .get(1)
            .as_string()
            .ok_or_else(|| JsError::new("facet filter category must be a string"))?;
        pairs.push((field, category));
    }
    Ok(pairs)
}

/// Filters a ranked doc-ID list by the selected facet `pairs` and returns the survivors
/// (input order preserved) plus search-filtered counts over them. Shared by the text index and
/// the standalone facet binding.
async fn filtered_ids(
    facets: Option<&FacetIndex<WasmFetch>>,
    ids: Vec<u32>,
    pairs: Vec<(String, String)>,
) -> Result<FilteredIds, JsError> {
    let kept = match facets {
        Some(facets) if !pairs.is_empty() => {
            let filter = facets.resolve(&pairs);
            if filter.is_empty() {
                ids
            } else {
                // Membership, not the full filter bitmap: the candidates span a
                // handful of 64K buckets, so each selected posting is read at
                // container granularity — a broad category (tens of MB whole)
                // costs KBs here.
                let candidates: RoaringBitmap = ids.iter().copied().collect();
                let mask = filter
                    .membership_bitmap(&candidates)
                    .await
                    .map_err(|e| JsError::new(&e.to_string()))?;
                ids.into_iter().filter(|id| mask.contains(*id)).collect()
            }
        }
        _ => ids,
    };
    let bitmap: RoaringBitmap = kept.iter().copied().collect();
    let counts = facet_counts_js(facets, &bitmap);
    Ok(FilteredIds { ids: kept, counts })
}

/// A range-fetchable RRS index exposed to JavaScript. Optionally carries an
/// opened facet sidecar (`RRSF`) used to filter queries.
#[wasm_bindgen]
pub struct RrsIndex {
    inner: Index<WasmFetch>,
    facets: Option<FacetIndex<WasmFetch>>,
}

#[wasm_bindgen]
impl RrsIndex {
    /// Boots the index at `url`: fetches the header and sparse index. Returns a
    /// `Promise<RrsIndex>` to JavaScript. Facets are not opened here; call
    /// [`RrsIndex::open_facets`] afterward if a sidecar is available.
    pub async fn open(url: String) -> Result<RrsIndex, JsError> {
        let inner = Index::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsIndex {
            inner,
            facets: None,
        })
    }

    /// Boots the optional facet sidecar at `url` and attaches it to this index,
    /// enabling [`RrsIndex::facets_json`] and filtered search.
    #[wasm_bindgen(js_name = openFacets)]
    pub async fn open_facets(&mut self, url: String) -> Result<(), JsError> {
        let facets = FacetIndex::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.facets = Some(facets);
        Ok(())
    }

    /// Returns the facet fields and their categories as a JS array of
    /// `{ field, cats: [{ name, count }] }`. An empty array when no sidecar is open. Counts are
    /// full-corpus and free (served from the in-memory meta region).
    #[wasm_bindgen(js_name = facets)]
    pub fn facets(&self) -> JsValue {
        facets_meta_js(self.facets.as_ref())
    }

    /// Returns up to `limit` matching doc IDs, most-popular first. Resolves to a
    /// `Uint32Array` in JavaScript.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .search(query, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Estimated client-side bytes a search for `query` (plus the optional facet
    /// `filters`, the same `[field, category]` pairs `searchCursorFiltered` takes)
    /// would fetch — priced from KB-scale dictionary reads and the resident facet
    /// table only; **no posting is fetched**. Compare against a routing threshold
    /// to send expensive queries to a server-side search instead. `0` when a query
    /// trigram is absent (the strict-AND search short-circuits to empty).
    #[wasm_bindgen(js_name = queryCost)]
    pub async fn query_cost(&self, query: &str, filters: Option<Array>) -> Result<f64, JsError> {
        let mut total = self
            .inner
            .query_cost(query)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        if let (Some(filters), Some(f)) = (filters, self.facets.as_ref()) {
            total += f.filter_cost(&filter_pairs(&filters)?);
        }
        Ok(total as f64)
    }

    /// Exact-or-bounded result count for a strict-AND `query` (+ optional facet
    /// `filters`), **without fetching any posting body**: KB-scale dictionary +
    /// posting-header reads, plus the resident facet counts. `exact` is true only
    /// for a single-trigram unfiltered query; otherwise `count` is an upper bound
    /// (the smallest per-trigram cardinality, min'd with the filter's resident
    /// count bound). Not valid for fuzzy (`max_missing > 0`) matching.
    #[wasm_bindgen(js_name = countEstimate)]
    pub async fn count_estimate(
        &self,
        query: &str,
        filters: Option<Array>,
    ) -> Result<CountEstimate, JsError> {
        let (mut count, mut exact) = self
            .inner
            .count_estimate(query)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        if let (Some(filters), Some(f)) = (filters, self.facets.as_ref()) {
            if let Some(bound) = f.filter_count_bound(&filter_pairs(&filters)?) {
                if bound < count {
                    count = bound;
                }
                exact = false; // a filter bound is never exact
            }
        }
        Ok(CountEstimate {
            count: count as f64,
            exact,
        })
    }

    /// Opens a stateful pagination cursor for `query` (one head fetch wave up
    /// front). Resolves to an `RrsCursor`.
    #[wasm_bindgen(js_name = searchCursor)]
    pub async fn search_cursor(
        &self,
        query: &str,
        max_missing: usize,
    ) -> Result<RrsCursor, JsError> {
        let inner = self
            .inner
            .search_cursor(query, max_missing)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let counts = facet_counts_js(self.facets.as_ref(), inner.head_bitmap());
        Ok(RrsCursor { inner, counts })
    }

    /// Like [`RrsIndex::search_cursor`] but ANDs the selected facets into the
    /// result. `filters` is an array of `[field, category]` pairs (within a field categories OR,
    /// across fields they AND); a malformed entry throws. The filter is applied only when a
    /// sidecar is open and `filters` is non-empty. Resolves to an `RrsCursor`.
    #[wasm_bindgen(js_name = searchCursorFiltered)]
    pub async fn search_cursor_filtered(
        &self,
        query: String,
        max_missing: usize,
        filters: Array,
    ) -> Result<RrsCursor, JsError> {
        let pairs = filter_pairs(&filters)?;
        let filter = match &self.facets {
            Some(facets) if !pairs.is_empty() => Some(facets.resolve(&pairs)),
            _ => None,
        };
        let inner = self
            .inner
            .search_cursor_filtered(&query, max_missing, filter)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let counts = facet_counts_js(self.facets.as_ref(), inner.head_bitmap());
        Ok(RrsCursor { inner, counts })
    }

    /// Filters a ranked doc-ID list (e.g. semantic/vector hits) by the selected
    /// facets, preserving the input order, and returns the survivors plus
    /// search-filtered facet counts over them. Because `vector_id == doc_id`, the
    /// vector path reuses the same `RRSF` sidecar the trigram path uses — no
    /// remapping. `filters` is an array of `[field, category]` pairs (within a field categories
    /// OR, across fields they AND); a malformed entry throws. With no sidecar open or no filters,
    /// the IDs pass through unchanged (counts still computed when a sidecar is open). Resolves to a
    /// `FilteredIds`.
    #[wasm_bindgen(js_name = filterIds)]
    pub async fn filter_ids(&self, ids: Vec<u32>, filters: Array) -> Result<FilteredIds, JsError> {
        filtered_ids(self.facets.as_ref(), ids, filter_pairs(&filters)?).await
    }

    /// Number of n-grams in the index dictionary.
    #[wasm_bindgen(js_name = ngramCount)]
    pub fn ngram_count(&self) -> u32 {
        self.inner.ngram_count()
    }
}

/// Result of [`RrsIndex::count_estimate`]: a result count and whether it is
/// exact (single-trigram unfiltered query) or an upper bound.
#[wasm_bindgen]
pub struct CountEstimate {
    count: f64,
    exact: bool,
}

#[wasm_bindgen]
impl CountEstimate {
    /// The exact count, or the upper bound when `exact` is false.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> f64 {
        self.count
    }

    /// Whether `count` is exact rather than an upper bound.
    #[wasm_bindgen(getter)]
    pub fn exact(&self) -> bool {
        self.exact
    }
}

/// Result of [`RrsIndex::filter_ids`]: the surviving doc IDs (input ranking
/// order preserved) and search-filtered facet counts over them.
#[wasm_bindgen]
pub struct FilteredIds {
    ids: Vec<u32>,
    counts: JsValue,
}

#[wasm_bindgen]
impl FilteredIds {
    /// The surviving doc IDs as a `Uint32Array`, in the input ranking order.
    #[wasm_bindgen(getter)]
    pub fn ids(&self) -> Vec<u32> {
        self.ids.clone()
    }

    /// Search-filtered facet counts over the survivors, as a JS array of
    /// `{ field, cats: [{ name, count }] }` (same shape as `facets()`); an empty array when no
    /// facet sidecar is open.
    #[wasm_bindgen(js_name = facetCounts)]
    pub fn facet_counts(&self) -> JsValue {
        self.counts.clone()
    }
}

/// A standalone facet sidecar (`RRSF`) exposed to JavaScript, opened on its own
/// without the text index. Lets the vector/semantic path filter results and show
/// facet counts even when the (much larger) `.rrs` text index isn't present —
/// they share the doc-ID space, so the `.rrf` applies directly.
#[wasm_bindgen]
pub struct RrfFacets {
    inner: FacetIndex<WasmFetch>,
}

#[wasm_bindgen]
impl RrfFacets {
    /// Boots the facet sidecar at `url` (header + category metadata; postings are
    /// range-fetched on demand). Resolves to an `RrfFacets`.
    pub async fn open(url: String) -> Result<RrfFacets, JsError> {
        let inner = FacetIndex::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrfFacets { inner })
    }

    /// Facet fields and categories with full-corpus counts, as a JS array of
    /// `{ field, cats: [{ name, count }] }` (same shape as `RrsIndex.facets()`).
    #[wasm_bindgen(js_name = facets)]
    pub fn facets(&self) -> JsValue {
        facets_meta_js(Some(&self.inner))
    }

    /// Filters a ranked doc-ID list by the selected facets (same contract as
    /// `RrsIndex.filterIds`). Resolves to a `FilteredIds`.
    #[wasm_bindgen(js_name = filterIds)]
    pub async fn filter_ids(&self, ids: Vec<u32>, filters: Array) -> Result<FilteredIds, JsError> {
        filtered_ids(Some(&self.inner), ids, filter_pairs(&filters)?).await
    }
}

/// A stateful pagination cursor exposed to JavaScript.
#[wasm_bindgen]
pub struct RrsCursor {
    inner: Cursor<WasmFetch>,
    /// Search-filtered facet counts for this query (counts restricted to this query's head
    /// result), prebuilt as a JS value at open time.
    counts: JsValue,
}

#[wasm_bindgen]
impl RrsCursor {
    /// The search-filtered facet counts as a JS array of `{ field, cats: [{ name, count }] }` —
    /// how many of this query's results fall in each category; an empty array when no facet
    /// sidecar is open.
    #[wasm_bindgen(js_name = facetCounts)]
    pub fn facet_counts(&self) -> JsValue {
        self.counts.clone()
    }

    /// Returns the next `n` doc IDs as a `Uint32Array`. Pages within the head
    /// cost no fetches; crossing into the tail triggers one concurrent wave.
    pub async fn next(&mut self, n: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .next(n)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Random-access page: up to `limit` doc IDs starting at `offset`. Paging
    /// backward never fetches; paging past the head fetches the tail once.
    pub async fn page(&mut self, offset: usize, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .page(offset, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Number of doc IDs materialized so far.
    pub fn loaded(&self) -> usize {
        self.inner.loaded()
    }

    /// Number of head (popular) results — available immediately, no tail fetch.
    #[wasm_bindgen(js_name = headCount)]
    pub fn head_count(&self) -> usize {
        self.inner.head_count()
    }

    /// Whether loading the tail could still add results (its intersection is unfetched).
    #[wasm_bindgen(js_name = pendingTail)]
    pub fn pending_tail(&self) -> bool {
        self.inner.pending_tail()
    }

    /// Fetches the lazy tail intersection on demand; afterwards `loaded`/`page`
    /// cover the full result set.
    #[wasm_bindgen(js_name = loadTail)]
    pub async fn load_tail(&mut self) -> Result<(), JsError> {
        self.inner
            .load_tail()
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }
}

/// A range-fetchable `RRSR` record store exposed to JavaScript: maps a ranked
/// doc ID to its raw record bytes over HTTP Range. The offset index (`.idx`) and
/// the record blob (`.bin`) are each backed by their own [`WasmFetch`] URL.
#[wasm_bindgen]
pub struct RrsRecords {
    inner: RecordStore<WasmFetch>,
}

#[wasm_bindgen]
impl RrsRecords {
    /// Boots the record store: reads and validates the 16-byte `RRSR` header of
    /// the offset index at `idx_url`, with records served from the blob at
    /// `bin_url`. Returns a `Promise<RrsRecords>` to JavaScript.
    pub async fn open(idx_url: String, bin_url: String) -> Result<RrsRecords, JsError> {
        let inner = RecordStore::open(WasmFetch::new(idx_url), WasmFetch::new(bin_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsRecords { inner })
    }

    /// Boots a record store and attaches the shared zstd dictionary `dict` (the
    /// `*.dict` sidecar's bytes, e.g. fetched once at boot, passed as a
    /// `Uint8Array`), so version-2 compressed records inflate transparently.
    /// Requires the crate to be built with the `zstd` feature for a compressed
    /// store; a raw store ignores the dictionary. Returns a `Promise<RrsRecords>`.
    #[wasm_bindgen(js_name = openWithDict)]
    pub async fn open_with_dict(
        idx_url: String,
        bin_url: String,
        dict: Vec<u8>,
    ) -> Result<RrsRecords, JsError> {
        let inner =
            RecordStore::open_with_dict(WasmFetch::new(idx_url), WasmFetch::new(bin_url), dict)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsRecords { inner })
    }

    /// Number of records (doc IDs `0..len`).
    pub fn len(&self) -> u32 {
        self.inner.len()
    }

    /// Whether the store holds no records.
    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Raw record bytes for doc `id` as a `Uint8Array`, or `undefined` (a JS
    /// `null`) when `id` is out of range. One Range read of the offset pair, one
    /// of the record slice.
    pub async fn get(&self, id: u32) -> Result<JsValue, JsError> {
        match self
            .inner
            .get(id)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?
        {
            Some(bytes) => Ok(Uint8Array::from(bytes.as_slice()).into()),
            None => Ok(JsValue::NULL),
        }
    }

    /// Record bytes for doc `id` decoded as a UTF-8 string, or `undefined` (a JS
    /// `null`) when `id` is out of range. Convenience for JSON/text records;
    /// invalid UTF-8 is replaced lossily.
    #[wasm_bindgen(js_name = getText)]
    pub async fn get_text(&self, id: u32) -> Result<JsValue, JsError> {
        match self
            .inner
            .get(id)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?
        {
            Some(bytes) => Ok(JsValue::from_str(&String::from_utf8_lossy(&bytes))),
            None => Ok(JsValue::NULL),
        }
    }

    /// Raw record bytes for several doc IDs (a results page is the typical
    /// input). Resolves to a JS `Array` aligned with `ids`: each element is a
    /// `Uint8Array`, or `null` for an out-of-range doc ID.
    #[wasm_bindgen(js_name = getMany)]
    pub async fn get_many(&self, ids: Vec<u32>) -> Result<Array, JsError> {
        let records = self
            .inner
            .get_many(&ids)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let out = Array::new_with_length(records.len() as u32);
        for (i, rec) in records.into_iter().enumerate() {
            let value = match rec {
                Some(bytes) => Uint8Array::from(bytes.as_slice()).into(),
                None => JsValue::NULL,
            };
            out.set(i as u32, value);
        }
        Ok(out)
    }
}

/// Builds the facet-count JS value for a [`SearchPage`]: a JS `Array` of
/// `{ field, cats: [{ name, count }, ...] }`, aligned with `fields`. The
/// per-category `count`s come from `counts` (search-filtered). Returns `null`
/// when no facet sidecar is attached (`counts` is `None`).
fn facet_counts_to_js(fields: &[Field], counts: &Option<Vec<Vec<u64>>>) -> JsValue {
    match counts {
        Some(counts) => facets_array_js(fields, counts).into(),
        None => JsValue::NULL,
    }
}

/// A range-fetchable [`Catalog`] exposed to JavaScript: one object bundling the
/// `RRS` index with an optional `RRSF` facet sidecar and `RRSR` record store, so
/// the whole "search → ranked IDs + records + facet counts" flow is one call.
/// Mirrors [`RrsIndex`]/[`RrsRecords`]; adopt it in place of wiring those three
/// together by hand.
#[wasm_bindgen]
pub struct RrsCatalog {
    /// Always `Some` between calls; held in an `Option` so the consuming
    /// builder methods (`with_facets`/`with_records`) can `take` and replace it.
    inner: Option<Catalog<WasmFetch>>,
}

#[wasm_bindgen]
impl RrsCatalog {
    /// Boots a catalog over the index at `index_url` alone (header + sparse
    /// dictionary). Attach the optional sidecars with [`RrsCatalog::open_facets`]
    /// and [`RrsCatalog::open_records`]. Returns a `Promise<RrsCatalog>`.
    pub async fn open(index_url: String) -> Result<RrsCatalog, JsError> {
        let inner = Catalog::open(WasmFetch::new(index_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsCatalog { inner: Some(inner) })
    }

    /// The wrapped catalog. Panics only if a previous builder call left `inner`
    /// empty, which the builder methods never do (they always restore it).
    fn cat(&self) -> &Catalog<WasmFetch> {
        self.inner.as_ref().expect("catalog present")
    }

    /// Boots the catalog with all three resources at once: the index at
    /// `index_url`, the facet sidecar at `facets_url`, and the record store
    /// (`records_idx_url` offset index + `records_bin_url` blob). Returns a
    /// `Promise<RrsCatalog>`.
    #[wasm_bindgen(js_name = openAll)]
    pub async fn open_all(
        index_url: String,
        facets_url: String,
        records_idx_url: String,
        records_bin_url: String,
    ) -> Result<RrsCatalog, JsError> {
        let inner = Catalog::open(WasmFetch::new(index_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?
            .with_facets(WasmFetch::new(facets_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?
            .with_records(
                WasmFetch::new(records_idx_url),
                WasmFetch::new(records_bin_url),
            )
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsCatalog { inner: Some(inner) })
    }

    /// Opens the facet sidecar at `url` and attaches it, enabling filtered search
    /// and facet counts.
    #[wasm_bindgen(js_name = openFacets)]
    pub async fn open_facets(&mut self, url: String) -> Result<(), JsError> {
        let prev = self.inner.take().expect("catalog present");
        self.inner = Some(
            prev.with_facets(WasmFetch::new(url))
                .await
                .map_err(|e| JsError::new(&e.to_string()))?,
        );
        Ok(())
    }

    /// Opens the record store (`idx_url` offset index + `bin_url` record blob)
    /// and attaches it, so [`RrsCatalog::search`] returns record bytes.
    #[wasm_bindgen(js_name = openRecords)]
    pub async fn open_records(&mut self, idx_url: String, bin_url: String) -> Result<(), JsError> {
        let prev = self.inner.take().expect("catalog present");
        self.inner = Some(
            prev.with_records(WasmFetch::new(idx_url), WasmFetch::new(bin_url))
                .await
                .map_err(|e| JsError::new(&e.to_string()))?,
        );
        Ok(())
    }

    /// Opens the record store (`idx_url` offset index + `bin_url` record blob)
    /// with the shared zstd dictionary `dict` (the `*.dict` sidecar's bytes,
    /// passed as a `Uint8Array`) and attaches it, so a version-2 compressed store
    /// inflates records transparently in [`RrsCatalog::search`]. Requires the
    /// crate to be built with the `zstd` feature for a compressed store; a raw
    /// store ignores the dictionary.
    #[wasm_bindgen(js_name = openRecordsWithDict)]
    pub async fn open_records_with_dict(
        &mut self,
        idx_url: String,
        bin_url: String,
        dict: Vec<u8>,
    ) -> Result<(), JsError> {
        let prev = self.inner.take().expect("catalog present");
        self.inner = Some(
            prev.with_records_dict(WasmFetch::new(idx_url), WasmFetch::new(bin_url), dict)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?,
        );
        Ok(())
    }

    /// Runs the full search flow and resolves to a JS object:
    /// `{ ids: Uint32Array, records: Array<Uint8Array|null> | null,
    /// facetCounts: Array<{field, cats:[{name, count}]}> | null }`.
    ///
    /// `filters` is an array of `[field, category]` pairs (e.g.
    /// `[["format","ebook"],["language","en"]]`); an empty array `[]` means no filter, and a
    /// malformed entry throws. Within a field categories OR, across fields they AND. The page
    /// covers ranked doc IDs `[offset, offset+len)`; `max_missing` is the fuzzy
    /// tolerance (0 = strict). `records`/`facetCounts` are `null` unless the
    /// matching sidecar is attached.
    pub async fn search(
        &self,
        query: String,
        offset: usize,
        len: usize,
        max_missing: usize,
        filters: Array,
    ) -> Result<JsValue, JsError> {
        let filter = filter_pairs(&filters)?;
        let page = self
            .cat()
            .search(&query, offset, len, max_missing, &filter)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;

        let ids = Uint32Array::from(page.ids.as_slice());
        let records = match page.records {
            Some(recs) => {
                let arr = Array::new_with_length(recs.len() as u32);
                for (i, rec) in recs.into_iter().enumerate() {
                    let value = match rec {
                        Some(bytes) => Uint8Array::from(bytes.as_slice()).into(),
                        None => JsValue::NULL,
                    };
                    arr.set(i as u32, value);
                }
                arr.into()
            }
            None => JsValue::NULL,
        };
        let facet_counts = facet_counts_to_js(self.cat().fields(), &page.facet_counts);

        let out = Object::new();
        Reflect::set(&out, &"ids".into(), &ids.into()).map_err(|e| JsError::new(&js_err(&e)))?;
        Reflect::set(&out, &"records".into(), &records).map_err(|e| JsError::new(&js_err(&e)))?;
        Reflect::set(&out, &"facetCounts".into(), &facet_counts)
            .map_err(|e| JsError::new(&js_err(&e)))?;
        Ok(out.into())
    }

    /// Returns the facet fields and their full-corpus category counts as a JS array of
    /// `{ field, cats: [{ name, count }] }`, or an empty array when no facet sidecar is attached.
    /// Mirrors [`RrsIndex::facets`].
    #[wasm_bindgen(js_name = facets)]
    pub fn facets(&self) -> JsValue {
        facets_meta_array(self.cat().fields()).into()
    }

    /// Number of n-grams in the index dictionary.
    #[wasm_bindgen(js_name = ngramCount)]
    pub fn ngram_count(&self) -> u32 {
        self.cat().index().ngram_count()
    }
}

/// A standalone portable RoaringBitmap exposed to JavaScript for client-side set
/// operations over external `.bm` bitmaps — e.g. the per-library bitmaps a static
/// catalog ships for library diff / intersection / collection paging. The bytes
/// are the portable serialization written by Go's `RoaringBitmap/roaring/v2`
/// `WriteTo` (the same format the index postings use), so they deserialize here
/// byte-for-byte with no glue.
#[wasm_bindgen]
pub struct WasmBitmap {
    inner: RoaringBitmap,
}

#[wasm_bindgen]
impl WasmBitmap {
    /// Deserializes a portable RoaringBitmap from `bytes`.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<WasmBitmap, JsError> {
        let inner = RoaringBitmap::deserialize_from(bytes)
            .map_err(|e| JsError::new(&format!("deserialize bitmap: {e}")))?;
        Ok(WasmBitmap { inner })
    }

    /// Intersection (`self ∩ other`) as a new bitmap.
    pub fn and(&self, other: &WasmBitmap) -> WasmBitmap {
        let mut inner = self.inner.clone();
        inner &= &other.inner;
        WasmBitmap { inner }
    }

    /// Difference (`self \ other`) as a new bitmap.
    pub fn andnot(&self, other: &WasmBitmap) -> WasmBitmap {
        let mut inner = self.inner.clone();
        inner -= &other.inner;
        WasmBitmap { inner }
    }

    /// Union (`self ∪ other`) as a new bitmap.
    pub fn or(&self, other: &WasmBitmap) -> WasmBitmap {
        let mut inner = self.inner.clone();
        inner |= &other.inner;
        WasmBitmap { inner }
    }

    /// Number of doc IDs set (cardinality).
    pub fn len(&self) -> u32 {
        u32::try_from(self.inner.len()).unwrap_or(u32::MAX)
    }

    /// Whether the bitmap holds no doc IDs.
    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Doc IDs in ascending order (== rank order, since doc IDs are popularity-
    /// ranked), skipping `offset` and taking up to `limit`. Resolves to a
    /// `Uint32Array`.
    pub fn page(&self, offset: usize, limit: usize) -> Vec<u32> {
        self.inner.iter().skip(offset).take(limit).collect()
    }
}

/// A range-fetchable identifier exact-match index (`RRIL`) exposed to JavaScript:
/// resolves an ISBN/ASIN/… to the ranked doc IDs of the title(s) carrying it, over
/// HTTP Range. Pairs with the trigram index, which no longer carries identifiers.
#[wasm_bindgen]
pub struct RrsLookup {
    inner: Lookup<WasmFetch>,
}

#[wasm_bindgen]
impl RrsLookup {
    /// Boots the index at `url` (reads the 16-byte header). Returns a
    /// `Promise<RrsLookup>`.
    pub async fn open(url: String) -> Result<RrsLookup, JsError> {
        let inner = Lookup::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsLookup { inner })
    }

    /// Resolves `identifier` to the doc IDs of the title(s) carrying it (most
    /// popular first), as a `Uint32Array`. Empty if none.
    pub async fn lookup(&self, identifier: String) -> Result<Vec<u32>, JsError> {
        self.inner
            .lookup(&identifier)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Number of index entries.
    pub fn len(&self) -> u32 {
        self.inner.len()
    }

    /// Whether the index holds no entries.
    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// A range-fetchable [`SortCols`] store exposed to JavaScript: dense columns
/// indexed by doc ID, used to re-rank a materialized candidate set client-side
/// (sort by rating / date / any secondary metric) and to map a secondary index's
/// doc IDs back to the primary space. See `SORTCOLS.md`.
#[wasm_bindgen]
pub struct RrsSortCols {
    inner: SortCols<WasmFetch>,
}

#[wasm_bindgen]
impl RrsSortCols {
    /// Boots the store at `url`: reads the header + column meta (a few KB; the dense
    /// data is range-fetched per query). Returns a `Promise<RrsSortCols>`.
    pub async fn open(url: String) -> Result<RrsSortCols, JsError> {
        let inner = SortCols::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsSortCols { inner })
    }

    /// Number of rows (doc IDs `0..rows`) every column holds.
    pub fn rows(&self) -> u32 {
        self.inner.rows()
    }

    /// The index of the column named `name`, or `-1` if absent.
    #[wasm_bindgen(js_name = columnIndex)]
    pub fn column_index(&self, name: String) -> i32 {
        self.inner
            .column_index(&name)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    /// A JS array of the columns' `{ name, type }` (`type` is one of
    /// `"u16"`/`"u32"`/`"i32"`/`"f32"`), in stored order.
    #[wasm_bindgen(js_name = columns)]
    pub fn columns(&self) -> JsValue {
        let cols = self.inner.columns();
        let out = Array::new_with_length(cols.len() as u32);
        for (i, c) in cols.iter().enumerate() {
            let t = match c.value_type {
                crate::sortcols::ValueType::U16 => "u16",
                crate::sortcols::ValueType::U32 => "u32",
                crate::sortcols::ValueType::I32 => "i32",
                crate::sortcols::ValueType::F32 => "f32",
            };
            let obj = Object::new();
            let _ = Reflect::set(&obj, &"name".into(), &c.name.as_str().into());
            let _ = Reflect::set(&obj, &"type".into(), &t.into());
            out.set(i as u32, obj.into());
        }
        out.into()
    }

    /// Values for `ids` in column `col`, as a `Float64Array` aligned with `ids`
    /// (every stored type is exactly representable in `f64`). One coalesced wave of
    /// ranged reads.
    pub async fn values(&self, col: usize, ids: Vec<u32>) -> Result<Vec<f64>, JsError> {
        let vals = self
            .inner
            .values(col, &ids)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(vals.into_iter().map(|v| v.as_f64()).collect())
    }

    /// The contiguous run `[start, start+len)` of a `u32` column as a `Uint32Array`
    /// — the permutation-page fast path. Clamps to the row count.
    #[wasm_bindgen(js_name = sliceU32)]
    pub async fn slice_u32(&self, col: usize, start: u32, len: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .slice_u32(col, start, len)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// The top `k` of `candidates` by column `col` as a `Uint32Array`, descending
    /// when `descending` (else ascending); ties keep ascending doc-ID order.
    pub async fn topk(
        &self,
        col: usize,
        candidates: Vec<u32>,
        k: usize,
        descending: bool,
    ) -> Result<Vec<u32>, JsError> {
        self.inner
            .topk(col, &candidates, k, descending)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }
}

/// A secondary full index exposed to JavaScript: a second `RRS` reindexed in an
/// alternate rank order (e.g. newest-first), the permutation back to primary doc
/// IDs, and an optional secondary-space facet sidecar for filtered search. Search it
/// like [`RrsIndex`]; the cursor's pages come back as **primary** doc IDs, so
/// records are fetched through the existing primary-keyed store unchanged. Facet
/// counts are identical to the primary order's. See `SORTCOLS.md`.
#[wasm_bindgen]
pub struct RrsSecondaryIndex {
    /// Held in an `Option` so the consuming builder `with_facets` can be driven by
    /// the `&mut self` `open_facets` (take, attach, replace).
    inner: Option<SecondaryIndex<WasmFetch>>,
}

#[wasm_bindgen]
impl RrsSecondaryIndex {
    /// Boots the secondary index over the text index at `rrs_url` and the
    /// permutation store at `perm_url`. Returns a `Promise<RrsSecondaryIndex>`.
    pub async fn open(rrs_url: String, perm_url: String) -> Result<RrsSecondaryIndex, JsError> {
        let inner = SecondaryIndex::open(WasmFetch::new(rrs_url), WasmFetch::new(perm_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrsSecondaryIndex { inner: Some(inner) })
    }

    /// Opens the secondary-space facet sidecar at `url` and attaches it, enabling
    /// `facetsJson` and filtered secondary search.
    #[wasm_bindgen(js_name = openFacets)]
    pub async fn open_facets(&mut self, url: String) -> Result<(), JsError> {
        let sec = self
            .inner
            .take()
            .ok_or_else(|| JsError::new("secondary index unavailable"))?;
        let sec = sec
            .with_facets(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.inner = Some(sec);
        Ok(())
    }

    /// The facet fields with full-corpus counts as a JS array of `{ field, cats: [{ name, count }]
    /// }` (same shape as [`RrsIndex::facets`]); an empty array when no sidecar is open.
    #[wasm_bindgen(js_name = facets)]
    pub fn facets(&self) -> JsValue {
        match self.inner.as_ref() {
            Some(sec) => facets_meta_array(sec.fields()).into(),
            None => Array::new().into(),
        }
    }

    /// Opens an unfiltered pagination cursor for `query` over the secondary order.
    /// `max_missing` is the fuzzy tolerance (0 = strict).
    #[wasm_bindgen(js_name = searchCursor)]
    pub async fn search_cursor(
        &self,
        query: String,
        max_missing: usize,
    ) -> Result<RrsSecondaryCursor, JsError> {
        self.search_cursor_filtered(query, max_missing, Array::new())
            .await
    }

    /// Like [`RrsSecondaryIndex::search_cursor`] but ANDs the selected facets into
    /// the result. `filters` is an array of `[field, category]` pairs (within a field categories
    /// OR, across fields they AND); a malformed entry throws. Applied only when a secondary
    /// sidecar is open and `filters` is non-empty.
    #[wasm_bindgen(js_name = searchCursorFiltered)]
    pub async fn search_cursor_filtered(
        &self,
        query: String,
        max_missing: usize,
        filters: Array,
    ) -> Result<RrsSecondaryCursor, JsError> {
        let sec = self
            .inner
            .as_ref()
            .ok_or_else(|| JsError::new("secondary index unavailable"))?;
        let pairs = filter_pairs(&filters)?;
        let inner = sec
            .search_cursor_filtered(&query, max_missing, &pairs)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let counts = facet_counts_js(sec.facets(), inner.head_bitmap());
        Ok(RrsSecondaryCursor { inner, counts })
    }
}

/// A pagination cursor over a secondary-ordered result set whose pages are mapped
/// back to primary doc IDs. Mirrors [`RrsCursor`]; [`RrsSecondaryCursor::page`]
/// returns a `Uint32Array` of **primary** doc IDs.
#[wasm_bindgen]
pub struct RrsSecondaryCursor {
    inner: SecondaryCursor<WasmFetch>,
    /// Search-filtered facet counts for this query (secondary-space head result, identical to the
    /// primary order's counts), prebuilt as a JS value at open time.
    counts: JsValue,
}

#[wasm_bindgen]
impl RrsSecondaryCursor {
    /// The search-filtered facet counts as a JS array of `{ field, cats: [{ name, count }] }`
    /// (same shape as `facets()`, counts restricted to this query's result); an empty array when
    /// no secondary sidecar is open.
    #[wasm_bindgen(js_name = facetCounts)]
    pub fn facet_counts(&self) -> JsValue {
        self.counts.clone()
    }

    /// The next `n` primary doc IDs in secondary rank order, advancing an internal position —
    /// the sequential counterpart of [`page`](Self::page), matching `RrsCursor.next`.
    pub async fn next(&mut self, n: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .next(n)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// The page of primary doc IDs for the secondary-ordered results
    /// `[offset, offset+limit)`. Head pages cost no posting fetch; crossing into the
    /// tail fetches it once. Always one small coalesced permutation gather per page.
    pub async fn page(&mut self, offset: usize, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .page(offset, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Number of secondary results materialized so far (head, plus tail once fetched).
    pub fn loaded(&self) -> usize {
        self.inner.loaded()
    }

    /// Number of head results — available with no tail fetch.
    #[wasm_bindgen(js_name = headCount)]
    pub fn head_count(&self) -> usize {
        self.inner.head_count()
    }

    /// Whether an unfetched tail could still add results.
    #[wasm_bindgen(js_name = pendingTail)]
    pub fn pending_tail(&self) -> bool {
        self.inner.pending_tail()
    }

    /// Forces the lazy tail to be fetched; afterwards `loaded`/`page` span the full
    /// result set.
    #[wasm_bindgen(js_name = loadTail)]
    pub async fn load_tail(&mut self) -> Result<(), JsError> {
        self.inner
            .load_tail()
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }
}

/// A range-fetchable RRVI similarity (vector) index exposed to JavaScript. Built
/// with `wasm-pack build --target web --features "wasm vector"`.
#[cfg(feature = "vector")]
#[wasm_bindgen]
pub struct RrviIndex {
    inner: VectorIndex<WasmFetch>,
    rerank: Option<crate::vector::RerankStore<WasmFetch>>,
}

#[cfg(feature = "vector")]
#[wasm_bindgen]
impl RrviIndex {
    /// Boots the RRVI index at `url`: one boot read of the coarse centroids, PQ
    /// codebooks, optional OPQ rotation, and cluster directory. Returns a
    /// `Promise<RrviIndex>`.
    pub async fn open(url: String) -> Result<RrviIndex, JsError> {
        let inner = VectorIndex::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrviIndex {
            inner,
            rerank: None,
        })
    }

    /// Opens the optional `RRVR` re-rank sidecar at `url` and attaches it, enabling
    /// [`RrviIndex::search_rerank`].
    #[wasm_bindgen(js_name = openRerank)]
    pub async fn open_rerank(&mut self, url: String) -> Result<(), JsError> {
        let store = crate::vector::RerankStore::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.rerank = Some(store);
        Ok(())
    }

    /// Searches for the `k` nearest vectors to `query` (a `Float32Array` of length
    /// `dim`), probing the `nprobe` nearest clusters in one concurrent wave of
    /// ranged reads. Resolves to an `RrviHits` with aligned `ids`/`scores`,
    /// best-first. An inner-product index normalizes the query for you; `doc_id`
    /// matches the text index's doc ID, so hits map straight to the record store.
    pub async fn search(
        &self,
        query: Vec<f32>,
        k: usize,
        nprobe: usize,
    ) -> Result<RrviHits, JsError> {
        let hits = self
            .inner
            .search(&query, k, nprobe)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrviHits::from_hits(hits))
    }

    /// Like [`RrviIndex::search`] but re-ranks the ADC top-`r` candidates against
    /// the higher-precision re-rank sidecar (open it first with `openRerank`),
    /// returning the exact top-`k`. Rejects if no sidecar is open.
    #[wasm_bindgen(js_name = searchRerank)]
    pub async fn search_rerank(
        &self,
        query: Vec<f32>,
        k: usize,
        nprobe: usize,
        r: usize,
    ) -> Result<RrviHits, JsError> {
        let rerank = self.rerank.as_ref().ok_or_else(|| {
            JsError::new("re-rank sidecar not opened; call openRerank(url) first")
        })?;
        let hits = self
            .inner
            .search_rerank(&query, k, nprobe, r, rerank)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrviHits::from_hits(hits))
    }

    /// Vector dimensionality the index was built with.
    pub fn dim(&self) -> usize {
        self.inner.dim()
    }

    /// Number of coarse (IVF) clusters.
    pub fn nlist(&self) -> usize {
        self.inner.nlist()
    }

    /// Total number of indexed vectors.
    pub fn len(&self) -> u32 {
        self.inner.len() as u32
    }

    /// Whether the index holds no vectors.
    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// The result of [`RrviIndex::search`]: aligned doc IDs and similarity scores,
/// best-first. In JavaScript `ids` is a `Uint32Array` and `scores` a
/// `Float32Array`.
#[cfg(feature = "vector")]
#[wasm_bindgen]
pub struct RrviHits {
    ids: Vec<u32>,
    scores: Vec<f32>,
}

#[cfg(feature = "vector")]
#[wasm_bindgen]
impl RrviHits {
    /// The matching doc IDs (`Uint32Array`), best-first.
    #[wasm_bindgen(getter)]
    pub fn ids(&self) -> Vec<u32> {
        self.ids.clone()
    }

    /// The similarity scores (`Float32Array`) aligned with `ids`; higher is better.
    #[wasm_bindgen(getter)]
    pub fn scores(&self) -> Vec<f32> {
        self.scores.clone()
    }
}

#[cfg(feature = "vector")]
impl RrviHits {
    /// Splits search hits into aligned id/score vectors for JS.
    fn from_hits(hits: Vec<crate::vector::VectorHit>) -> Self {
        let mut ids = Vec::with_capacity(hits.len());
        let mut scores = Vec::with_capacity(hits.len());
        for h in hits {
            ids.push(h.doc_id);
            scores.push(h.score);
        }
        RrviHits { ids, scores }
    }
}

/// Reciprocal-rank fusion of a vector (`RRVI`) and a trigram (`RRS`) result list
/// into one ranking of doc IDs, best-first — the no-score-normalization hybrid.
/// `kParam` is conventionally ~60. Returns a `Uint32Array`.
#[cfg(feature = "vector")]
#[wasm_bindgen(js_name = reciprocalRankFusion)]
pub fn reciprocal_rank_fusion_js(
    vector_ids: Vec<u32>,
    trigram_ids: Vec<u32>,
    k_param: f64,
) -> Vec<u32> {
    crate::vector::reciprocal_rank_fusion(&[&vector_ids, &trigram_ids], k_param)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// The in-browser model2vec query embedder (mode 2) exposed to JavaScript: turns
/// query text into a `Float32Array` vector with no backend, to feed
/// [`RrviIndex::search`]. Built with `wasm-pack build --features "wasm vector"`.
#[cfg(feature = "vector")]
#[wasm_bindgen]
pub struct Model2vecEmbedder {
    inner: crate::model2vec::Model2vec,
}

#[cfg(feature = "vector")]
#[wasm_bindgen]
impl Model2vecEmbedder {
    /// Downloads the `RRM2` artifact at `url` once (a plain GET; ~tens of MB,
    /// browser-cached) and builds the embedder. Returns a `Promise<Model2vecEmbedder>`.
    pub async fn open(url: String) -> Result<Model2vecEmbedder, JsError> {
        let bytes = WasmFetch::get_all(&url)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let inner = crate::model2vec::Model2vec::from_bytes(&bytes)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(Model2vecEmbedder { inner })
    }

    /// Vector dimensionality (must match the `RRVI` index it queries).
    pub fn dim(&self) -> usize {
        self.inner.dim()
    }

    /// Embeds `text` into a `Float32Array` query vector (BERT tokenize → static
    /// embedding mean-pool → L2-normalize). Pass it to `RrviIndex.search`.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        self.inner.embed(text)
    }
}

/// A range-fetchable `RRTI` term-level inverted index exposed to JavaScript. Boot
/// holds only the small resident block router in memory (O(#blocks), not O(vocab));
/// each query range-fetches the dict blocks and postings it needs. Built with
/// `wasm-pack build --target web --features "wasm terms"`.
#[cfg(feature = "terms")]
#[wasm_bindgen]
pub struct RrtIndex {
    inner: TermIndex<WasmFetch>,
}

#[cfg(feature = "terms")]
#[wasm_bindgen]
impl RrtIndex {
    /// Boots the index at `url`: one boot read of the small block router, held
    /// resident so a term resolves to its dict block with a single ranged read.
    /// Returns a `Promise<RrtIndex>`.
    pub async fn open(url: String) -> Result<RrtIndex, JsError> {
        let inner = TermIndex::open(WasmFetch::new(url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrtIndex { inner })
    }

    /// Returns up to `limit` doc IDs matching every query term (whole-word AND),
    /// most popular first (ascending doc ID == descending rank). Resolves to a
    /// `Uint32Array`. A query term absent from the dictionary yields no results.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .search(query, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Returns up to `limit` doc IDs matching any term that starts with `prefix`
    /// (the union of every prefix-matching term's posting), most popular first.
    /// Resolves to a `Uint32Array`.
    #[wasm_bindgen(js_name = searchPrefix)]
    pub async fn search_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .search_prefix(prefix, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Autocompletes `prefix`: up to `max_terms` dictionary terms that start with
    /// it, in lexicographic order, as a JS `string[]`. Range-fetches only the dict
    /// blocks spanning the prefix. Resolves to a `Promise<string[]>`. (Typo/substring
    /// search is the trigram `RRS` index's job — it composes over the same doc IDs.)
    pub async fn complete(&self, prefix: &str, max_terms: usize) -> Result<Vec<String>, JsError> {
        self.inner
            .complete(prefix, max_terms)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Number of distinct terms in the dictionary.
    pub fn len(&self) -> u32 {
        self.inner.len() as u32
    }

    /// Whether the dictionary holds no terms.
    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// A [`SplitFetcher`] for the browser: resolves each split's `data_file` name (and the
/// stable-key sort-column name) to a [`WasmFetch`] over `base_url/<name>`. When the set was
/// booted from a boot bundle (`RrssIndex.openBundle`), `boot` hands each split its inlined boot
/// from the resident `.rrhc`, so the split opens with no per-split header fetch.
#[cfg(feature = "splits")]
struct WasmSplitResolver {
    base: String,
    /// Optional global term-Bloom sidecar name (resolved like any split file);
    /// the tiered query path range-probes it after an empty top tier.
    bloom: Option<String>,
    #[cfg(feature = "hotcache")]
    hc: Option<std::rc::Rc<crate::hotcache::Hotcache>>,
}

#[cfg(feature = "splits")]
impl crate::splitset::SplitFetcher for WasmSplitResolver {
    type Fetch = WasmFetch;
    fn fetch_named(&self, name: &str) -> WasmFetch {
        WasmFetch::new(format!("{}/{}", self.base.trim_end_matches('/'), name))
    }

    fn global_bloom_name(&self) -> Option<String> {
        self.bloom.clone()
    }

    #[cfg(feature = "hotcache")]
    fn boot(&self, split: &crate::splitset::Split) -> Option<Vec<u8>> {
        self.hc
            .as_ref()?
            .inlined_by_name(&split.data_file)
            .map(<[u8]>::to_vec)
    }
}

/// Aggregated split-set facet counts as the JS `Array<{ field, cats: [{ name, count }] }>` the
/// demo's `applyFacetCounts` consumes — the same shape as `facets_array_js`, but from the
/// name-keyed [`crate::splitset::FieldCounts`] (split sets have no global category table).
#[cfg(feature = "splits")]
fn field_counts_to_js(fields: &[crate::splitset::FieldCounts]) -> JsValue {
    let out = Array::new_with_length(fields.len() as u32);
    for (fi, fc) in fields.iter().enumerate() {
        let cats = Array::new_with_length(fc.categories.len() as u32);
        for (ci, (name, count)) in fc.categories.iter().enumerate() {
            let obj = Object::new();
            let _ = Reflect::set(&obj, &"name".into(), &name.as_str().into());
            let _ = Reflect::set(&obj, &"count".into(), &(*count as f64).into());
            cats.set(ci as u32, obj.into());
        }
        let obj = Object::new();
        let _ = Reflect::set(&obj, &"field".into(), &fc.field.as_str().into());
        let _ = Reflect::set(&obj, &"cats".into(), &cats.into());
        out.set(fi as u32, obj.into());
    }
    out.into()
}

/// A range-fetchable `RRSS` split set exposed to JavaScript. Boots the manifest in two ranged
/// reads; each query opens (and prunes) the splits it needs, resolved as `base_url/<name>`.
#[cfg(feature = "splits")]
#[wasm_bindgen]
pub struct RrssIndex {
    inner: crate::splitset::SplitSet,
    base: String,
    /// Optional global term-Bloom sidecar name (see [`set_global_bloom`](Self::set_global_bloom)).
    bloom: Option<String>,
    /// The boot bundle, when booted via [`open_bundle`](Self::open_bundle); its inlined split
    /// boots let the query path open splits with no per-split header fetch. `Rc` so the
    /// per-search resolver clones a handle, not the resident blob.
    #[cfg(feature = "hotcache")]
    hc: Option<std::rc::Rc<crate::hotcache::Hotcache>>,
}

#[cfg(feature = "splits")]
#[wasm_bindgen]
impl RrssIndex {
    /// Boots the split-set manifest at `manifest_url`; per-split files (and the sort-column
    /// store, if any) are fetched from `base_url/<name>`. Each queried split cold-opens its own
    /// header; for the boot-bundle path that collapses those opens, see
    /// [`openBundle`](Self::open_bundle). Returns a `Promise<RrssIndex>`.
    pub async fn open(manifest_url: String, base_url: String) -> Result<RrssIndex, JsError> {
        let inner = crate::splitset::SplitSet::open(WasmFetch::new(manifest_url))
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrssIndex {
            inner,
            base: base_url,
            bloom: None,
            #[cfg(feature = "hotcache")]
            hc: None,
        })
    }

    /// Names a **global term-Bloom sidecar** (resolved as `base_url/<name>`, the
    /// `build_global_bloom` layout) covering the whole set's vocabulary. It is never
    /// downloaded: the tiered query path range-probes `k` byte positions per query
    /// term, and only after the top tier yields nothing — so an absent/typo term ends
    /// the tier descent in a handful of one-byte reads instead of opening every split,
    /// while present-term queries never touch it.
    #[wasm_bindgen(js_name = setGlobalBloom)]
    pub fn set_global_bloom(&mut self, name: String) {
        self.bloom = Some(name);
    }

    /// Boots the split set with an `RRHC` boot bundle: the manifest at `manifest_url` and the
    /// bundle at `rrhc_url` are fetched in **one parallel wave** (two GETs, one round trip of
    /// latency), then each split the query opens takes its boot from the bundle's inlined blob —
    /// no per-split header fetch. A split the bundle didn't inline falls back to a cold open, so
    /// the path degrades gracefully. Per-split data files still resolve as `base_url/<name>`.
    /// Returns a `Promise<RrssIndex>`.
    #[cfg(feature = "hotcache")]
    #[wasm_bindgen(js_name = openBundle)]
    pub async fn open_bundle(
        manifest_url: String,
        base_url: String,
        rrhc_url: String,
    ) -> Result<RrssIndex, JsError> {
        let (manifest, bundle) = futures::future::join(
            crate::splitset::SplitSet::open(WasmFetch::new(manifest_url)),
            crate::hotcache::Hotcache::open(WasmFetch::new(rrhc_url)),
        )
        .await;
        let inner = manifest.map_err(|e| JsError::new(&e.to_string()))?;
        let hc = bundle.map_err(|e| JsError::new(&e.to_string()))?;
        Ok(RrssIndex {
            inner,
            base: base_url,
            bloom: None,
            hc: Some(std::rc::Rc::new(hc)),
        })
    }

    /// Returns up to `limit` matching global doc IDs, ranked by policy (tiered short-circuit or
    /// stable-key sort, with delta supersession). Resolves to a `Uint32Array`.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, JsError> {
        let resolver = WasmSplitResolver {
            base: self.base.clone(),
            bloom: self.bloom.clone(),
            #[cfg(feature = "hotcache")]
            hc: self.hc.clone(),
        };
        self.inner
            .search(&resolver, query, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Like [`search`](Self::search) but ANDs a facet filter in. Args are `(query, limit,
    /// filters)` — `limit` adjacent to `query`, options trailing, matching
    /// `RrsIndex.searchCursorFiltered`. `filters` is an array of `[field, category]` pairs (within
    /// a field categories OR, across fields AND; a malformed entry throws); each surviving split's
    /// own `‹split›.rrf` sidecar resolves it, and a split lacking a selected field's categories is
    /// pruned without a fetch. An empty `filters` is exactly [`search`](Self::search).
    #[wasm_bindgen(js_name = searchFiltered)]
    pub async fn search_filtered(
        &self,
        query: &str,
        limit: usize,
        filters: Array,
    ) -> Result<Vec<u32>, JsError> {
        let pairs = filter_pairs(&filters)?;
        let resolver = WasmSplitResolver {
            base: self.base.clone(),
            bloom: self.bloom.clone(),
            #[cfg(feature = "hotcache")]
            hc: self.hc.clone(),
        };
        self.inner
            .search_filtered(&resolver, query, &pairs, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Per-(field, category) facet counts over `ids` (global doc IDs — typically a query's ranked
    /// result from `search`/`searchFiltered`), as a JS `Array<{ field, cats: [{ name, count }] }>`
    /// — the same shape the monolith's facet accessors return. Each contributing split's own
    /// `‹split›.rrf` sidecar is opened and counted; counts are summed by category name (split sets
    /// carry no global category table). Categories the result never hits are omitted (the demo
    /// renders missing keys as `0`). Resolves to that array.
    #[wasm_bindgen(js_name = facetCounts)]
    pub async fn facet_counts(&self, ids: Vec<u32>) -> Result<JsValue, JsError> {
        let resolver = WasmSplitResolver {
            base: self.base.clone(),
            bloom: self.bloom.clone(),
            #[cfg(feature = "hotcache")]
            hc: self.hc.clone(),
        };
        let counts = self
            .inner
            .facet_counts(&resolver, &ids)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(field_counts_to_js(&counts))
    }

    /// Number of splits named by the manifest (base + delta).
    #[wasm_bindgen(js_name = splitCount)]
    pub fn split_count(&self) -> usize {
        self.inner.splits().len()
    }

    /// Total documents the manifest's splits hold (Σ per-split doc counts) — what
    /// a tier-pruned manifest (e.g. the lite tier-prefix set) actually searches,
    /// as opposed to the record store's full corpus size.
    #[wasm_bindgen(js_name = docCount)]
    pub fn doc_count(&self) -> f64 {
        self.inner
            .splits()
            .iter()
            .map(|s| s.doc_count as u64)
            .sum::<u64>() as f64
    }

    /// Number of delta splits flushed since the base (0 for a base-only set).
    #[wasm_bindgen(js_name = deltaCount)]
    pub fn delta_count(&self) -> usize {
        self.inner.delta_splits().len()
    }

    /// Total on-S3 size of every split in bytes (the split set's footprint).
    #[wasm_bindgen(js_name = totalBytes)]
    pub fn total_bytes(&self) -> u64 {
        self.inner.total_byte_size()
    }

    /// Whether this set was booted from an `RRHC` boot bundle ([`openBundle`](Self::open_bundle)),
    /// i.e. its split boots are resident and split opens skip the per-split header GET.
    #[cfg(feature = "hotcache")]
    #[wasm_bindgen(js_name = hasBundle)]
    pub fn has_bundle(&self) -> bool {
        self.hc.is_some()
    }

    /// Number of split boots resident from the boot bundle (`0` when booted without one) — the
    /// count of per-split header GETs the bundle collapsed into its single GET.
    #[cfg(feature = "hotcache")]
    #[wasm_bindgen(js_name = bundledBootCount)]
    pub fn bundled_boot_count(&self) -> usize {
        self.hc
            .as_ref()
            .map(|hc| hc.members().iter().filter(|m| m.inlined).count())
            .unwrap_or(0)
    }
}
