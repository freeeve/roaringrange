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
//! Build: `wasm-pack build --target web --features wasm`. The host serving the
//! index must support HTTP Range requests; pass the index URL to
//! [`RrsIndex::open`].

use crate::catalog::Catalog;
use crate::facet::{FacetIndex, Field};
use crate::fetch::{FetchError, RangeFetch};
use crate::index::{Cursor, Index};
use crate::records::RecordStore;
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
fn facet_counts_json(facets: &Option<FacetIndex<WasmFetch>>, cursor: &Cursor<WasmFetch>) -> String {
    let Some(facets) = facets else {
        return "[]".to_string();
    };
    let counts = facets.counts(cursor.head_bitmap());
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
        let counts_json = facet_counts_json(&self.facets, &inner);
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
        let counts_json = facet_counts_json(&self.facets, &inner);
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
