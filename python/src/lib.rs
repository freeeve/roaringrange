//! Python bindings for building roaringrange datasets.
//!
//! Exposes a high-level [`Builder`] that turns a collection of records into the
//! four static files the WASM/Go reader serves over HTTP Range — the `RRS` text
//! index, the `RRSF` facet sidecar, and the `RRSR` record store (`.idx`/`.bin`).
//! The heavy lifting (posting layout, facet sidecar, record framing, n-gram key
//! derivation) is reused from the core `roaringrange` crate so the output
//! is byte-identical to the Go and Rust builders.
//!
//! ```python
//! import roaringrange as rr, json
//! b = rr.Builder(gram_size=3)
//! for row in rows:
//!     b.add(rank=row["citations"],                       # higher rank = listed first
//!           text=f'{row["title"]} {row["abstract"]}',    # tokenized into trigrams
//!           record=json.dumps(row).encode(),             # opaque bytes, your encoding
//!           facets={"year": [str(row["year"])], "type": [row["type"]]})
//! stats = b.build("out/")   # writes out/index.rrs, index.rrf, records.idx, records.bin
//! ```

use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use roaring::RoaringBitmap;
use roaringrange_core::build::{
    serialize_posting, split_posting, write_facets, write_index, write_records, FacetCategory,
    FacetField, DEFAULT_HEAD_BOUNDARY,
};
use roaringrange_core::ngram_keys;
use roaringrange_core::vector::{METRIC_IP, METRIC_L2};
use roaringrange_core::write_term_index as core_write_term_index;
use roaringrange_core::{
    build_ivfpq, build_ivfpq_from_parts, IvfpqParams, IvfpqParts, VectorBuildError,
};
use roaringrange_core::{Language, TermIndexBuilder, TermIndexConfig};
use roaringrange_core::{
    Policy as CorePolicy, SortColSpec as CoreSortColSpec, SplitBuildConfig,
    SplitSet as CoreSplitSet, SplitSetBuilder as CoreSplitSetBuilder,
    SplitSetWriter as CoreSplitSetWriter, TermSplitBuildConfig,
    TermSplitSetBuilder as CoreTermSplitSetBuilder, WriterConfig,
};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

/// One staged record: its rank (doc IDs are assigned in descending rank, so the
/// top-ranked records sit in every posting's head), the text to index, the
/// opaque record bytes, and its facet memberships (field → categories).
struct Doc {
    rank: i64,
    text: String,
    record: Vec<u8>,
    facets: Vec<(String, Vec<String>)>,
}

/// Summary of a completed build, returned by [`Builder::build`].
#[pyclass]
struct BuildStats {
    /// Number of records written.
    #[pyo3(get)]
    docs: usize,
    /// Number of distinct n-gram keys in the text index.
    #[pyo3(get)]
    ngrams: usize,
    /// Number of facet fields written.
    #[pyo3(get)]
    fields: usize,
}

#[pymethods]
impl BuildStats {
    fn __repr__(&self) -> String {
        format!(
            "BuildStats(docs={}, ngrams={}, fields={})",
            self.docs, self.ngrams, self.fields
        )
    }
}

/// Accumulates records, then writes a complete roaringrange dataset.
#[pyclass]
struct Builder {
    gram_size: u16,
    head_boundary: u32,
    docs: Vec<Doc>,
}

#[pymethods]
impl Builder {
    /// Creates a builder. `gram_size` is the trigram window (3 is the usual
    /// choice; it must match what the reader queries with). `head_boundary` is the
    /// doc-ID split between the head posting (the top-ranked docs, fetched first)
    /// and the tail; it must be a multiple of 65536 and defaults to 65536.
    #[new]
    #[pyo3(signature = (gram_size = 3, head_boundary = 65536))]
    fn new(gram_size: u16, head_boundary: u32) -> Self {
        Builder {
            gram_size: gram_size.max(1),
            head_boundary: head_boundary.max(DEFAULT_HEAD_BOUNDARY),
            docs: Vec::new(),
        }
    }

    /// Stages one record. `rank` orders results (higher first); `text` is
    /// tokenized into n-gram keys; `record` is the opaque bytes returned for a
    /// hit (use any encoding — JSON, msgpack, …); `facets` maps each field to the
    /// categories this record belongs to (e.g. `{"year": ["2020"]}`).
    #[pyo3(signature = (rank, text, record, facets = None))]
    fn add(
        &mut self,
        rank: i64,
        text: String,
        record: Vec<u8>,
        facets: Option<HashMap<String, Vec<String>>>,
    ) {
        self.docs.push(Doc {
            rank,
            text,
            record,
            facets: facets.map(|m| m.into_iter().collect()).unwrap_or_default(),
        });
    }

