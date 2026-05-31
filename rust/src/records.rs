//! Reader for the `RRSR` record store written by [`crate::build::write_records`].
//!
//! Completes the no-backend story: a search yields ranked doc IDs, and this maps
//! each doc ID to its raw record bytes over HTTP Range — one read of the 16-byte
//! offset pair in the index, one read of the record slice in the blob. The
//! record's *encoding* is the application's choice (JSON, msgpack, …); the store
//! only frames opaque bytes for O(1) lookup by doc ID.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, IndexError};
use futures::future::join_all;

/// `RRSR` index magic.
const MAGIC: &[u8; 4] = b"RRSR";
/// Index header size in bytes.
const HEADER_SIZE: usize = 16;

/// A range-fetchable record store: an offset index (`idx`) over a record blob
/// (`bin`). Both are addressed through [`RangeFetch`], so the same reader serves
/// native callers and the browser.
pub struct RecordStore<F: RangeFetch> {
    idx: F,
    bin: F,
    count: u32,
}

impl<F: RangeFetch> RecordStore<F> {
    /// Boots the store: reads the 16-byte index header and validates magic and
    /// version. `idx` addresses the offset index, `bin` the record blob.
    pub async fn open(idx: F, bin: F) -> Result<Self, IndexError> {
        let header = idx.read(0, HEADER_SIZE).await?;
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
        Ok(Self { idx, bin, count })
    }

    /// Number of records (doc IDs `0..len`).
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether the store holds no records.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Raw record bytes for doc `id`, or `None` if `id` is out of range. A
    /// zero-length record (a doc with no stored fields) returns `Some(empty)`.
    pub async fn get(&self, id: u32) -> Result<Option<Vec<u8>>, IndexError> {
        if id >= self.count {
            return Ok(None);
        }
        let pair = self
            .idx
            .read(HEADER_SIZE as u64 + id as u64 * 8, 16)
            .await?;
        let start = read_u64(&pair, 0);
        let end = read_u64(&pair, 8);
        if end < start {
            return Err(IndexError::Malformed("record offset pair has end < start"));
        }
        let bytes = self.bin.read(start, (end - start) as usize).await?;
        Ok(Some(bytes))
    }

    /// Raw record bytes for several doc IDs, aligned with `ids`. A results page
    /// (ascending doc IDs in rank order) is the typical input. Every doc's `get`
    /// is issued before any is awaited, so a page's reads proceed as a few
    /// concurrent waves rather than one serial round-trip per doc; `join_all`
    /// preserves order, keeping the output aligned with `ids`.
    pub async fn get_many(&self, ids: &[u32]) -> Result<Vec<Option<Vec<u8>>>, IndexError> {
        let results = join_all(ids.iter().map(|&id| self.get(id))).await;
        let mut out = Vec::with_capacity(results.len());
        for rec in results {
            out.push(rec?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::write_records;
    use crate::MemoryFetch;
    use futures::executor::block_on;

    #[test]
    fn get_returns_record_bytes_by_doc_id() {
        let recs: Vec<Vec<u8>> = vec![
            br#"{"id":"A","c":9}"#.to_vec(),
            Vec::new(),
            b"third".to_vec(),
        ];
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        write_records(&mut bin, &mut idx, &recs).unwrap();

        let store = block_on(RecordStore::open(
            MemoryFetch::new(idx),
            MemoryFetch::new(bin),
        ))
        .unwrap();
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());
        assert_eq!(
            block_on(store.get(0)).unwrap().unwrap(),
            br#"{"id":"A","c":9}"#
        );
        assert_eq!(block_on(store.get(1)).unwrap().unwrap(), b"");
        assert_eq!(block_on(store.get(2)).unwrap().unwrap(), b"third");
        assert!(block_on(store.get(3)).unwrap().is_none());

        let many = block_on(store.get_many(&[2, 0])).unwrap();
        assert_eq!(many[0].as_deref().unwrap(), b"third");
        assert_eq!(many[1].as_deref().unwrap(), br#"{"id":"A","c":9}"#);
    }

    #[test]
    fn get_rejects_corrupt_offset_pair() {
        let recs: Vec<Vec<u8>> = vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        write_records(&mut bin, &mut idx, &recs).unwrap();
        // Doc 1's offset pair is (off[1], off[2]) at idx[24..40]; off[2] is the
        // second u64 (idx[32..40]). Corrupt it to precede off[1] so end < start.
        idx[32..40].copy_from_slice(&0u64.to_le_bytes());
        let store = block_on(RecordStore::open(
            MemoryFetch::new(idx),
            MemoryFetch::new(bin),
        ))
        .unwrap();
        let got = block_on(store.get(1));
        assert!(
            matches!(got, Err(IndexError::Malformed(_))),
            "expected Malformed, got {got:?}"
        );
    }
}
