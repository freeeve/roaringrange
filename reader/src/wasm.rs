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

use crate::facet::FacetIndex;
use crate::fetch::{FetchError, RangeFetch};
use crate::index::{Cursor, Index};
use js_sys::{ArrayBuffer, Reflect, Uint8Array};
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
        let end = offset + len as u64 - 1;
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
        Ok(Uint8Array::new(&array_buffer).to_vec())
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
}