    /// Stages a batch of records in one call — each `rows` entry is
    /// `(rank, text, record, facets)` with the same meaning as [`add`](Self::add) (the trailing
    /// `facets` is optional). Cuts the per-row Python↔Rust call overhead for large corpora.
    #[allow(clippy::type_complexity)]
    fn add_many(
        &mut self,
        rows: Vec<(i64, String, Vec<u8>, Option<HashMap<String, Vec<String>>>)>,
    ) {
        self.docs.reserve(rows.len());
        for (rank, text, record, facets) in rows {
            self.docs.push(Doc {
                rank,
                text,
                record,
                facets: facets.map(|m| m.into_iter().collect()).unwrap_or_default(),
            });
        }
    }

    /// Number of staged records.
    fn __len__(&self) -> usize {
        self.docs.len()
    }

    /// Builds the dataset into `out_dir`, writing `index.rrs`, `index.rrf`,
    /// `records.idx`, and `records.bin`. Doc IDs are assigned in descending rank,
    /// text is tokenized into the index, and facet memberships become the sidecar.
    fn build(&self, out_dir: &str) -> PyResult<BuildStats> {
        let dir = Path::new(out_dir);
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let n = self.docs.len();

        // Assign doc IDs in descending rank (stable, so equal ranks keep insertion
        // order); the record at order[i] becomes doc ID i.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| self.docs[b].rank.cmp(&self.docs[a].rank));

        let mut index: HashMap<u64, RoaringBitmap> = HashMap::new();
        let mut facet_index: BTreeMap<String, BTreeMap<String, RoaringBitmap>> = BTreeMap::new();
        let mut records: Vec<Vec<u8>> = vec![Vec::new(); n];

        for (doc_id, &orig) in order.iter().enumerate() {
            let did = doc_id as u32;
            let doc = &self.docs[orig];
            for key in ngram_keys(&doc.text, self.gram_size as usize) {
                index.entry(key).or_default().insert(did);
            }
            for (field, cats) in &doc.facets {
                let f = facet_index.entry(field.clone()).or_default();
                for cat in cats {
                    f.entry(cat.clone()).or_default().insert(did);
                }
            }
            records[doc_id] = doc.record.clone();
        }

        // RRS text index (v3: one posting per term, no head/tail split).
        let entries: Vec<(u64, Vec<u8>)> = index
            .into_iter()
            .map(|(key, bm)| (key, serialize_posting(&bm)))
            .collect();
        let ngrams = entries.len();
        write_index(
            File::create(dir.join("index.rrs")).map_err(io_err)?,
            self.gram_size,
            0,
            entries,
        )
        .map_err(io_err)?;

        // RRSF facet sidecar.
        let fields: Vec<FacetField> = facet_index
            .into_iter()
            .map(|(name, cats)| FacetField {
                name,
                cats: cats
                    .into_iter()
                    .map(|(name, bm)| {
                        let card = bm.len() as u32;
                        let (head, tail) = split_posting(&bm, self.head_boundary);
                        FacetCategory {
                            name,
                            card,
                            head,
                            tail,
                        }
                    })
                    .collect(),
            })
            .collect();
        let nfields = fields.len();
        write_facets(File::create(dir.join("index.rrf")).map_err(io_err)?, fields)
            .map_err(io_err)?;

        // RRSR record store.
        write_records(
            File::create(dir.join("records.bin")).map_err(io_err)?,
            File::create(dir.join("records.idx")).map_err(io_err)?,
            &records,
        )
        .map_err(io_err)?;

        Ok(BuildStats {
            docs: n,
            ngrams,
            fields: nfields,
        })
    }
}

/// Returns the deduplicated n-gram keys `text` tokenizes into — the same keys the
/// index is built from and the reader queries with. Handy for debugging matches.
#[pyfunction]
#[pyo3(signature = (text, gram_size = 3))]
fn tokenize(text: &str, gram_size: usize) -> Vec<u64> {
    ngram_keys(text, gram_size)
}

/// Summary of a completed vector-index build, returned by [`VectorBuilder::build`].
#[pyclass]
struct VectorBuildStats {
    /// Number of vectors indexed.
    #[pyo3(get)]
    vectors: usize,
    /// Vector dimensionality.
    #[pyo3(get)]
    dim: usize,
    /// Coarse (IVF) clusters actually trained (clamped to the vector count).
    #[pyo3(get)]
    nlist: usize,
    /// PQ subquantizers.
    #[pyo3(get)]
    m: usize,
    /// Bits per PQ code.
    #[pyo3(get)]
    nbits: u8,
}

#[pymethods]
impl VectorBuildStats {
    fn __repr__(&self) -> String {
        format!(
            "VectorBuildStats(vectors={}, dim={}, nlist={}, m={}, nbits={})",
            self.vectors, self.dim, self.nlist, self.m, self.nbits
        )
    }
}

/// Accumulates `(doc_id, vector)` pairs, then trains and writes an `RRVI`
/// similarity (vector) index — the range-fetchable sibling of the `RRS` text
/// index. `doc_id` must equal the text index's doc ID so hits map to the same
/// records. Wraps the core IVFPQ trainer; see `VECTORS.md` for the byte layout.
#[pyclass]
struct VectorBuilder {
    params: IvfpqParams,
    vectors: Vec<(u32, Vec<f32>)>,
}

