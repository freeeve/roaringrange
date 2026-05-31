//! Reader for the `RRIL` identifier exact-match index.
//!
//! Maps a normalized identifier (ISBN / ASIN / …) to the doc ID(s) of the title(s)
//! carrying it: the identifier is FNV-hashed and a sorted `[hash u64, verify u32,
//! doc u32]` table is range-binary-searched. A second (verify) hash makes the
//! effective key 96 bits, so collisions stay ~1e-15 even at 100M+ entries. Hashing
//! handles digit ISBNs and alphanumeric ASINs uniformly. The writer (the build
//! pipeline) must reproduce [`normalize_id`] + [`fnv64a_basis`] byte-for-byte.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, IndexError};

/// `RRIL` index magic.
const MAGIC: &[u8; 4] = b"RRIL";
/// Header size in bytes.
const HEADER_SIZE: usize = 16;
/// Record size in bytes: hash(8) + verify(4) + doc(4).
const REC_SIZE: usize = 16;

/// FNV-1a offset basis for the primary identifier hash. Crate-visible so the
/// build-side writer ([`crate::build::write_lookup`]) hashes identically.
pub(crate) const FNV_OFFSET: u64 = 14695981039346656037;
const FNV_PRIME: u64 = 1099511628211;
/// Distinct nonzero basis for the independent verify hash (golden-ratio
/// constant). Crate-visible so the build-side writer hashes identically.
pub(crate) const FNV_VERIFY_BASIS: u64 = 0x9E37_79B9_7F4A_7C15;

/// Keeps only ASCII letters/digits, uppercasing letters. Must match the Go build.
pub fn normalize_id(s: &str) -> String {
    s.bytes()
        .filter_map(|c| match c {
            b'0'..=b'9' | b'A'..=b'Z' => Some(c as char),
            b'a'..=b'z' => Some((c - 32) as char),
            _ => None,
        })
        .collect()
}

/// FNV-1a over the bytes of `s` with the given offset basis (wrapping multiply).
/// Must match the Go build.
pub fn fnv64a_basis(s: &str, basis: u64) -> u64 {
    let mut h = basis;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// A range-fetchable identifier exact-match index addressed through [`RangeFetch`].
pub struct Lookup<F: RangeFetch> {
    f: F,
    count: u32,
}

impl<F: RangeFetch> Lookup<F> {
    /// Boots the index: reads and validates the 16-byte `RRIL` header.
    pub async fn open(f: F) -> Result<Self, IndexError> {
        let header = f.read(0, HEADER_SIZE).await?;
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let count = read_u32(&header, 8);
        Ok(Self { f, count })
    }

    /// Number of index entries.
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether the index holds no entries.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Reads entry `i` as (hash, verify, doc).
    async fn record(&self, i: u32) -> Result<(u64, u32, u32), IndexError> {
        let off = HEADER_SIZE as u64 + i as u64 * REC_SIZE as u64;
        let b = self.f.read(off, REC_SIZE).await?;
        Ok((read_u64(&b, 0), read_u32(&b, 8), read_u32(&b, 12)))
    }

    /// Resolves `identifier` to the doc ID(s) of the title(s) carrying it, in
    /// ascending (rank) order; empty if none. The binary search issues ~log2(n)
    /// dependent ranged reads, then scans the (usually length-1) matching run,
    /// keeping only entries whose verify hash also matches.
    pub async fn lookup(&self, identifier: &str) -> Result<Vec<u32>, IndexError> {
        let n = normalize_id(identifier);
        if n.is_empty() || self.count == 0 {
            return Ok(Vec::new());
        }
        let hash = fnv64a_basis(&n, FNV_OFFSET);
        let verify = fnv64a_basis(&n, FNV_VERIFY_BASIS) as u32;

        // Lower bound on hash.
        let (mut lo, mut hi) = (0u32, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (rhash, _, _) = self.record(mid).await?;
            if rhash < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // Scan the matching hash run, keeping verify matches.
        let mut out = Vec::new();
        let mut i = lo;
        while i < self.count {
            let (rhash, rverify, rdoc) = self.record(i).await?;
            if rhash != hash {
                break;
            }
            if rverify == verify {
                out.push(rdoc);
            }
            i += 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryFetch;
    use futures::executor::block_on;

    fn build(entries: &[(&str, u32)]) -> Vec<u8> {
        let mut recs: Vec<(u64, u32, u32)> = entries
            .iter()
            .map(|(id, doc)| {
                let n = normalize_id(id);
                (
                    fnv64a_basis(&n, FNV_OFFSET),
                    fnv64a_basis(&n, FNV_VERIFY_BASIS) as u32,
                    *doc,
                )
            })
            .collect();
        recs.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(recs.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        for (h, v, d) in recs {
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
        }
        out
    }

    #[test]
    fn resolves_identifiers() {
        // Same ISBN on two editions (docs 5, 7); an ASIN on doc 10.
        let bytes = build(&[
            ("978-1-234567-89-0", 5),
            ("B00ABC123X", 10),
            ("978-1-234567-89-0", 7),
        ]);
        let lk = block_on(Lookup::open(MemoryFetch::new(bytes))).unwrap();
        assert_eq!(lk.len(), 3);
        // Hyphen/case-insensitive ISBN -> both editions, ascending.
        assert_eq!(block_on(lk.lookup("9781234567890")).unwrap(), vec![5, 7]);
        // ASIN, case-insensitive.
        assert_eq!(block_on(lk.lookup("b00abc123x")).unwrap(), vec![10]);
        // Misses.
        assert!(block_on(lk.lookup("0000000000000")).unwrap().is_empty());
        assert!(block_on(lk.lookup("")).unwrap().is_empty());
    }
}
