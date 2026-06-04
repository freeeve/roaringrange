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
use crate::records::RecordStore;
use crate::secondary::{SecondaryCursor, SecondaryIndex};
use crate::sortcols::SortCols;
#[cfg(feature = "vector")]
use crate::vector::VectorIndex;
use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint32Array, Uint8Array};
use roaring::RoaringBitmap;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, RequestMode, Response};

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

/// Escapes `s` for embedding inside a JSON string literal: backslash and double
/// quote are backslash-escaped, the common control characters use their short
/// escapes, and any other control byte below `0x20` becomes a `\u00xx` escape.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl RangeFetch for WasmFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
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
        Ok(bytes)
    }
}

/// Builds the search-filtered facet-count JSON for `cursor`'s head result:
/// `[{"field":"<name>","cats":[{"name":"<name>","count":<n>},...]},...]`, or
/// `"[]"` when no facet sidecar is open. Counts are computed in memory.
fn facet_counts_json(facets: Option<&FacetIndex<WasmFetch>>, head: &RoaringBitmap) -> String {
    let Some(facets) = facets else {
        return "[]".to_string();
    };
    let counts = facets.counts(head);
    let mut out = String::from("[");
    for (fi, field) in facets.fields.iter().enumerate() {
        if fi > 0 {
            out.push(',');
        }
        out.push_str("{\"field\":\"");
        out.push_str(&json_escape(&field.name));
        out.push_str("\",\"cats\":[");
        for (ci, cat) in field.categories.iter().enumerate() {
            if ci > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":\"");
            out.push_str(&json_escape(&cat.name));
            out.push_str("\",\"count\":");
            out.push_str(&counts[fi][ci].to_string());
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push(']');
    out
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

    /// Returns the facet fields and their categories as a JSON string in the form
    /// `[{"field":"<name>","cats":[{"name":"<name>","count":<u32>},...]},...]`.
    /// Yields `"[]"` when no sidecar is open. Counts are full-corpus and free
    /// (served from the in-memory meta region).
    #[wasm_bindgen(js_name = facetsJson)]
    pub fn facets_json(&self) -> String {
        let Some(facets) = &self.facets else {
            return "[]".to_string();
        };
        let mut out = String::from("[");
        for (fi, field) in facets.fields.iter().enumerate() {
            if fi > 0 {
                out.push(',');
            }
            out.push_str("{\"field\":\"");
            out.push_str(&json_escape(&field.name));
            out.push_str("\",\"cats\":[");
            for (ci, cat) in field.categories.iter().enumerate() {
                if ci > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":\"");
                out.push_str(&json_escape(&cat.name));
                out.push_str("\",\"count\":");
                out.push_str(&cat.count.to_string());
                out.push('}');
            }
            out.push_str("]}");
        }
        out.push(']');
        out
    }

    /// Returns up to `limit` matching doc IDs, most-popular first. Resolves to a
    /// `Uint32Array` in JavaScript.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, JsError> {
        self.inner
            .search(query, limit)
            .await
            .map_err(|e| JsError::new(&e.to_string()))
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
        let counts_json = facet_counts_json(self.facets.as_ref(), inner.head_bitmap());
        Ok(RrsCursor { inner, counts_json })
    }

    /// Like [`RrsIndex::search_cursor`] but ANDs the selected facets into the
    /// result. Each `filters` entry is `"field\tcategory"` (tab-separated);
    /// within a field categories OR, across fields they AND. The filter is
    /// applied only when a sidecar is open and `filters` is non-empty. Resolves
    /// to an `RrsCursor`.
    #[wasm_bindgen(js_name = searchCursorFiltered)]
    pub async fn search_cursor_filtered(
        &self,
        query: String,
        max_missing: usize,
        filters: Vec<String>,
    ) -> Result<RrsCursor, JsError> {
        let filter = match &self.facets {
            Some(facets) if !filters.is_empty() => {
                let pairs: Vec<(String, String)> = filters
                    .iter()
                    .filter_map(|entry| {
                        let mut parts = entry.splitn(2, '\t');
                        match (parts.next(), parts.next()) {
                            (Some(field), Some(cat)) => Some((field.to_string(), cat.to_string())),
                            _ => None,
                        }
                    })
                    .collect();
                Some(facets.resolve(&pairs))
            }
            _ => None,
        };
        let inner = self
            .inner
            .search_cursor_filtered(&query, max_missing, filter)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let counts_json = facet_counts_json(self.facets.as_ref(), inner.head_bitmap());
        Ok(RrsCursor { inner, counts_json })
    }

    /// Number of n-grams in the index dictionary.
    #[wasm_bindgen(js_name = ngramCount)]
    pub fn ngram_count(&self) -> u32 {
        self.inner.ngram_count()
    }
}