#[pymethods]
impl VectorBuilder {
    /// Creates a vector-index builder. `dim` is the (fixed) vector dimensionality;
    /// `nlist` the number of coarse clusters (a good default is `≈ 4·√N`, and it is
    /// clamped to the vector count); `m` the number of PQ subquantizers (must
    /// divide `dim`). `nbits` (1–8) sets `2^nbits` codes per subspace; `metric` is
    /// `"ip"`/`"cosine"` (inner product on L2-normalized vectors) or `"l2"`;
    /// `kmeans_iters` and `seed` make the (deterministic) training reproducible.
    #[new]
    #[pyo3(signature = (dim, nlist, m, nbits = 8, metric = "ip", kmeans_iters = 25, seed = None))]
    fn new(
        dim: usize,
        nlist: usize,
        m: usize,
        nbits: u8,
        metric: &str,
        kmeans_iters: usize,
        seed: Option<u64>,
    ) -> PyResult<Self> {
        if dim == 0 || m == 0 || !dim.is_multiple_of(m) {
            return Err(PyValueError::new_err(
                "need dim > 0, m > 0, and m to divide dim",
            ));
        }
        if nbits == 0 || nbits > 8 {
            return Err(PyValueError::new_err("nbits must be in 1..=8"));
        }
        if nlist == 0 {
            return Err(PyValueError::new_err("nlist must be >= 1"));
        }
        let mut params = IvfpqParams::new(dim, nlist, m);
        params.nbits = nbits;
        params.metric = parse_metric(metric)?;
        params.kmeans_iters = kmeans_iters;
        if let Some(s) = seed {
            params.seed = s;
        }
        Ok(VectorBuilder {
            params,
            vectors: Vec::new(),
        })
    }

    /// Stages one vector under `doc_id`. `vector` must have length `dim` (any
    /// sequence of floats — a Python list, tuple, or numpy row via `.tolist()`).
    fn add(&mut self, doc_id: u32, vector: Vec<f32>) -> PyResult<()> {
        if vector.len() != self.params.dim {
            return Err(PyValueError::new_err(format!(
                "vector has length {}, expected dim {}",
                vector.len(),
                self.params.dim
            )));
        }
        self.vectors.push((doc_id, vector));
        Ok(())
    }

    /// Stages many `(doc_id, vector)` pairs at once (one call instead of a Python
    /// loop). Every vector must have length `dim`.
    fn add_many(&mut self, items: Vec<(u32, Vec<f32>)>) -> PyResult<()> {
        for (id, v) in items {
            self.add(id, v)?;
        }
        Ok(())
    }

    /// Number of staged vectors.
    fn __len__(&self) -> usize {
        self.vectors.len()
    }

    /// Trains the IVFPQ index over the staged vectors and writes it to
    /// `out_path` (an `.rrvi` file; parent directories are created). Returns a
    /// [`VectorBuildStats`]. Raises `ValueError` for empty input or invalid params.
    fn build(&self, out_path: &str) -> PyResult<VectorBuildStats> {
        let idx = build_ivfpq(&self.vectors, &self.params).map_err(build_err)?;
        let path = Path::new(out_path);
        create_parent(path)?;
        idx.write(File::create(path).map_err(io_err)?)
            .map_err(io_err)?;
        Ok(VectorBuildStats {
            vectors: idx.len() as usize,
            dim: idx.dim(),
            nlist: idx.nlist(),
            m: idx.subquantizers(),
            nbits: idx.nbits(),
        })
    }
}

/// Maps a metric name to the core's metric tag.
fn parse_metric(s: &str) -> PyResult<u8> {
    match s.to_ascii_lowercase().as_str() {
        "ip" | "cosine" | "inner_product" | "dot" => Ok(METRIC_IP),
        "l2" | "euclidean" => Ok(METRIC_L2),
        other => Err(PyValueError::new_err(format!(
            "unknown metric {other:?}; use 'ip' (cosine) or 'l2'"
        ))),
    }
}

