//! Regional **hybrid-search** Lambda — fuses a text arm with the Gemma semantic arm.
//!
//! One crate, deployed twice via the `TEXT_MODE` env: `trigram` (substring `RRS`) or `term`
//! (stemmed whole-word `RRTI` + BM25 `RRSB`) for the text arm. The vector arm embeds the query
//! with EmbeddingGemma **in-process** (onnxruntime, baked into the container image — no
//! cross-Lambda hop) and searches the Gemma `RRVI`; the two ranked lists are fused with
//! **reciprocal rank fusion** — `term + Gemma` is exactly "BM25 with semantic". Everything runs
//! in-region over S3 range reads; only fused page IDs + facet counts cross the wire.
//!
//! Env: `INDEX_BUCKET`, `TEXT_MODE` (trigram|term), `RRVI_KEY`, `INDEX_FACETS_KEY`, and
//! `EMBED_MODEL_DIR` (baked `model/`, default `/var/task/model`) + `ORT_DYLIB_PATH`; trigram
//! needs `TRIGRAM_KEY`, term needs `TERM_KEY` + `IMPACTS_KEY`.

mod embed;

use embed::Embedder;
use lambda_http::{run, service_fn, Body, Error, Request, RequestExt, Response};
use roaring::RoaringBitmap;
use roaringrange::{
    reciprocal_rank_fusion, search_bm25, FacetIndex, FetchError, ImpactIndex, Index, RangeFetch,
    TermIndex, VectorIndex,
};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// Ranked depth per arm before fusion (matches the demo's SEM_K).
const DEPTH: usize = 250;
/// IVFPQ probes (matches the demo's SEM_NPROBE).
const NPROBE: usize = 8;
/// Reciprocal-rank-fusion constant (matches the demo's RRF_K).
const RRF_K: f64 = 60.0;
/// BM25 candidate window for the term arm.
const BM25_M: usize = 2000;
/// Per-object in-container range cache cap. This function is the heaviest reader —
/// two index families (text + Gemma RRVI) plus facets per query — so it is sized to
/// the Lambda max (10 GiB), and an 8 GiB cap holds both arms' hot working sets.
const CACHE_CAP_BYTES: usize = 8 * 1024 * 1024 * 1024;

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

/// A byte-capped LRU cache of range reads keyed by `(offset, len)`, one per S3 object:
/// a `get` hit promotes the entry to most-recently-used, and the least-recently-used
/// entry is evicted once the cap is exceeded.
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
            if let Some(v) = self.cache.lock().unwrap().get(&key) {
                return Ok(v);
            }
        }
        let v = self.inner.read(offset, len).await?;
        self.cache.lock().unwrap().put(key, v.clone());
        Ok(v)
    }
}

type CF = CachedFetch<S3Fetch>;

/// The text arm, selected by `TEXT_MODE`.
enum TextArm {
    Trigram(Index<CF>),
    Term(TermIndex<CF>, ImpactIndex<CF>),
}

impl TextArm {
    /// Top-`k` text-matched doc IDs in static-rank (term: BM25-reranked) order.
    async fn search(&self, q: &str, k: usize) -> Result<Vec<u32>, roaringrange::IndexError> {
        match self {
            TextArm::Trigram(idx) => idx.search(q, k).await,
            TextArm::Term(terms, impacts) => Ok(search_bm25(terms, impacts, q, BM25_M, k)
                .await?
                .into_iter()
                .map(|d| d.doc_id)
                .collect()),
        }
    }
}

struct Hybrid {
    text: TextArm,
    vectors: VectorIndex<CF>,
    facets: FacetIndex<CF>,
    embedder: Embedder,
}

static HYBRID: OnceCell<Hybrid> = OnceCell::const_new();

