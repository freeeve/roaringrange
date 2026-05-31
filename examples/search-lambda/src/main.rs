//! Regional search Lambda.
//!
//! Runs the roaringrange reader over **in-region** S3 range reads and returns
//! result IDs, so an explicit "load full results" never egresses the multi-MB
//! postings to the browser — the postings stay inside the bucket's region and
//! only a small ID list (KB) crosses the wire. The intersection is the *same*
//! `Index::search` the WASM build uses; only the `RangeFetch` differs (S3 here,
//! `fetch()` in the browser), so results are byte-identical.
//!
//! Env: `INDEX_BUCKET`, `INDEX_KEY` (the `.rrs` object). Front with CloudFront
//! for a same-origin path + per-query caching; the client calls it only on a
//! full-results request, so the popular head stays fully client-side.

use lambda_http::{run, service_fn, Body, Error, Request, RequestExt, Response};
use roaringrange::{FetchError, Index, RangeFetch};
use tokio::sync::OnceCell;

/// An [`Index`] backed by S3 byte-range reads. In-region reads are free and fast,
/// so the heavy posting traffic never leaves the bucket's region.
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

/// The index is opened once per warm container (boot reads the header + sparse
/// index), then reused across invocations; each query is just a few ranged reads.
static INDEX: OnceCell<Index<S3Fetch>> = OnceCell::const_new();

async fn index() -> Result<&'static Index<S3Fetch>, Error> {
    INDEX
        .get_or_try_init(|| async {
            let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_s3::Client::new(&cfg);
            let bucket = std::env::var("INDEX_BUCKET").map_err(|_| "INDEX_BUCKET not set")?;
            let key = std::env::var("INDEX_KEY").map_err(|_| "INDEX_KEY not set")?;
            Index::open(S3Fetch {
                client,
                bucket,
                key,
            })
            .await
            .map_err(|e| Error::from(format!("open index: {e}")))
        })
        .await
}

/// `GET ?q=<query>&max_missing=<n>&limit=<n>` → `{"total":N,"ids":[...]}`.
async fn handler(event: Request) -> Result<Response<Body>, Error> {
    let params = event.query_string_parameters();
    let query = params.first("q").unwrap_or("").trim();
    let max_missing: usize = params
        .first("max_missing")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .first("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);

    let body = if query.is_empty() {
        r#"{"total":0,"ids":[]}"#.to_string()
    } else {
        let idx = index().await?;
        // Cursor materializes the full result (head + tail) so `total` is exact;
        // we return up to `limit` ids — the browser paginates them and fetches
        // records per page. Strict AND when max_missing == 0; fuzzy otherwise.
        let mut cur = idx
            .search_cursor(query, max_missing)
            .await
            .map_err(|e| Error::from(format!("search: {e}")))?;
        let ids = cur
            .page(0, limit)
            .await
            .map_err(|e| Error::from(format!("page: {e}")))?;
        serde_json::json!({ "total": cur.loaded(), "ids": ids }).to_string()
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