fn build_err(e: VectorBuildError) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// Decodes `count` little-endian `f32`s from `b`, erroring if the byte length is
/// not exactly `count * 4`.
fn bytes_to_f32(b: &[u8], count: usize, what: &str) -> PyResult<Vec<f32>> {
    if b.len() != count * 4 {
        return Err(PyValueError::new_err(format!(
            "{what}: expected {} bytes ({count} f32), got {}",
            count * 4,
            b.len()
        )));
    }
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Decodes little-endian `u32`s from `b`, erroring if the byte length is not a
/// multiple of 4.
fn bytes_to_u32(b: &[u8], what: &str) -> PyResult<Vec<u32>> {
    if !b.len().is_multiple_of(4) {
        return Err(PyValueError::new_err(format!(
            "{what}: byte length {} is not a multiple of 4",
            b.len()
        )));
    }
    Ok(b.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Creates the parent directory of `path` if it has one.
fn create_parent(path: &Path) -> PyResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
    }
    Ok(())
}

/// Writes an `RRVI` index from already-trained IVFPQ parts (e.g. a FAISS
/// `OPQ,IVF,PQ` export) to `out_path`, without re-training.
///
/// Arrays are passed as little-endian byte buffers (numpy `array.astype('<f4'or
/// '<u4').tobytes()` / `codes.astype('uint8').tobytes()`) so the extension needs
/// no numpy dependency:
/// - `centroids`: `nlist*dim` f32 (coarse centroids, in the OPQ-rotated space)
/// - `codebooks`: `m*(2^nbits)*(dim/m)` f32 (PQ codebooks)
/// - `ids`: `n` u32 (doc IDs), `assignments`: `n` u32 (coarse cluster per vector)
/// - `codes`: `n*m` u8 (PQ codes), `opq` (optional): `dim*dim` f32 (row-major `R`)
///
/// `metric` is `"ip"`/`"cosine"` or `"l2"`. Returns [`VectorBuildStats`]; raises
/// `ValueError` if any array length is inconsistent or an assignment/code is out
/// of range.
#[pyfunction]
#[pyo3(signature = (
    out_path, dim, nlist, m, centroids, codebooks, ids, assignments, codes,
    nbits = 8, metric = "ip", opq = None
))]
#[allow(clippy::too_many_arguments)]
fn write_rrvi_from_faiss(
    out_path: &str,
    dim: usize,
    nlist: usize,
    m: usize,
    centroids: Vec<u8>,
    codebooks: Vec<u8>,
    ids: Vec<u8>,
    assignments: Vec<u8>,
    codes: Vec<u8>,
    nbits: u8,
    metric: &str,
    opq: Option<Vec<u8>>,
) -> PyResult<VectorBuildStats> {
    if dim == 0 || m == 0 || !dim.is_multiple_of(m) {
        return Err(PyValueError::new_err(
            "need dim > 0, m > 0, and m to divide dim",
        ));
    }
    if nbits == 0 || nbits > 8 {
        return Err(PyValueError::new_err("nbits must be in 1..=8"));
    }
    if nlist == 0 {
        return Err(PyValueError::new_err("nlist must be >= 1"));
    }
    let ksub = 1usize << nbits;
    let dsub = dim / m;
    let parts = IvfpqParts {
        dim,
        nlist,
        m,
        nbits,
        metric: parse_metric(metric)?,
        centroids: bytes_to_f32(&centroids, nlist * dim, "centroids")?,
        codebooks: bytes_to_f32(&codebooks, m * ksub * dsub, "codebooks")?,
        opq: match opq {
            Some(b) => Some(bytes_to_f32(&b, dim * dim, "opq")?),
            None => None,
        },
        ids: bytes_to_u32(&ids, "ids")?,
        assignments: bytes_to_u32(&assignments, "assignments")?,
        codes,
    };
    let idx = build_ivfpq_from_parts(parts).map_err(build_err)?;
    let path = Path::new(out_path);
    create_parent(path)?;
    idx.write(File::create(path).map_err(io_err)?)
        .map_err(io_err)?;
    Ok(VectorBuildStats {
        vectors: idx.len() as usize,
        dim: idx.dim(),
        nlist: idx.nlist(),
        m: idx.subquantizers(),
        nbits: idx.nbits(),
    })
}

/// Builds an `RRTI` term-level inverted index over `(doc_id, text)` documents and
/// writes it to `path` (parent directories are created). Unlike the trigram `RRS`
/// index, this keys postings by whole terms (an FST dictionary); doc IDs should be
/// the shared rank-order IDs so facets/records/vector compose. `head_boundary` is
/// the doc-ID head/tail split (a multiple of 65536); it defaults to the core
/// default (65536). Raises `IOError` on write failure or a posting that overflows
/// the format's size/offset limits.
#[pyfunction]
#[pyo3(signature = (path, docs, head_boundary = None))]
fn write_term_index(
    path: &str,
    docs: Vec<(u32, String)>,
    head_boundary: Option<u32>,
) -> PyResult<()> {
    let borrowed: Vec<(u32, &str)> = docs.iter().map(|(id, text)| (*id, text.as_str())).collect();
    let p = Path::new(path);
    create_parent(p)?;
    core_write_term_index(
        File::create(p).map_err(io_err)?,
        &borrowed,
        head_boundary.unwrap_or(DEFAULT_HEAD_BOUNDARY),
    )
    .map_err(io_err)?;
    Ok(())
}

