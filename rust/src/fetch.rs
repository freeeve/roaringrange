//! The byte-access abstraction.
//!
//! All index byte access goes through [`RangeFetch`] so the core reader is
//! transport-agnostic: native tests use [`MemoryFetch`] (a `Vec<u8>` slice),
//! while the WASM build supplies a `fetch()`-backed HTTP Range implementation
//! (behind the `wasm` feature). The core never changes.
//!
//! [`RangeFetch::read`] is asynchronous so a query can issue its independent
//! ranged reads concurrently (see [`crate::index::Index::search`]). Each read
//! borrows `&self` immutably, so a batch of reads can be awaited together via
//! `futures::future::join_all` without conflicting borrows.

use std::error::Error;
use std::fmt;

/// An error raised when a ranged read cannot be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The requested `[offset, offset+len)` lies outside the backing bytes.
    OutOfRange {
        /// Requested start offset.
        offset: u64,
        /// Requested length.
        len: usize,
        /// Total available bytes.
        available: u64,
    },
    /// A transport-specific failure, carrying a human-readable message.
    Transport(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::OutOfRange {
                offset,
                len,
                available,
            } => write!(
                f,
                "ranged read [{offset}, {}) exceeds {available} available bytes",
                offset.saturating_add(*len as u64)
            ),
            FetchError::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl Error for FetchError {}

/// Random byte-range access over an opaque source (file, HTTP Range, ...).
///
/// Implementations return exactly `len` bytes starting at `offset`, or an error
/// if the range cannot be satisfied. The method is `async` so callers can drive
/// many independent reads concurrently; the returned future must only borrow
/// `&self` so a batch of reads can be joined together.
pub trait RangeFetch {
    /// Reads `len` bytes starting at `offset`.
    fn read(
        &self,
        offset: u64,
        len: usize,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, FetchError>>;
}

/// An in-memory [`RangeFetch`] backed by a byte vector, used by tests and any
/// caller that already holds the full index in memory.
#[derive(Debug, Clone)]
pub struct MemoryFetch {
    bytes: Vec<u8>,
}

impl MemoryFetch {
    /// Wraps the given bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Returns the total number of backing bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Reports whether the backing buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl RangeFetch for MemoryFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        // The offset converts before any arithmetic: an `as usize` cast would
        // truncate an offset >= 2^32 on wasm32 and serve the wrong bytes as Ok.
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| start.checked_add(len).map(|end| (start, end)));
        match range {
            Some((start, end)) if end <= self.bytes.len() => Ok(self.bytes[start..end].to_vec()),
            _ => Err(FetchError::OutOfRange {
                offset,
                len,
                available: self.bytes.len() as u64,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn reads_exact_range() {
        let f = MemoryFetch::new(vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(block_on(f.read(2, 3)).unwrap(), vec![2, 3, 4]);
        assert_eq!(block_on(f.read(0, 0)).unwrap(), Vec::<u8>::new());
        assert_eq!(f.len(), 6);
        assert!(!f.is_empty());
    }

    #[test]
    fn out_of_range_errors() {
        let f = MemoryFetch::new(vec![0, 1, 2]);
        assert!(matches!(
            block_on(f.read(2, 2)),
            Err(FetchError::OutOfRange { .. })
        ));
        assert!(matches!(
            block_on(f.read(10, 1)),
            Err(FetchError::OutOfRange { .. })
        ));
    }
}
