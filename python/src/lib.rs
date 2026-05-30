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

use pyo3::exceptions::PyIOError;
use pyo3::prelude::*;
use roaring::RoaringBitmap;
use roaringrange_core::build::{
    split_posting, write_facets, write_index, write_records, FacetCategory, FacetField,
};
use roaringrange_core::ngram_keys;
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
    docs: Vec<Doc>,
}

#[pymethods]
impl Builder {
    /// Creates a builder. `gram_size` is the trigram window (3 is the usual
    /// choice; it must match what the reader queries with).
    #[new]
    #[pyo3(signature = (gram_size = 3))]
    fn new(gram_size: u16) -> Self {
        Builder {
            gram_size: gram_size.max(1),
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
                let (head, tail) = split_posting(&bm);
                (key, head, tail)
            })
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
                        let (head, tail) = split_posting(&bm);
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
        write_facets(File::create(dir.join("index.rrf")).map_err(io_err)?, fields).map_err(io_err)?;

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

fn io_err(e: std::io::Error) -> PyErr {
    PyIOError::new_err(e.to_string())
}

#[pymodule]
fn roaringrange(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Builder>()?;
    m.add_class::<BuildStats>()?;
    m.add_function(wrap_pyfunction!(tokenize, m)?)?;
    Ok(())
}
