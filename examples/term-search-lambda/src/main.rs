//! Regional **term-search** Lambda — the whole-word sibling of `search-lambda`.
//!
//! Runs the roaringrange **term** reader (`TermIndex`) over in-region S3 range reads,
//! reranks with the BM25 impact sidecar (`ImpactIndex`, default-on), counts/filters
//! facets (`FacetIndex`), and returns just this page's IDs + facet counts. The heavy
//! reads — the ~76 MB resident term dictionary, the BM25 impacts, the facet postings —
//! never egress to the browser; only a small JSON (KB) crosses the wire. The
//! intersection, BM25 scoring, and facet counting are the *same* readers the WASM build
//! runs; only the `RangeFetch` differs (S3 here), so results are byte-identical.
//!
//! A **separate** function from the trigram `search-lambda`: it loads a different index
//! family (`.rrt` + `.rrb` + `.rrf`) with its own memory/cold-start profile and its own
//! query semantics (stemmed whole-word + BM25, not substring).
//!
//! Env: `INDEX_BUCKET`, `TERM_KEY` (the `.rrt`), `IMPACTS_KEY` (the `.rrb` BM25 sidecar),
//! `INDEX_FACETS_KEY` (the `.rrf`). Front with CloudFront for a same-origin path.

use lambda_http::{run, service_fn, Body, Error, Request, RequestExt, Response};
use roaring::RoaringBitmap;
use roaringrange::{search_bm25, FacetIndex, FetchError, ImpactIndex, RangeFetch, TermIndex};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// Ranked candidate window the term search materializes before paging — bounds the
/// per-query cost; the client pages within it (matches the demo's `SEM_K` depth).
const DEPTH: usize = 1000;
/// BM25 candidate window: the rerank scores the first `M` static-rank hits.
const BM25_M: usize = 2000;
/// Per-object in-container range cache cap. A warm invocation uses a few hundred MB of
/// the container's gigabytes, so a generous cap holds the resident term dictionary +
/// hot postings + impacts across queries.
const CACHE_CAP_BYTES: usize = 512 * 1024 * 1024;

/// A [`RangeFetch`] over in-region S3 byte-range reads (in-region reads are free/fast,
/// so the heavy posting traffic never leaves the bucket's region). One per object.
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

/// A byte-capped FIFO cache of range reads keyed by `(offset, len)`, one per S3 object.
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

/// Wraps a [`RangeFetch`] with an in-memory [`RangeCache`] so a warm container re-serves
/// a posting / dictionary block / impact stripe it already read instead of re-hitting S3.
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

/// The three readers, opened once per warm container and reused across invocations.
struct TermSearch {
    term: TermIndex<CachedFetch<S3Fetch>>,
    impacts: ImpactIndex<CachedFetch<S3Fetch>>,
    facets: FacetIndex<CachedFetch<S3Fetch>>,
}

static SEARCH: OnceCell<TermSearch> = OnceCell::const_new();

async fn search() -> Result<&'static TermSearch, Error> {
    SEARCH
        .get_or_try_init(|| async {
            let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_s3::Client::new(&cfg);
            let bucket = std::env::var("INDEX_BUCKET").map_err(|_| "INDEX_BUCKET not set")?;
            let open = |key: String| {
                CachedFetch::new(
                    S3Fetch {
                        client: client.clone(),
                        bucket: bucket.clone(),
                        key,
                    },
                    CACHE_CAP_BYTES,
                )
            };
            let term_key = std::env::var("TERM_KEY").map_err(|_| "TERM_KEY not set")?;
            let impacts_key = std::env::var("IMPACTS_KEY").map_err(|_| "IMPACTS_KEY not set")?;
            let facets_key =
                std::env::var("INDEX_FACETS_KEY").map_err(|_| "INDEX_FACETS_KEY not set")?;
            let term = TermIndex::open(open(term_key))
                .await
                .map_err(|e| Error::from(format!("open term index: {e}")))?;
            let impacts = ImpactIndex::open(open(impacts_key))
                .await
                .map_err(|e| Error::from(format!("open impacts: {e}")))?;
            let facets = FacetIndex::open(open(facets_key))
                .await
                .map_err(|e| Error::from(format!("open facets: {e}")))?;
            Ok::<_, Error>(TermSearch {
                term,
                impacts,
                facets,
            })
        })
        .await
}

