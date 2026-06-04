//! Browser-side reader for the RRS range-fetchable static search
//! index.
//!
//! The crate is transport-agnostic: all byte access goes through the
//! [`RangeFetch`] trait, so the same core serves native callers (via
//! [`MemoryFetch`]) and a future WASM build with a `fetch()`-backed Range
//! reader (behind the `wasm` feature). See `README.md` and `FORMAT.md`.

pub mod catalog;
pub mod facet;
pub mod fetch;
pub mod index;
pub mod lookup;
pub mod ngram;
pub mod records;
pub mod secondary;
pub mod sortcols;

/// The `RRVI` range-fetchable similarity (vector) search reader. Behind the
/// `vector` feature; adds no dependencies (pure-Rust IVFPQ ADC over `RangeFetch`).
#[cfg(feature = "vector")]
pub mod vector;

/// In-browser model2vec query embedder (mode 2): BERT tokenize → static-embedding
/// mean-pool, no backend. Behind the `vector` feature; wasm-safe.
#[cfg(feature = "vector")]
pub mod model2vec;

/// The `RRTI` range-fetchable term-level inverted index reader: an FST term
/// dictionary over term-keyed roaring postings, sharing the doc-ID space with the
/// other formats. Behind the `terms` feature; wasm-safe.
#[cfg(feature = "terms")]
pub mod terms;

/// Native build-side writer for the `RRTI` term index (excluded from wasm).
/// Behind the `terms` feature.
#[cfg(all(feature = "terms", not(target_arch = "wasm32")))]
pub mod terms_build;

/// The `RRHC` catalog-hotcache reader: a cross-format boot accelerator that front-loads
/// every member's boot region into one small artifact, booting a composition in 1–2 round
/// trips instead of N cold opens. Behind the `hotcache` feature; wasm-safe.
#[cfg(feature = "hotcache")]
pub mod hotcache;

/// Native build-side writer for the `RRHC` catalog hotcache (excluded from wasm).
/// Behind the `hotcache` feature.
#[cfg(all(feature = "hotcache", not(target_arch = "wasm32")))]
pub mod hotcache_build;

/// Container-level ranged reads into tail postings (search-fetch reduction).
mod posting;

/// Native build-side writers for the `RRS`/`RRSF` formats (excluded from wasm).
#[cfg(not(target_arch = "wasm32"))]
pub mod build;

/// Native build-side IVFPQ trainer/writer for the `RRVI` format (excluded from
/// wasm). Behind the `vector` feature.
#[cfg(all(feature = "vector", not(target_arch = "wasm32")))]
pub mod vector_build;

pub use catalog::{Catalog, SearchPage};
pub use facet::FacetIndex;
pub use fetch::{FetchError, MemoryFetch, RangeFetch};
pub use index::{Index, IndexError, ResolvedFilter};
pub use lookup::Lookup;
pub use ngram::ngram_keys;
pub use records::RecordStore;
pub use secondary::{SecondaryCursor, SecondaryIndex};
pub use sortcols::{ColInfo, SortCols, Value, ValueType};

#[cfg(feature = "vector")]
pub use model2vec::Model2vec;
#[cfg(feature = "vector")]
pub use vector::{reciprocal_rank_fusion, RerankStore, VectorHit, VectorIndex};
#[cfg(all(feature = "vector", not(target_arch = "wasm32")))]
pub use vector_build::{
    build_ivfpq, build_ivfpq_from_parts, write_rerank, Ivfpq, IvfpqParams, IvfpqParts,
    VectorBuildError,
};

#[cfg(feature = "terms")]
pub use terms::{tokenize, TermIndex};
#[cfg(all(feature = "terms", not(target_arch = "wasm32")))]
pub use terms_build::write_term_index;

#[cfg(feature = "hotcache")]
pub use hotcache::{Hotcache, Member, MemberTag};
#[cfg(all(feature = "hotcache", not(target_arch = "wasm32")))]
pub use hotcache_build::{write_hotcache, MemberSpec};

#[cfg(feature = "wasm")]
mod wasm;

// The in-crate test modules exercise the native build writers (`crate::build`),
// which are excluded from wasm32; gate them to native so `wasm-pack test` (which
// compiles the crate's tests) builds. The wasm decode path is covered by the
// dedicated integration test in tests/wasm_zstd.rs.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod build_tests;