/// A stateful pagination cursor exposed to JavaScript.
#[wasm_bindgen]
pub struct RrsCursor {
    inner: Cursor<WasmFetch>,
    /// Search-filtered facet counts for this query, as a JSON string (same shape
    /// as `facetsJson` but counts are restricted to this query's head result).
    counts_json: String,
}

#[wasm_bindgen]
impl RrsCursor {
    /// Returns the search-filtered facet counts as a JSON string in the form
    /// `[{"field":"<name>","cats":[{"name":"<name>","count":<n>},...]},...]` —
    /// how many of this query's results fall in each category. `"[]"` when no
    /// facet sidecar is open.
    #[wasm_bindgen(js_name = facetCountsJson)]
    pub fn facet_counts_json(&self) -> String {
        self.counts_json.clone()
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
    let Some(counts) = counts else {
        return JsValue::NULL;
    };
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
    out.into()
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
    /// `filters_json` is a JSON array of `[field, category]` pairs (e.g.
    /// `[["format","ebook"],["language","en"]]`); `null`, `""`, or `"[]"` means
    /// no filter. Within a field categories OR, across fields they AND. The page
    /// covers ranked doc IDs `[offset, offset+len)`; `max_missing` is the fuzzy
    /// tolerance (0 = strict). `records`/`facetCounts` are `null` unless the
    /// matching sidecar is attached.
    pub async fn search(
        &self,
        query: String,
        offset: usize,
        len: usize,
        max_missing: usize,
        filters_json: Option<String>,
    ) -> Result<JsValue, JsError> {
        let filter = parse_filters_json(filters_json.as_deref());
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

    /// Returns the facet fields and their full-corpus category counts as a JSON
    /// string `[{"field":"<name>","cats":[{"name":"<name>","count":<u32>},...]},...]`,
    /// or `"[]"` when no facet sidecar is attached. Mirrors [`RrsIndex::facets_json`].
    #[wasm_bindgen(js_name = facetsJson)]
    pub fn facets_json(&self) -> String {
        let fields = self.cat().fields();
        let mut out = String::from("[");
        for (fi, field) in fields.iter().enumerate() {
            if fi > 0 {
                out.push(',');
            }
            out.push_str("{\"field\":\"");
            out.push_str(&json_escape(&field.name));
            out.push_str("\",\"cats\":[");
            for (ci, cat) in field.categories.iter().enumerate() {
                if ci > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":\"");
                out.push_str(&json_escape(&cat.name));
                out.push_str("\",\"count\":");
                out.push_str(&cat.count.to_string());
                out.push('}');
            }
            out.push_str("]}");
        }
        out.push(']');
        out
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

/// Parses a JSON array of `[field, category]` string pairs into resolve input.
/// Tolerant: a `None`, empty, or unparseable input yields no selections, and
/// malformed entries are skipped. Avoids pulling in a JSON dependency by walking
/// the value with `js_sys::JSON`.
fn parse_filters_json(json: Option<&str>) -> Vec<(String, String)> {
    let Some(json) = json else {
        return Vec::new();
    };
    let trimmed = json.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Vec::new();
    }
    let Ok(value) = js_sys::JSON::parse(trimmed) else {
        return Vec::new();
    };
    let Ok(arr) = value.dyn_into::<Array>() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.length() as usize);
    for entry in arr.iter() {
        if let Ok(pair) = entry.dyn_into::<Array>() {
            if let (Some(field), Some(cat)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
                out.push((field, cat));
            }
        }
    }
    out
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
    #[wasm_bindgen(js_name = columnsJson)]
    pub fn columns_json(&self) -> String {
        let parts: Vec<String> = self
            .inner
            .columns()
            .iter()
            .map(|c| {
                let t = match c.value_type {
                    crate::sortcols::ValueType::U16 => "u16",
                    crate::sortcols::ValueType::U32 => "u32",
                    crate::sortcols::ValueType::I32 => "i32",
                    crate::sortcols::ValueType::F32 => "f32",
                };
                format!(
                    "{{\"name\":\"{}\",\"type\":\"{}\"}}",
                    json_escape(&c.name),
                    t
                )
            })
            .collect();
        format!("[{}]", parts.join(","))
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

/// Serializes facet `fields` with their **full-corpus** counts to the
/// `facetsJson` shape `[{"field":..,"cats":[{"name":..,"count":<u32>},..]},..]`.
fn fields_json(fields: &[Field]) -> String {
    let mut out = String::from("[");
    for (fi, field) in fields.iter().enumerate() {
        if fi > 0 {
            out.push(',');
        }
        out.push_str("{\"field\":\"");
        out.push_str(&json_escape(&field.name));
        out.push_str("\",\"cats\":[");
        for (ci, cat) in field.categories.iter().enumerate() {
            if ci > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":\"");
            out.push_str(&json_escape(&cat.name));
            out.push_str("\",\"count\":");
            out.push_str(&cat.count.to_string());
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// Parses `"field\tcategory"` filter entries into `(field, category)` pairs,
/// skipping malformed entries. Shared by the secondary filtered search.
fn parse_tab_filters(filters: &[String]) -> Vec<(String, String)> {
    filters
        .iter()
        .filter_map(|entry| {
            let mut parts = entry.splitn(2, '\t');
            match (parts.next(), parts.next()) {
                (Some(field), Some(cat)) => Some((field.to_string(), cat.to_string())),
                _ => None,
            }
        })
        .collect()
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

    /// The facet fields with full-corpus counts as a JSON string (same shape as
    /// [`RrsIndex::facets_json`]); `"[]"` when no sidecar is open.
    #[wasm_bindgen(js_name = facetsJson)]
    pub fn facets_json(&self) -> String {
        match self.inner.as_ref() {
            Some(sec) => fields_json(sec.fields()),
            None => "[]".to_string(),
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
        self.search_cursor_filtered(query, max_missing, Vec::new())
            .await
    }

    /// Like [`RrsSecondaryIndex::search_cursor`] but ANDs the selected facets into
    /// the result. Each `filters` entry is `"field\tcategory"` (tab-separated);
    /// within a field categories OR, across fields they AND. Applied only when a
    /// secondary sidecar is open and `filters` is non-empty.
    #[wasm_bindgen(js_name = searchCursorFiltered)]
    pub async fn search_cursor_filtered(
        &self,
        query: String,
        max_missing: usize,
        filters: Vec<String>,
    ) -> Result<RrsSecondaryCursor, JsError> {
        let sec = self
            .inner
            .as_ref()
            .ok_or_else(|| JsError::new("secondary index unavailable"))?;
        let pairs = parse_tab_filters(&filters);
        let inner = sec
            .search_cursor_filtered(&query, max_missing, &pairs)
            .await
            .map_err(|e| JsError::new(&e.to_string()))?;
        let counts_json = facet_counts_json(sec.facets(), inner.head_bitmap());
        Ok(RrsSecondaryCursor { inner, counts_json })
    }
}

/// A pagination cursor over a secondary-ordered result set whose pages are mapped
/// back to primary doc IDs. Mirrors [`RrsCursor`]; [`RrsSecondaryCursor::page`]
/// returns a `Uint32Array` of **primary** doc IDs.
#[wasm_bindgen]
pub struct RrsSecondaryCursor {
    inner: SecondaryCursor<WasmFetch>,
    /// Search-filtered facet counts for this query (secondary-space head result,
    /// identical to the primary order's counts), as a JSON string.
    counts_json: String,
}

#[wasm_bindgen]
impl RrsSecondaryCursor {
    /// The search-filtered facet counts as a JSON string (same shape as
    /// `facetsJson`, counts restricted to this query's result); `"[]"` when no
    /// secondary sidecar is open.
    #[wasm_bindgen(js_name = facetCountsJson)]
    pub fn facet_counts_json(&self) -> String {
        self.counts_json.clone()
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