/// A streaming `RRTI` term-index builder for corpora too large to hold in memory.
/// Construct with a head boundary and optional Snowball stemming / stop-word removal,
/// feed `(doc_id, text)` documents in chunks with `add_batch` (each is tokenized and the
/// text discarded — only the postings grow), then `finish(path)` writes the `.rrt`. Doc
/// IDs should be the shared rank-order IDs so the index composes with the others.
#[pyclass]
struct TermBuilder {
    inner: Option<TermIndexBuilder>,
}

#[pymethods]
impl TermBuilder {
    #[new]
    #[pyo3(signature = (head_boundary = None, language = None, stopwords = false, block_cap = None))]
    fn new(
        head_boundary: Option<u32>,
        language: Option<String>,
        stopwords: bool,
        block_cap: Option<usize>,
    ) -> PyResult<Self> {
        let language = match language.as_deref() {
            None => None,
            Some(code) => Some(Language::from_code(code).ok_or_else(|| {
                PyValueError::new_err(format!(
                    "unknown stemmer language {code:?} (supported: \"english\", \"spanish\")"
                ))
            })?),
        };
        let config = TermIndexConfig {
            head_boundary: head_boundary.unwrap_or(DEFAULT_HEAD_BOUNDARY),
            language,
            stopwords,
            block_cap: block_cap.unwrap_or(0),
        };
        Ok(TermBuilder {
            inner: Some(TermIndexBuilder::new(&config)),
        })
    }

    /// Adds a batch of `(doc_id, text)` documents; the text is tokenized and discarded.
    fn add_batch(&mut self, docs: Vec<(u32, String)>) -> PyResult<()> {
        let b = self
            .inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("TermBuilder already finished"))?;
        for (doc, text) in &docs {
            b.add(*doc, text);
        }
        Ok(())
    }

    /// Distinct terms accumulated so far.
    fn term_count(&self) -> usize {
        self.inner.as_ref().map_or(0, TermIndexBuilder::len)
    }

    /// Writes the accumulated index to `path`, consuming the builder.
    fn finish(&mut self, path: &str) -> PyResult<()> {
        let b = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("TermBuilder already finished"))?;
        let p = Path::new(path);
        create_parent(p)?;
        b.finish(File::create(p).map_err(io_err)?).map_err(io_err)?;
        Ok(())
    }
}

fn io_err(e: std::io::Error) -> PyErr {
    PyIOError::new_err(e.to_string())
}

/// Maps a policy name to the core's [`CorePolicy`].
fn parse_policy(s: &str) -> PyResult<CorePolicy> {
    match s.to_ascii_lowercase().as_str() {
        "tiered" | "rank" | "rank_tiered" => Ok(CorePolicy::Tiered),
        "stable" | "stable_key" | "stablekey" | "ingest" => Ok(CorePolicy::StableKey),
        other => Err(PyValueError::new_err(format!(
            "unknown policy {other:?}; use 'tiered' or 'stable_key'"
        ))),
    }
}

/// Builds a [`CoreSortColSpec`] from a `(name, column, descending)` tuple.
fn sortcol_spec(t: Option<(String, u16, bool)>) -> Option<CoreSortColSpec> {
    t.map(|(name, column, descending)| CoreSortColSpec {
        name,
        column,
        descending,
    })
}

/// Summary of a completed split-set build, returned by [`SplitSetBuilder::build`].
#[pyclass]
struct SplitSetBuildStats {
    /// Number of splits written.
    #[pyo3(get)]
    splits: usize,
    /// Number of documents indexed.
    #[pyo3(get)]
    docs: usize,
    /// Total bytes across every split file.
    #[pyo3(get)]
    total_bytes: u64,
}

#[pymethods]
impl SplitSetBuildStats {
    fn __repr__(&self) -> String {
        format!(
            "SplitSetBuildStats(splits={}, docs={}, total_bytes={})",
            self.splits, self.docs, self.total_bytes
        )
    }
}

/// A byte-capped `RRSS` split-set builder — the batch greedy seal. Feed documents with
/// [`add`](Self::add) (rank order for the tiered policy, ingest order for stable-key); each is
/// tokenized into n-gram keys and accumulated into the open split, which is sealed into an
/// immutable `RRS` when an upper-bound size estimate nears `byte_cap`. [`build`](Self::build)
/// writes the manifest and every split file to a directory.
///
/// ```python
/// import roaringrange as rr
/// b = rr.SplitSetBuilder(policy="tiered", byte_cap=32 << 20, gram_size=3, name_prefix="corpus")
/// for text in texts_in_rank_order:
///     b.add(text)
/// stats = b.build("out/")   # writes out/index.rrss + out/corpus-s00000.rrs, ...
/// ```
#[pyclass]
struct SplitSetBuilder {
    inner: Option<CoreSplitSetBuilder>,
}

