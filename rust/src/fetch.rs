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
    /// The backing object does not exist (e.g. HTTP 404, `io::ErrorKind::NotFound`).
    /// Distinct from [`Transport`](Self::Transport) so a caller can tell "this object
    /// is legitimately absent" from "the fetch failed transiently" — an absent optional
    /// sidecar is skipped, a transient failure is propagated.
    NotFound,
    /// A transport-specific failure, carrying a human-readable message.
    Transport(String),
}

impl FetchError {
    /// Whether this error means the object does not exist (vs a transient/transport
    /// failure or an out-of-range read against an object that does exist).
    pub fn is_not_found(&self) -> bool {
        matches!(self, FetchError::NotFound)
    }
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
            FetchError::NotFound => write!(f, "object not found"),
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
    /// When set, every read returns [`FetchError::NotFound`] — a stand-in for an
    /// absent object, so a resolver can represent "this sidecar does not exist"
    /// distinctly from an empty (present-but-truncated) one.
    missing: bool,
}

impl MemoryFetch {
    /// Wraps the given bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            missing: false,
        }
    }

    /// A fetcher standing in for an object that does not exist: every read returns
    /// [`FetchError::NotFound`]. Lets a resolver signal a legitimately-absent optional
    /// sidecar (vs a present-but-corrupt one) so a caller can skip it rather than error.
    pub fn missing() -> Self {
        Self {
            bytes: Vec::new(),
            missing: true,
        }
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
        if self.missing {
            return Err(FetchError::NotFound);
        }
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

/// Gap (bytes) up to which [`read_coalesced`] bridges two nearby ranges into one
/// read: over-reading at most this much beats paying another round trip. Matches
/// the posting reader's container-span gap.
pub(crate) const COALESCE_GAP: u64 = 16 * 1024;

/// Fetches a batch of byte ranges in one concurrent wave, **coalescing**
/// near-adjacent ranges (gap ≤ `gap` bytes) into single reads and slicing the
/// results back out, aligned with `ranges`. Object stores have no multi-range
/// GET, so a page of clustered small reads (record offset pairs, rank-adjacent
/// record slices, one field's facet postings) otherwise costs one round trip
/// each. The bridged gap bytes land in the shared range cache, so they are not
/// wasted on a warm client. A zero-length range yields an empty vec, no fetch.
pub(crate) async fn read_coalesced<F: RangeFetch>(
    fetch: &F,
    ranges: &[(u64, usize)],
    gap: u64,
) -> Result<Vec<Vec<u8>>, FetchError> {
    use futures::future::join_all;
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&i| ranges[i].0);

    let mut spans: Vec<(u64, u64)> = Vec::new(); // [start, end)
    let mut span_of: Vec<usize> = vec![usize::MAX; ranges.len()];
    for &i in &order {
        let (off, len) = ranges[i];
        if len == 0 {
            continue;
        }
        // `off`/`len` may be offsets parsed from an untrusted index block (e.g. a
        // facet posting's container directory). A wrapping `off + len` would panic
        // in debug and silently corrupt the span math in release, so reject the
        // overflow as an out-of-range read rather than trusting it.
        let end = off.checked_add(len as u64).ok_or(FetchError::OutOfRange {
            offset: off,
            len,
            available: u64::MAX,
        })?;
        match spans.last_mut() {
            Some(last) if off <= last.1.saturating_add(gap) => {
                if end > last.1 {
                    last.1 = end;
                }
            }
            _ => spans.push((off, end)),
        }
        span_of[i] = spans.len() - 1;
    }

    // A span length can exceed `usize` on a 32-bit (wasm) target; `try_from`
    // rejects it rather than truncating into a short read whose slice-back panics.
    let oor = |off: u64, len: usize| FetchError::OutOfRange {
        offset: off,
        len,
        available: u64::MAX,
    };
    let mut reads = Vec::with_capacity(spans.len());
    for &(s, e) in &spans {
        let span_len = usize::try_from(e - s).map_err(|_| oor(s, 0))?;
        reads.push(fetch.read(s, span_len));
    }
    let datas = join_all(reads).await;
    let mut bytes: Vec<Vec<u8>> = Vec::with_capacity(spans.len());
    for d in datas {
        bytes.push(d?);
    }
    ranges
        .iter()
        .enumerate()
        .map(|(i, &(off, len))| {
            if len == 0 {
                return Ok(Vec::new());
            }
            let (s, _) = spans[span_of[i]];
            // `off >= s` by span construction, but slice back via checked offsets
            // and `get` so a corrupted directory degrades to an error, not a panic.
            let rel = usize::try_from(off - s).map_err(|_| oor(off, len))?;
            let end = rel.checked_add(len).ok_or_else(|| oor(off, len))?;
            bytes[span_of[i]]
                .get(rel..end)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| oor(off, len))
        })
        .collect()
}

