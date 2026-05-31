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
pub mod ngram;
pub mod records;

/// Container-level ranged reads into tail postings (search-fetch reduction).
mod posting;

/// Native build-side writers for the `RRS`/`RRSF` formats (excluded from wasm).
#[cfg(not(target_arch = "wasm32"))]
pub mod build;

pub use catalog::{Catalog, SearchPage};
pub use facet::FacetIndex;
pub use fetch::{FetchError, MemoryFetch, RangeFetch};
pub use index::{Index, IndexError, ResolvedFilter};
pub use ngram::ngram_keys;
pub use records::RecordStore;

#[cfg(feature = "wasm")]
mod wasm;

#[cfg(test)]
mod build_tests;