#[pymethods]
impl SplitSetBuilder {
    /// Creates a builder. `policy` is `"tiered"` (rank order) or `"stable_key"` (ingest order
    /// + a query-time rank `RRSC`); `byte_cap` is the per-split seal target; `name_prefix`
    /// names the split files (`‹prefix›-s00000.rrs`, …). `head_boundary`/`stride` of `0` take
    /// the `RRS` defaults. `sortcol` is an optional `(rrsc_name, column, descending)` recorded
    /// for the stable-key rank. `bloom_bits_per_key` sizes the per-split term Bloom filters
    /// (`0` disables; `~10` ≈ 1% false positives) — the biggest fan-out reducer for term queries.
    #[new]
    #[pyo3(signature = (policy="tiered", byte_cap=33_554_432, gram_size=3, head_boundary=0, stride=0, name_prefix="split", sortcol=None, bloom_bits_per_key=10, byte_cap_max=0))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        policy: &str,
        byte_cap: u64,
        gram_size: u16,
        head_boundary: u32,
        stride: u32,
        name_prefix: &str,
        sortcol: Option<(String, u16, bool)>,
        bloom_bits_per_key: u32,
        byte_cap_max: u64,
    ) -> PyResult<Self> {
        let config = SplitBuildConfig {
            policy: parse_policy(policy)?,
            byte_cap,
            byte_cap_max,
            gram_size: gram_size.max(1),
            head_boundary,
            stride,
            name_prefix: name_prefix.to_string(),
            sortcol: sortcol_spec(sortcol),
            bloom_bits_per_key,
        };
        Ok(SplitSetBuilder {
            inner: Some(CoreSplitSetBuilder::new(config)),
        })
    }

    /// Stages one document (tokenized into n-gram keys) and returns its global doc id.
    fn add(&mut self, text: &str) -> PyResult<u32> {
        self.inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("SplitSetBuilder already built"))?
            .add_text(text)
            .map_err(io_err)
    }

    /// Stages one faceted document and returns its global doc id. `facets` maps each field to the
    /// categories this document belongs to (e.g. `{"year": ["2020"], "type": ["article"]}`), the
    /// same shape as `Builder.add`. Each split then gets its own `RRSF` facet sidecar (written by
    /// `build`) plus a facet-presence summary, so a facet-filtered query can skip splits lacking a
    /// selected category.
    fn add_faceted(&mut self, text: &str, facets: HashMap<String, Vec<String>>) -> PyResult<u32> {
        let pairs: Vec<(String, String)> = facets
            .into_iter()
            .flat_map(|(field, cats)| cats.into_iter().map(move |cat| (field.clone(), cat)))
            .collect();
        self.inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("SplitSetBuilder already built"))?
            .add_faceted(text, &pairs)
            .map_err(io_err)
    }

    /// Total documents added so far.
    fn doc_count(&self) -> u32 {
        self.inner
            .as_ref()
            .map_or(0, CoreSplitSetBuilder::doc_count)
    }

    fn __len__(&self) -> usize {
        self.doc_count() as usize
    }

    /// Seals the final split and writes the dataset into `out_dir`: the manifest
    /// `‹manifest_name›.rrss` plus every split `RRS` file (named by the builder's prefix).
    /// Parent directories are created. Consumes the builder. Raises `IOError` on a degenerate
    /// corpus (a single document whose postings exceed the byte cap) or a write failure.
    #[pyo3(signature = (out_dir, manifest_name="index"))]
    fn build(&mut self, out_dir: &str, manifest_name: &str) -> PyResult<SplitSetBuildStats> {
        let b = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("SplitSetBuilder already built"))?;
        let docs = b.doc_count() as usize;
        let built = b.finish().map_err(io_err)?;
        let dir = Path::new(out_dir);
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let mut total_bytes = 0u64;
        for (name, bytes) in &built.splits {
            std::fs::write(dir.join(name), bytes).map_err(io_err)?;
            total_bytes += bytes.len() as u64;
        }
        // Per-split `RRSF` facet sidecars (one per split that carried any faceted document).
        for (name, bytes) in &built.facets {
            std::fs::write(dir.join(name), bytes).map_err(io_err)?;
            total_bytes += bytes.len() as u64;
        }
        std::fs::write(dir.join(format!("{manifest_name}.rrss")), &built.manifest)
            .map_err(io_err)?;
        Ok(SplitSetBuildStats {
            splits: built.splits.len(),
            docs,
            total_bytes,
        })
    }
}

/// The term/FST (`RRTI`) analogue of [`SplitSetBuilder`]: each sealed split is a term index
/// rather than a trigram `RRS`, for a head-to-head comparison over the same corpus. `language`
/// enables Snowball stemming (`"english"`); `stopwords` drops stop words. There is no
/// `gram_size`/`bloom_bits_per_key` — the body tokenizes whole terms and term-Bloom pruning is
/// deferred. Splits are named `‹prefix›-s00000.rrt`.
#[pyclass]
struct TermSplitSetBuilder {
    inner: Option<CoreTermSplitSetBuilder>,
}

