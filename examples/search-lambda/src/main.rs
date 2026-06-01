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

/// Cap on the in-container range cache, per index object. A warm invocation uses
/// ~100 MB of the container's gigabytes, so a generous cap holds the hot trigram
/// postings + dictionary blocks across queries.
const CACHE_CAP_BYTES: usize = 512 * 1024 * 1024;

/// A byte-capped cache of range reads keyed by `(offset, len)`, FIFO-evicted once
/// the cap is exceeded. One per S3 object, so identical offsets in the `.rrs` and
/// `.rrf` never collide.
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

    fn get(&self, k: &(u64, usize)) -> Option<Vec<u8>> {
        self.map.get(k).cloned()
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
                .with_facets(facets)
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
        .fields
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
    let filters: Vec<(String, String)> = params
        .first("filters")
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let body = if query.is_empty() {
        r#"{"total":0,"offset":0,"ids":[],"facets":[]}"#.to_string()
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

        // The exact total needs the full tail intersection, and facet counts are
        // computed from the (tail-independent) head result. Both are stable across
        // the query's pages, so compute them once on page 0; the client caches them
        // and later pages stay cheap (just this page's IDs).
        let (total, facets) = if offset == 0 {
            cur.load_tail()
                .await
                .map_err(|e| Error::from(format!("load_tail: {e}")))?;
            let total = cur.loaded();
            let facets = match cat.facets() {
                Some(f) => facets_value(f, &f.counts(cur.head_bitmap())),
                None => serde_json::Value::Array(vec![]),
            };
            (total, facets)
        } else {
            (0, serde_json::Value::Array(vec![]))
        };
        let ids = cur
            .page(offset, limit)
            .await
            .map_err(|e| Error::from(format!("page: {e}")))?;
        serde_json::json!({ "total": total, "offset": offset, "ids": ids, "facets": facets })
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
