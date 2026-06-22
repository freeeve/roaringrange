//! Regional search Lambda.
//!
//! Runs the roaringrange reader over **in-region** S3 range reads and returns
//! result IDs + facet counts, so a full-results request never egresses the
//! multi-MB postings to the browser — they stay inside the bucket's region and
//! only a small JSON (KB) crosses the wire. The intersection and facet counting
//! are the *same* `Index`/`FacetIndex` the WASM build uses; only the `RangeFetch`
//! differs (S3 here, `fetch()` in the browser), so results are byte-identical.
//!
//! Env: `INDEX_BUCKET`, `INDEX_KEY` (the `.rrs` object), `INDEX_FACETS_KEY` (the
//! `.rrf` facet sidecar). Front with CloudFront for a same-origin path + per-query
//! caching; the client calls it only on a full-results request, so the popular
//! head stays fully client-side.

use lambda_http::{run, service_fn, Body, Error, Request, RequestExt, Response};
use roaringrange::{Catalog, FacetIndex, FetchError, RangeFetch};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// A reader resource backed by S3 byte-range reads. In-region reads are free and
/// fast, so the heavy posting traffic never leaves the bucket's region. One per
/// object (the index and the facet sidecar are distinct keys).
#[derive(Clone)]
struct S3Fetch {
    client: aws_sdk_s3::Client,
    bucket: String,
    key: String,
}

impl RangeFetch for S3Fetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .range(range)
            .send()
            .await
            .map_err(|e| FetchError::Transport(format!("s3 get_object: {e}")))?;
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| FetchError::Transport(format!("s3 body: {e}")))?;
        Ok(body.into_bytes().to_vec())
    }
}

/// Cap on the in-container range cache, per index object. The function is sized to
/// the Lambda max (10 GiB), so an 8 GiB cap keeps the hot trigram postings +
/// dictionary blocks resident across queries while leaving runtime headroom.
const CACHE_CAP_BYTES: usize = 8 * 1024 * 1024 * 1024;

/// A byte-capped LRU cache of range reads keyed by `(offset, len)`: a `get` hit
/// promotes the entry to most-recently-used, and the least-recently-used entry is
/// evicted once the cap is exceeded. One per S3 object, so identical offsets in the
/// `.rrs` and `.rrf` never collide.
struct RangeCache {
    map: HashMap<(u64, usize), Vec<u8>>,
    order: VecDeque<(u64, usize)>,
    bytes: usize,
    cap: usize,
}

impl RangeCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap,
        }
    }

    fn get(&mut self, k: &(u64, usize)) -> Option<Vec<u8>> {
        let v = self.map.get(k)?.clone();
        if let Some(pos) = self.order.iter().position(|x| x == k) {
            self.order.remove(pos);
        }
        self.order.push_back(*k);
        Some(v)
    }

    fn put(&mut self, k: (u64, usize), v: Vec<u8>) {
        if self.map.contains_key(&k) {
            return;
        }
        self.bytes += v.len();
        self.order.push_back(k);
        self.map.insert(k, v);
        while self.bytes > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    if let Some(ov) = self.map.remove(&old) {
                        self.bytes -= ov.len();
                    }
                }
                None => break,
            }
        }
    }
}

/// Wraps a [`RangeFetch`] with an in-memory [`RangeCache`], so a warm container
/// re-serves a posting (or dictionary block) it already read instead of issuing
/// another S3 GET. The intersection re-reads the same trigram postings on every
/// query, so common and overlapping queries skip the S3 round-trips entirely.
#[derive(Clone)]
struct CachedFetch<F> {
    inner: F,
    cache: Arc<Mutex<RangeCache>>,
}

impl<F> CachedFetch<F> {
    fn new(inner: F, cap: usize) -> Self {
        Self {
            inner,
            cache: Arc::new(Mutex::new(RangeCache::new(cap))),
        }
    }
}

impl<F: RangeFetch> RangeFetch for CachedFetch<F> {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let key = (offset, len);
        {
            // Scope the lock so it is never held across the await below.
            if let Some(v) = self.cache.lock().unwrap().get(&key) {
                return Ok(v);
            }
        }
        let v = self.inner.read(offset, len).await?;
        self.cache.lock().unwrap().put(key, v.clone());
        Ok(v)
    }
}

/// The catalog (text index + facet sidecar) is opened once per warm container
/// (boot reads the index header + sparse dictionary and the facet meta + category
/// heads), then reused across invocations; each query is just a few ranged reads.
static CATALOG: OnceCell<Catalog<CachedFetch<S3Fetch>>> = OnceCell::const_new();

async fn catalog() -> Result<&'static Catalog<CachedFetch<S3Fetch>>, Error> {
    CATALOG
        .get_or_try_init(|| async {
            let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_s3::Client::new(&cfg);
            let bucket = std::env::var("INDEX_BUCKET").map_err(|_| "INDEX_BUCKET not set")?;
            let index_key = std::env::var("INDEX_KEY").map_err(|_| "INDEX_KEY not set")?;
            let facets_key =
                std::env::var("INDEX_FACETS_KEY").map_err(|_| "INDEX_FACETS_KEY not set")?;
            let index = CachedFetch::new(
                S3Fetch {
                    client: client.clone(),
                    bucket: bucket.clone(),
                    key: index_key,
                },
                CACHE_CAP_BYTES,
            );
            let facets = CachedFetch::new(
                S3Fetch {
                    client,
                    bucket,
                    key: facets_key,
                },
                CACHE_CAP_BYTES,
            );
            Catalog::open(index)
                .await
                .map_err(|e| Error::from(format!("open index: {e}")))?
                .load_facets(facets)
                .await
                .map_err(|e| Error::from(format!("open facets: {e}")))
        })
        .await
}