/// Per-query facet-count JSON, the shape the demo client consumes:
/// `[{"field":name,"cats":[{"name":cat,"count":n}, …]}, …]`. Only non-zero categories.
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

fn json_response(status: u16, body: String) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(Body::from(body))?)
}

/// `GET ?q=<query>&offset=<n>&limit=<n>&bm25=<0|1>&filters=<json>` →
/// `{"total":N,"more":bool,"offset":O,"ids":[…],"facets":[…]}`. Whole-word (stemmed)
/// term search, BM25-reranked by default (`bm25=0` opts out). `filters` is a JSON array
/// of `[field,category]` pairs (within-field OR, across-field AND). `total`/`facets` are
/// returned on `offset` 0 (the client caches both across the query's pages).
async fn handler(event: Request) -> Result<Response<Body>, Error> {
    let params = event.query_string_parameters();
    let query = params.first("q").unwrap_or("").trim().to_string();
    let offset: usize = params.first("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = params
        .first("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
        .min(500);
    let bm25 = params.first("bm25").map(|s| s != "0").unwrap_or(true);
    let filters: Vec<(String, String)> = match params.first("filters") {
        Some(s) => match serde_json::from_str(s) {
            Ok(f) => f,
            Err(e) => {
                return json_response(
                    400,
                    serde_json::json!({
                        "error": format!("filters must be a JSON array of [field, category] pairs: {e}")
                    })
                    .to_string(),
                )
            }
        },
        None => Vec::new(),
    };

    if query.is_empty() {
        return json_response(
            200,
            r#"{"total":0,"more":false,"offset":0,"ids":[],"facets":[]}"#.to_string(),
        );
    }

    let s = search().await?;

    // Ranked candidate window (BM25-reranked by default), in static-rank order within ties.
    let ranked: Vec<u32> = if bm25 {
        search_bm25(&s.term, &s.impacts, &query, BM25_M, DEPTH)
            .await
            .map_err(|e| Error::from(format!("bm25 search: {e}")))?
            .into_iter()
            .map(|d| d.doc_id)
            .collect()
    } else {
        s.term
            .search(&query, DEPTH)
            .await
            .map_err(|e| Error::from(format!("term search: {e}")))?
    };
    let capped = ranked.len() >= DEPTH;

    // Facet counts over the query result (pre-filter) so the client can refine; the
    // filter then narrows the returned IDs via a membership read (only the candidates'
    // 64K-doc buckets, container-granularity — never the whole category posting).
    let ranked_bm: RoaringBitmap = ranked.iter().copied().collect();
    let filtered: Vec<u32> = if filters.is_empty() {
        ranked.clone()
    } else {
        let resolved = s.facets.resolve(&filters);
        if resolved.has_empty_arm() {
            Vec::new()
        } else {
            let mask = resolved
                .membership_bitmap(&ranked_bm)
                .await
                .map_err(|e| Error::from(format!("facet membership: {e}")))?;
            ranked.into_iter().filter(|id| mask.contains(*id)).collect()
        }
    };

    let page: Vec<u32> = filtered
        .iter()
        .skip(offset)
        .take(limit)
        .copied()
        .collect();
    let facets = if offset == 0 {
        facets_value(&s.facets, &s.facets.counts(&ranked_bm))
    } else {
        serde_json::Value::Array(vec![])
    };

    json_response(
        200,
        serde_json::json!({
            "total": filtered.len(),
            "more": capped,
            "offset": offset,
            "ids": page,
            "facets": facets,
            "bound": serde_json::Value::Null,
        })
        .to_string(),
    )
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}
