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
use roaring::RoaringBitmap;
use roaringrange_core::build::{
    split_posting, write_facets, write_index, write_records, FacetCategory, FacetField,
    DEFAULT_HEAD_BOUNDARY,
};
use roaringrange_core::ngram_keys;
use roaringrange_core::vector::{METRIC_IP, METRIC_L2};
use roaringrange_core::write_term_index as core_write_term_index;
use roaringrange_core::{
    build_ivfpq, build_ivfpq_from_parts, IvfpqParams, IvfpqParts, VectorBuildError,
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

        // RRS text index.
        let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = index
            .into_iter()
            .map(|(key, bm)| {
                let (head, tail) = split_posting(&bm, self.head_boundary);
                (key, head, tail)
            })
            .collect();
        let ngrams = entries.len();
        write_index(
            File::create(dir.join("index.rrs")).map_err(io_err)?,
            self.gram_size,
            0,
            self.head_boundary,
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

fn io_err(e: std::io::Error) -> PyErr {
    PyIOError::new_err(e.to_string())
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
    Ok(())
}