/// Builds the per-query facet-count JSON in the shape the demo client consumes:
/// `[{"field":name,"cats":[{"name":cat,"count":n}, …]}, …]`. Only non-zero
/// categories are emitted (the client treats any it doesn't see as zero), so the
/// payload stays small. Aligned with `Catalog::fields()`.
fn facets_value<F: RangeFetch>(fi: &FacetIndex<F>, counts: &[Vec<u64>]) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = fi
        .fields()
        .iter()
        .zip(counts)
        .map(|(field, field_counts)| {
            let cats: Vec<serde_json::Value> = field
                .categories
                .iter()
                .zip(field_counts)
                .filter(|(_, &n)| n > 0)
                .map(|(c, &n)| serde_json::json!({ "name": c.name, "count": n }))
                .collect();
            serde_json::json!({ "field": field.name, "cats": cats })
        })
        .collect();
    serde_json::Value::Array(groups)
}

/// `GET ?q=<query>&offset=<n>&limit=<n>&max_missing=<n>&filters=<json>` →
/// `{"total":N,"offset":O,"ids":[…],"facets":[…]}`. `filters` is a JSON array of
/// `[field,category]` pairs (within-field OR, across-field AND). `total` and
/// `facets` are returned only on `offset` 0 (the client caches both across the
/// query's pages); later pages omit the full tail intersection and stay cheap.
async fn handler(event: Request) -> Result<Response<Body>, Error> {
    let params = event.query_string_parameters();
    let query = params.first("q").unwrap_or("").trim();
    let max_missing: usize = params
        .first("max_missing")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let offset: usize = params
        .first("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .first("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
        .min(500);
    // Malformed filters are a hard 400: silently dropping them would serve the
    // full unfiltered result set (ids, total, AND facet counts) labeled as the
    // client's filtered query, with no signal anywhere.
    let filters: Vec<(String, String)> = match params.first("filters") {
        Some(s) => match serde_json::from_str(s) {
            Ok(f) => f,
            Err(e) => {
                return Ok(Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .header("access-control-allow-origin", "*")
                    .body(Body::from(
                        serde_json::json!({
                            "error": format!("filters must be a JSON array of [field, category] pairs: {e}")
                        })
                        .to_string(),
                    ))?)
            }
        },
        None => Vec::new(),
    };

    let body = if query.is_empty() {
        r#"{"total":0,"more":false,"offset":0,"ids":[],"facets":[]}"#.to_string()
    } else {
        let cat = catalog().await?;
        // The intersection runs here, in-region; the browser only ever sees IDs +
        // facet counts. Strict AND when max_missing == 0; fuzzy threshold otherwise.
        let resolved = cat
            .facets()
            .filter(|_| !filters.is_empty())
            .map(|f| f.resolve(&filters));
        let mut cur = cat
            .index()
            .search_cursor_filtered(query, max_missing, resolved)
            .await
            .map_err(|e| Error::from(format!("search: {e}")))?;

        // This page's ids first: the v3 cursor pages the tail incrementally (only
        // the container buckets the page spans), so every request — any offset —
        // stays well inside API Gateway's hard 30-second cap. The old design ran
        // the FULL tail intersection on page 0 for an exact total, which the 484M
        // corpus blew past that cap on common-trigram queries.
        // The cursor's tail scan returns BOUNDED work per call (a first-paint bias for
        // interactive clients that page a persistent cursor repeatedly). This Lambda is
        // stateless — a fresh cursor per request — so a single `page` call surfaces only
        // the first sliver of the tail and an `offset > 0` page comes back empty. Drive
        // the scan until THIS page is filled (or the tail is exhausted), so every offset
        // returns its full slice. Bounded by `offset + limit`, not the whole tail, so it
        // stays well inside API Gateway's 30s cap for paginated requests.
        let mut ids = cur
            .page(offset, limit)
            .await
            .map_err(|e| Error::from(format!("page: {e}")))?;
        while ids.len() < limit && cur.pending_tail() {
            ids = cur
                .page(offset, limit)
                .await
                .map_err(|e| Error::from(format!("page: {e}")))?;
        }
        // total = results materialized so far (a lower bound while `more`), the
        // same incremental contract the client-side cursor renders as "N+".
        let total = cur.loaded();
        let more = cur.pending_tail();

        // Page 0 extras, all KB-scale: facet counts from the (tail-independent)
        // head result, and the header-derived count — exact for a single-trigram
        // unfiltered query, otherwise an upper bound (Σ per-container cardinalities
        // from posting headers; not valid for fuzzy matching).
        let (facets, bound) = if offset == 0 {
            let facets = match cat.facets() {
                Some(f) => facets_value(f, &f.counts(cur.head_bitmap())),
                None => serde_json::Value::Array(vec![]),
            };
            let bound = if max_missing == 0 {
                match cat.index().count_estimate(query).await {
                    Ok((count, exact)) => {
                        serde_json::json!({ "count": count, "exact": exact && filters.is_empty() })
                    }
                    Err(_) => serde_json::Value::Null,
                }
            } else {
                serde_json::Value::Null
            };
            (facets, bound)
        } else {
            (serde_json::Value::Array(vec![]), serde_json::Value::Null)
        };
        serde_json::json!({
            "total": total, "more": more, "offset": offset,
            "ids": ids, "facets": facets, "bound": bound
        })
        .to_string()
    };

    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(Body::from(body))?)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}
