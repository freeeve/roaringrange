//! Browser-side reader for the RRS range-fetchable static search
//! index.
//!
//! The crate is transport-agnostic: all byte access goes through the
//! [`RangeFetch`] trait, so the same core serves native callers (via
//! [`MemoryFetch`]) and a future WASM build with a `fetch()`-backed Range
//! reader (behind the `wasm` feature). See `README.md` and `FORMAT.md`.

pub mod facet;
pub mod fetch;
pub mod index;
pub mod ngram;

pub use facet::FacetIndex;
pub use fetch::{FetchError, MemoryFetch, RangeFetch};
pub use index::{CatRange, Index, IndexError, ResolvedFilter};
pub use ngram::ngram_keys;

#[cfg(feature = "wasm")]
mod wasm;

#[cfg(test)]
mod build_tests;
