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
pub use vector::{reciprocal_rank_fusion, RerankStore, VectorHit, VectorIndex};
#[cfg(all(feature = "vector", not(target_arch = "wasm32")))]
pub use vector_build::{
    build_ivfpq, build_ivfpq_from_parts, write_rerank, Ivfpq, IvfpqParams, IvfpqParts,
    VectorBuildError,
};

#[cfg(feature = "wasm")]
mod wasm;

// The in-crate test modules exercise the native build writers (`crate::build`),
// which are excluded from wasm32; gate them to native so `wasm-pack test` (which
// compiles the crate's tests) builds. The wasm decode path is covered by the
// dedicated integration test in tests/wasm_zstd.rs.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod build_tests;