async fn hybrid() -> Result<&'static Hybrid, Error> {
    HYBRID
        .get_or_try_init(|| async {
            let t0 = std::time::Instant::now();
            // Start the embedder load (ONNX session build, CPU-bound) on the blocking pool up
            // front so it overlaps the index opens (S3 I/O) below — both are cold-start costs.
            let model_dir = PathBuf::from(
                std::env::var("EMBED_MODEL_DIR").unwrap_or_else(|_| "/var/task/model".to_string()),
            );
            let embedder_task = tokio::task::spawn_blocking(move || Embedder::load(&model_dir));

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
            let mode = std::env::var("TEXT_MODE").map_err(|_| "TEXT_MODE not set")?;
            let text = match mode.as_str() {
                "trigram" => {
                    let key = std::env::var("TRIGRAM_KEY").map_err(|_| "TRIGRAM_KEY not set")?;
                    TextArm::Trigram(
                        Index::open(open(key))
                            .await
                            .map_err(|e| Error::from(format!("open trigram: {e}")))?,
                    )
                }
                "term" => {
                    let tk = std::env::var("TERM_KEY").map_err(|_| "TERM_KEY not set")?;
                    let ik = std::env::var("IMPACTS_KEY").map_err(|_| "IMPACTS_KEY not set")?;
                    let terms = TermIndex::open(open(tk))
                        .await
                        .map_err(|e| Error::from(format!("open term: {e}")))?;
                    let impacts = ImpactIndex::open(open(ik))
                        .await
                        .map_err(|e| Error::from(format!("open impacts: {e}")))?;
                    TextArm::Term(terms, impacts)
                }
                other => {
                    return Err(Error::from(format!(
                        "TEXT_MODE must be trigram|term, got {other}"
                    )))
                }
            };
            let rrvi = std::env::var("RRVI_KEY").map_err(|_| "RRVI_KEY not set")?;
            let facets_key =
                std::env::var("INDEX_FACETS_KEY").map_err(|_| "INDEX_FACETS_KEY not set")?;
            let vectors = VectorIndex::open(open(rrvi))
                .await
                .map_err(|e| Error::from(format!("open rrvi: {e}")))?;
            let facets = FacetIndex::open(open(facets_key))
                .await
                .map_err(|e| Error::from(format!("open facets: {e}")))?;
            let idx_secs = t0.elapsed().as_secs_f32();
            // Join the embedder load started above (overlapped with the index opens).
            let embedder = embedder_task
                .await
                .map_err(|e| Error::from(format!("embedder join: {e}")))?
                .map_err(|e| Error::from(format!("load embedder: {e}")))?;
            eprintln!(
                "hybrid init: indexes {idx_secs:.1}s, total {:.1}s",
                t0.elapsed().as_secs_f32()
            );
            Ok::<_, Error>(Hybrid {
                text,
                vectors,
                facets,
                embedder,
            })
        })
        .await
}

fn facets_value<F: RangeFetch>(fi: &FacetIndex<F>, counts: &[Vec<u64>]) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = fi
        .fields
        .iter()
        .zip(counts)
        .map(|(field, fc)| {
            let cats: Vec<serde_json::Value> = field
                .categories
                .iter()
                .zip(fc)
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

/// `GET ?q=&offset=&limit=&filters=` → `{total,more,offset,ids,facets}` — text ⊕ Gemma, RRF-fused.
async fn handler(event: Request) -> Result<Response<Body>, Error> {
    let params = event.query_string_parameters();
    let query = params.first("q").unwrap_or("").trim().to_string();
    let offset: usize = params
        .first("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .first("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(25)
        .min(500);
    let filters: Vec<(String, String)> = match params.first("filters") {
        Some(s) => match serde_json::from_str(s) {
            Ok(f) => f,
            Err(e) => return json_response(
                400,
                serde_json::json!({
                    "error": format!("filters must be a JSON array of [field, category] pairs: {e}")
                })
                .to_string(),
            ),
        },
        None => Vec::new(),
    };
    if query.is_empty() {
        return json_response(
            200,
            r#"{"total":0,"more":false,"offset":0,"ids":[],"facets":[]}"#.to_string(),
        );
    }

    let h = hybrid().await?;
    // Two arms in parallel: text (BM25/substring) and Gemma vector. The vector arm embeds
    // the query in-process (ONNX is CPU-bound, so on the blocking pool) then probes the RRVI.
    let (text_ids, vec_hits) = tokio::join!(h.text.search(&query, DEPTH), async {
        let q = query.clone();
        let qv = tokio::task::spawn_blocking(move || h.embedder.embed(&q))
            .await
            .map_err(|e| Error::from(format!("embed join: {e}")))?
            .map_err(|e| Error::from(format!("embed: {e}")))?;
        h.vectors
            .search(&qv, DEPTH, NPROBE)
            .await
            .map_err(|e| Error::from(format!("vector search: {e}")))
    });
    let text_ids = text_ids.map_err(|e| Error::from(format!("text search: {e}")))?;
    let vec_ids: Vec<u32> = vec_hits?.into_iter().map(|hit| hit.doc_id).collect();

    // Reciprocal rank fusion of the two ranked lists.
    let fused: Vec<u32> = reciprocal_rank_fusion(&[&text_ids, &vec_ids], RRF_K)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    // Facet counts over the fused result, then narrow the returned IDs by a membership read.
    let fused_bm: RoaringBitmap = fused.iter().copied().collect();
    let filtered: Vec<u32> = if filters.is_empty() {
        fused
    } else {
        let resolved = h.facets.resolve(&filters);
        if resolved.has_empty_arm() {
            Vec::new()
        } else {
            let mask = resolved
                .membership_bitmap(&fused_bm)
                .await
                .map_err(|e| Error::from(format!("facet membership: {e}")))?;
            fused.into_iter().filter(|id| mask.contains(*id)).collect()
        }
    };

    let page: Vec<u32> = filtered.iter().skip(offset).take(limit).copied().collect();
    let facets = if offset == 0 {
        facets_value(&h.facets, &h.facets.counts(&fused_bm))
    } else {
        serde_json::Value::Array(vec![])
    };

    json_response(
        200,
        serde_json::json!({
            "total": filtered.len(),
            "more": false,
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