#[pymethods]
impl TermSplitSetBuilder {
    #[new]
    #[pyo3(signature = (policy="tiered", byte_cap=33_554_432, head_boundary=0, name_prefix="split", sortcol=None, language=None, stopwords=false, byte_cap_max=0))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        policy: &str,
        byte_cap: u64,
        head_boundary: u32,
        name_prefix: &str,
        sortcol: Option<(String, u16, bool)>,
        language: Option<String>,
        stopwords: bool,
        byte_cap_max: u64,
    ) -> PyResult<Self> {
        let language = match language.as_deref() {
            None => None,
            Some(code) => Some(Language::from_code(code).ok_or_else(|| {
                PyValueError::new_err(format!(
                    "unknown stemmer language {code:?} (supported: \"english\", \"spanish\")"
                ))
            })?),
        };
        let config = TermSplitBuildConfig {
            policy: parse_policy(policy)?,
            byte_cap,
            byte_cap_max,
            head_boundary,
            name_prefix: name_prefix.to_string(),
            sortcol: sortcol_spec(sortcol),
            language,
            stopwords,
        };
        Ok(TermSplitSetBuilder {
            inner: Some(CoreTermSplitSetBuilder::new(config)),
        })
    }

    /// Stages one document (tokenized into whole terms) and returns its global doc id.
    fn add(&mut self, text: &str) -> PyResult<u32> {
        self.inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("TermSplitSetBuilder already built"))?
            .add_text(text)
            .map_err(io_err)
    }

    /// Stages one faceted document — `facets` maps each field to its categories, as in
    /// [`SplitSetBuilder.add_faceted`]. Returns the global id.
    fn add_faceted(&mut self, text: &str, facets: HashMap<String, Vec<String>>) -> PyResult<u32> {
        let pairs: Vec<(String, String)> = facets
            .into_iter()
            .flat_map(|(field, cats)| cats.into_iter().map(move |cat| (field.clone(), cat)))
            .collect();
        self.inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("TermSplitSetBuilder already built"))?
            .add_faceted(text, &pairs)
            .map_err(io_err)
    }

    /// Total documents added so far.
    fn doc_count(&self) -> u32 {
        self.inner
            .as_ref()
            .map_or(0, CoreTermSplitSetBuilder::doc_count)
    }

    fn __len__(&self) -> usize {
        self.doc_count() as usize
    }

    /// Seals and writes the term split set into `out_dir`: `‹manifest_name›.rrss` plus every split
    /// `‹prefix›-s*.rrt` and any per-split `.rrf` facet sidecar. Consumes the builder.
    #[pyo3(signature = (out_dir, manifest_name="index"))]
    fn build(&mut self, out_dir: &str, manifest_name: &str) -> PyResult<SplitSetBuildStats> {
        let b = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("TermSplitSetBuilder already built"))?;
        let docs = b.doc_count() as usize;
        let built = b.finish().map_err(io_err)?;
        let dir = Path::new(out_dir);
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let mut total_bytes = 0u64;
        for (name, bytes) in built.splits.iter().chain(built.facets.iter()) {
            std::fs::write(dir.join(name), bytes).map_err(io_err)?;
            total_bytes += bytes.len() as u64;
        }
        std::fs::write(dir.join(format!("{manifest_name}.rrss")), &built.manifest)
            .map_err(io_err)?;
        Ok(SplitSetBuildStats {
            splits: built.splits.len(),
            docs,
            total_bytes,
        })
    }
}

/// A pure `RRSS` ingestion writer — the streaming base+delta lifecycle. `add`/`delete` into an
/// in-RAM memtable; [`flush`](Self::flush) seals it into one immutable delta split plus a new
/// manifest, **returned as bytes** so the caller owns transport and durability (PUT the split,
/// then the manifest = atomic cutover). [`compact`](Self::compact) merges delta splits into one.
///
/// ```python
/// w = rr.SplitSetWriter.resume(prev_manifest_bytes, gram_size=3, name_prefix="corpus")
/// for text in new_docs:
///     w.add(text)
/// if w.memtable_bytes() > CAP:
///     name, split, manifest = w.flush()         # bytes in, bytes out
///     put(name, split); put("index.rrss", manifest)   # the client does the I/O
/// ```
#[pyclass]
struct SplitSetWriter {
    inner: CoreSplitSetWriter,
}