/// A file-backed [`RangeFetch`] for native tooling (builders, benches,
/// verifiers): positioned reads against a local file, mirroring the ranged-GET
/// access pattern with no server. Cheap to clone — the open handle is shared.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct FileFetch {
    file: std::sync::Arc<std::fs::File>,
}

#[cfg(unix)]
impl FileFetch {
    /// Opens `path` read-only. Note this `open` is **synchronous** (a local file
    /// handle, like [`std::fs::File::open`]); the range-fetchable index readers'
    /// `open` is async and returns [`crate::IndexError`].
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Ok(Self {
            file: std::sync::Arc::new(std::fs::File::open(path)?),
        })
    }
}

#[cfg(unix)]
impl RangeFetch for FileFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            match self
                .file
                .read_at(&mut buf[filled..], offset + filled as u64)
            {
                Ok(0) => {
                    return Err(FetchError::Transport(format!(
                        "unexpected EOF at offset {offset} (+{filled} of {len})"
                    )))
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(FetchError::NotFound)
                }
                Err(e) => return Err(FetchError::Transport(e.to_string())),
            }
        }
        Ok(buf)
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

    /// A [`RangeFetch`] wrapper counting how many reads reach the backing store.
    struct CountingFetch {
        inner: MemoryFetch,
        reads: std::cell::Cell<usize>,
    }

    impl RangeFetch for CountingFetch {
        async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
            self.reads.set(self.reads.get() + 1);
            self.inner.read(offset, len).await
        }
    }

    #[test]
    fn coalesced_reads_match_and_merge() {
        let backing: Vec<u8> = (0..200u8).collect();
        let f = CountingFetch {
            inner: MemoryFetch::new(backing.clone()),
            reads: std::cell::Cell::new(0),
        };
        // Unsorted, overlapping, adjacent, gapped, and zero-length ranges.
        let ranges: Vec<(u64, usize)> = vec![
            (50, 10), // merges with (40,15) (overlap) and (62,8) (gap 2)
            (0, 4),   // own span (gap to 40 exceeds 16)
            (40, 15),
            (62, 8),
            (10, 0),   // zero-length: no fetch, empty result
            (150, 20), // far: own span
        ];
        let out = block_on(read_coalesced(&f, &ranges, 16)).unwrap();
        for (i, &(off, len)) in ranges.iter().enumerate() {
            assert_eq!(
                out[i],
                backing[off as usize..off as usize + len],
                "range {i} bytes"
            );
        }
        assert_eq!(
            f.reads.get(),
            3,
            "five non-empty ranges merged into 3 spans"
        );

        // gap 0 still merges true overlaps/adjacency but not the 2-byte gap.
        let f2 = CountingFetch {
            inner: MemoryFetch::new(backing),
            reads: std::cell::Cell::new(0),
        };
        let out2 = block_on(read_coalesced(&f2, &ranges, 0)).unwrap();
        assert_eq!(out2[0], out[0]);
        assert_eq!(
            f2.reads.get(),
            4,
            "(62,8) splits off without the gap bridge"
        );
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