#[pymethods]
impl SplitSetWriter {
    /// Creates a fresh writer (no prior splits — the first flush writes the first delta).
    /// `policy`/`tier_count`/`sortcol` are recorded in the manifest header for when a base is
    /// later compacted in; `head_boundary`/`stride` of `0` take the `RRS` defaults.
    #[new]
    #[pyo3(signature = (gram_size=3, byte_cap=33_554_432, name_prefix="split", policy="stable_key", head_boundary=0, stride=0, tier_count=0, sortcol=None, bloom_bits_per_key=10))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        gram_size: u16,
        byte_cap: u64,
        name_prefix: &str,
        policy: &str,
        head_boundary: u32,
        stride: u32,
        tier_count: u16,
        sortcol: Option<(String, u16, bool)>,
        bloom_bits_per_key: u32,
    ) -> PyResult<Self> {
        let config = WriterConfig {
            gram_size: gram_size.max(1),
            head_boundary,
            stride,
            byte_cap,
            name_prefix: name_prefix.to_string(),
            policy: parse_policy(policy)?,
            tier_count,
            sortcol: sortcol_spec(sortcol),
            bloom_bits_per_key,
        };
        Ok(SplitSetWriter {
            inner: CoreSplitSetWriter::new(config),
        })
    }

    /// Resumes over an existing split set given its manifest `bytes`: carries forward every
    /// split, continues the global id space, and advances the epoch. `gram_size`/
    /// `head_boundary`/`stride`/`bloom_bits_per_key` must match the base (they are not fully
    /// recorded per split in the manifest). Raises `ValueError` if the manifest is malformed.
    #[staticmethod]
    #[pyo3(signature = (manifest, gram_size=3, head_boundary=0, stride=0, name_prefix="split", bloom_bits_per_key=10))]
    fn resume(
        manifest: Vec<u8>,
        gram_size: u16,
        head_boundary: u32,
        stride: u32,
        name_prefix: &str,
        bloom_bits_per_key: u32,
    ) -> PyResult<Self> {
        let prev = CoreSplitSet::from_bytes(&manifest)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(SplitSetWriter {
            inner: CoreSplitSetWriter::resume(
                &prev,
                gram_size.max(1),
                head_boundary,
                stride,
                name_prefix.to_string(),
                bloom_bits_per_key,
            ),
        })
    }

    /// Appends one document (tokenized into n-gram keys) to the memtable; returns its global id.
    fn add(&mut self, text: &str) -> u32 {
        self.inner.add_text(text)
    }

    /// Records a tombstone for a previously-indexed global doc id; the next flush carries it.
    fn delete(&mut self, doc_id: u32) {
        self.inner.delete(doc_id);
    }

    /// An estimate of the open memtable's serialized size — the flush size trigger.
    fn memtable_bytes(&self) -> u64 {
        self.inner.memtable_bytes()
    }

    /// Documents buffered in the open memtable (not yet flushed).
    fn memtable_doc_count(&self) -> u32 {
        self.inner.memtable_doc_count()
    }

    /// Total documents ever added.
    fn doc_count(&self) -> u32 {
        self.inner.doc_count()
    }

    /// Seals the memtable (and any pending deletes) into one delta split + a new manifest.
    /// Returns `(split_name, split_bytes, manifest_bytes)`, or `None` when there is nothing to
    /// flush. The caller PUTs the split then the manifest (the atomic cutover).
    #[allow(clippy::type_complexity)]
    fn flush<'py>(
        &mut self,
        py: Python<'py>,
    ) -> PyResult<Option<(String, Bound<'py, PyBytes>, Bound<'py, PyBytes>)>> {
        match self.inner.flush().map_err(io_err)? {
            None => Ok(None),
            Some(f) => Ok(Some((
                f.split_name,
                PyBytes::new_bound(py, &f.split_bytes),
                PyBytes::new_bound(py, &f.manifest),
            ))),
        }
    }

    /// Merges the named delta `inputs` (`[(name, rrs_bytes), ...]`) into one absolute-id split,
    /// dropping tombstoned docs. Returns `(split_name, split_bytes, manifest_bytes, removed)`,
    /// where `removed` is the input names the caller may delete after the cutover. Raises
    /// `IOError` if an input is not a current delta split (re-tiering the base is a rebuild).
    #[allow(clippy::type_complexity)]
    fn compact<'py>(
        &mut self,
        py: Python<'py>,
        inputs: Vec<(String, Vec<u8>)>,
    ) -> PyResult<(
        String,
        Bound<'py, PyBytes>,
        Bound<'py, PyBytes>,
        Vec<String>,
    )> {
        let c = self.inner.compact(&inputs).map_err(io_err)?;
        Ok((
            c.split_name,
            PyBytes::new_bound(py, &c.split_bytes),
            PyBytes::new_bound(py, &c.manifest),
            c.removed,
        ))
    }
}

#[pymodule]
fn roaringrange(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Builder>()?;
    m.add_class::<BuildStats>()?;
    m.add_class::<VectorBuilder>()?;
    m.add_class::<VectorBuildStats>()?;
    m.add_function(wrap_pyfunction!(tokenize, m)?)?;
    m.add_function(wrap_pyfunction!(write_rrvi_from_faiss, m)?)?;
    m.add_function(wrap_pyfunction!(write_term_index, m)?)?;
    m.add_class::<TermBuilder>()?;
    m.add_class::<SplitSetBuilder>()?;
    m.add_class::<TermSplitSetBuilder>()?;
    m.add_class::<SplitSetBuildStats>()?;
    m.add_class::<SplitSetWriter>()?;
    Ok(())
}
