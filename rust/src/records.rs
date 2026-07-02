//! Reader for the `RRSR` record store written by [`crate::build::write_records`].
//!
//! Completes the no-backend story: a search yields ranked doc IDs, and this maps
//! each doc ID to its raw record bytes over HTTP Range — one read of the 16-byte
//! offset pair in the index, one read of the record slice in the blob. The
//! record's *encoding* is the application's choice (JSON, msgpack, …); the store
//! only frames opaque bytes for O(1) lookup by doc ID.
//!
//! ## Optional compression (version 2)
//! A version-1 store (the original, currently-deployed format) holds each record
//! as raw bytes, untagged — it reads here byte-for-byte regardless of crate
//! features. A version-2 store, written by [`crate::build::write_records_zstd`],
//! frames every record as `[1-byte tag][payload]`: tag `0` is a raw payload (the
//! reader returns it as-is, no codec needed); tag `1` is a zstd frame compressed
//! against a shared dictionary shipped in a `*.dict` sidecar. The reader inflates
//! a tag-1 frame with the pure-Rust `ruzstd` decoder (so the same path works on
//! native and wasm) using the dictionary passed to [`RecordStore::with_dict`] /
//! [`RecordStore::open_with_dict`]. Without the `zstd` feature, or with no
//! dictionary set, reading a tag-1 frame returns a clear error rather than
//! panicking. See `RECORDS.md`.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, IndexError};

/// `RRSR` index magic.
const MAGIC: &[u8; 4] = b"RRSR";
/// Index header size in bytes.
const HEADER_SIZE: usize = 16;

/// Frame tag for a raw (uncompressed) payload in a version-2 store.
const TAG_RAW: u8 = 0;
/// Frame tag for a zstd frame compressed against the shared dictionary.
const TAG_ZSTD_DICT: u8 = 1;

/// Upper bound on a single record's decompressed size. A zstd frame from an
/// untrusted store can inflate to gigabytes from a handful of bytes (a
/// decompression bomb); records are document metadata, so 64 MiB is orders of
/// magnitude above any legitimate record while bounding the allocation. A frame
/// that decodes past this is rejected as malformed. Only the `zstd` decode paths
/// reference it, so it is gated to that feature to stay dead-code-clean without it.
#[cfg(feature = "zstd")]
const MAX_DECOMPRESSED_RECORD: u64 = 64 << 20;

/// A range-fetchable record store: an offset index (`idx`) over a record blob
/// (`bin`). Both are addressed through [`RangeFetch`], so the same reader serves
/// native callers and the browser. A version-2 store may carry a shared zstd
/// dictionary (see [`RecordStore::with_dict`]) used to inflate compressed records.
pub struct RecordStore<F: RangeFetch> {
    idx: F,
    bin: F,
    count: u32,
    /// Store format version: `1` = untagged raw records (the original format),
    /// `2` = `[tag][payload]`-framed records (optionally zstd-compressed).
    version: u16,
    /// Shared zstd dictionary for inflating tag-1 frames, when set.
    dict: Option<Vec<u8>>,
}

impl<F: RangeFetch> RecordStore<F> {
    /// Boots the store: reads the 16-byte index header and validates magic and
    /// version. `idx` addresses the offset index, `bin` the record blob. Accepts
    /// both version 1 (untagged raw records) and version 2 (framed, optionally
    /// compressed); no dictionary is attached — use [`RecordStore::open_with_dict`]
    /// or [`RecordStore::with_dict`] when the store may hold compressed records.
    pub async fn open(idx: F, bin: F) -> Result<Self, IndexError> {
        let header = idx.read(0, HEADER_SIZE).await?;
        Self::from_boot(&header, idx, bin)
    }

    /// Boots from a **resident** copy of the 16-byte index header instead of
    /// fetching it — the boot-bundle path (`RRHC`): the caller already holds the
    /// header bytes, so opening costs no read. Equivalent to [`open`](Self::open);
    /// attach a dictionary with [`with_dict`](Self::with_dict) as usual.
    pub fn from_boot(header: &[u8], idx: F, bin: F) -> Result<Self, IndexError> {
        if header.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRSR header"));
        }
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(header, 4);
        if version != 1 && version != 2 {
            return Err(IndexError::BadVersion(version));
        }
        let count = read_u32(header, 8);
        Ok(Self {
            idx,
            bin,
            count,
            version,
            dict: None,
        })
    }

    /// Boots the store and attaches the shared zstd `dict` (the `*.dict` sidecar
    /// the builder emits) in one call — a convenience over [`RecordStore::open`] +
    /// [`RecordStore::with_dict`]. The dictionary is used to inflate tag-1
    /// (compressed) records; a version-1 or all-raw store ignores it.
    pub async fn open_with_dict(idx: F, bin: F, dict: Vec<u8>) -> Result<Self, IndexError> {
        Ok(Self::open(idx, bin).await?.with_dict(dict))
    }

    /// Attaches the shared zstd dictionary used to inflate tag-1 (compressed)
    /// records. Builder style: consumes and returns `self`. Has no effect on raw
    /// records, so it is always safe to set.
    pub fn with_dict(mut self, dict: Vec<u8>) -> Self {
        self.dict = Some(dict);
        self
    }

    /// Number of records (doc IDs `0..len`).
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether the store holds no records.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Decodes one stored record's bytes into the record payload. For a version-1
    /// store the bytes are returned verbatim (untagged raw). For a version-2 store
    /// the leading tag byte selects the codec: tag 0 returns the payload as-is;
    /// tag 1 inflates the zstd frame against the shared dictionary (requires the
    /// `zstd` feature and a dictionary set via [`RecordStore::with_dict`]). A
    /// zero-length version-2 record carries no tag and decodes to empty.
    fn decode(&self, raw: Vec<u8>) -> Result<Vec<u8>, IndexError> {
        if self.version == 1 || raw.is_empty() {
            return Ok(raw);
        }
        match raw[0] {
            // Strip the tag byte in place, reusing `raw`'s buffer, rather than
            // copying the payload into a fresh Vec on every record of a page.
            TAG_RAW => {
                let mut raw = raw;
                raw.remove(0);
                Ok(raw)
            }
            TAG_ZSTD_DICT => self.inflate_zstd(&raw[1..]),
            _ => Err(IndexError::Malformed(
                "RRSR record has an unknown frame tag",
            )),
        }
    }

    /// Inflates a zstd frame compressed against the shared dictionary, on native
    /// targets, through C `libzstd` (the `zstd` crate). The pure-Rust `ruzstd`
    /// decoder has a heap-corrupting defect in its `RingBuffer` under heavy
    /// concurrent decode — observed as a `malloc` guard abort (`abort()` from
    /// `nanov2_guard_corruption_detected`) mid-build — so native callers use the
    /// reference decoder. The wasm path keeps `ruzstd` (libzstd's C/asm does not
    /// build for wasm32). Output is byte-identical for valid frames.
    #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
    fn inflate_zstd(&self, frame: &[u8]) -> Result<Vec<u8>, IndexError> {
        use std::io::Read;
        let dict = self.dict.as_deref().ok_or(IndexError::Malformed(
            "compressed record but no dictionary set",
        ))?;
        // Stream the frame through a fresh dictionary-seeded decoder, but cap the
        // output (see `MAX_DECOMPRESSED_RECORD`): an untrusted frame can be a zstd
        // "bomb" that inflates to gigabytes from a few bytes and OOMs the reader.
        let decoder = zstd::stream::read::Decoder::with_dictionary(frame, dict)
            .map_err(|_| IndexError::Malformed("zstd frame header failed to decode"))?;
        let mut out = Vec::new();
        decoder
            .take(MAX_DECOMPRESSED_RECORD + 1)
            .read_to_end(&mut out)
            .map_err(|_| IndexError::Malformed("zstd frame failed to decode"))?;
        if out.len() as u64 > MAX_DECOMPRESSED_RECORD {
            return Err(IndexError::Malformed(
                "decompressed record exceeds size cap",
            ));
        }
        Ok(out)
    }

    /// Inflates a zstd frame compressed against the shared dictionary, on wasm32,
    /// through pure-Rust `ruzstd` (libzstd's C/asm does not build for wasm32).
    #[cfg(all(feature = "zstd", target_arch = "wasm32"))]
    fn inflate_zstd(&self, frame: &[u8]) -> Result<Vec<u8>, IndexError> {
        // ruzstd 0.8 re-exports these from `decoding`; the 0.7 top-level
        // `frame_decoder`/`streaming_decoder` module paths were removed.
        use ruzstd::decoding::{Dictionary, FrameDecoder, StreamingDecoder};
        use std::io::Read;
        let dict = self.dict.as_deref().ok_or(IndexError::Malformed(
            "compressed record but no dictionary set",
        ))?;
        // Parse the shared dictionary, seed a frame decoder with it, then stream the
        // frame through it via `read_to_end`. `decode_dict`/`add_dict`/
        // `new_with_decoder` each return a `Result`. Decode-with-dictionary is
        // pure-Rust ruzstd (public since 0.8.2).
        let dictionary = Dictionary::decode_dict(dict)
            .map_err(|_| IndexError::Malformed("zstd dictionary failed to parse"))?;
        let mut fd = FrameDecoder::new();
        fd.add_dict(dictionary)
            .map_err(|_| IndexError::Malformed("zstd dictionary failed to load"))?;
        let decoder = StreamingDecoder::new_with_decoder(frame, fd)
            .map_err(|_| IndexError::Malformed("zstd frame header failed to decode"))?;
        // Cap the output against a decompression bomb (see the native variant).
        let mut out = Vec::new();
        decoder
            .take(MAX_DECOMPRESSED_RECORD + 1)
            .read_to_end(&mut out)
            .map_err(|_| IndexError::Malformed("zstd frame failed to decode"))?;
        if out.len() as u64 > MAX_DECOMPRESSED_RECORD {
            return Err(IndexError::Malformed(
                "decompressed record exceeds size cap",
            ));
        }
        Ok(out)
    }

    /// Without the `zstd` feature a tag-1 (compressed) record cannot be inflated;
    /// surface a clear error instead of pulling in a decoder.
    #[cfg(not(feature = "zstd"))]
    fn inflate_zstd(&self, _frame: &[u8]) -> Result<Vec<u8>, IndexError> {
        Err(IndexError::Malformed(
            "compressed record needs the `zstd` feature",
        ))
    }

    /// Decoded record bytes for doc `id`, or `None` if `id` is out of range. A
    /// zero-length record (a doc with no stored fields) returns `Some(empty)`. For
    /// a version-2 store the stored frame is decoded (tag stripped, zstd inflated
    /// when needed); for a version-1 store the raw bytes are returned as-is.
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
            return Err(IndexError::Malformed(
                "RRSR record offset pair has end < start",
            ));
        }
        // Checked: an `as usize` cast truncates a corrupt >4 GiB length on wasm32,
        // silently returning a wrong-length prefix of the blob as the record.
        let len = usize::try_from(end - start)
            .map_err(|_| IndexError::Malformed("RRSR record length exceeds the address space"))?;
        let bytes = self.bin.read(start, len).await?;
        Ok(Some(self.decode(bytes)?))
    }

    /// Decoded record bytes for several doc IDs, aligned with `ids`. A results page
    /// (ascending doc IDs in rank order) is the typical input, and rank-adjacent
    /// docs sit adjacently in both files — so the reads run as two **coalesced**
    /// waves (every offset pair, then every record slice), each merging
    /// near-adjacent ranges into single requests: a 25-doc page costs a handful
    /// of round trips instead of 50. An out-of-range id yields `None`.
    pub async fn get_many(&self, ids: &[u32]) -> Result<Vec<Option<Vec<u8>>>, IndexError> {
        use crate::fetch::{read_coalesced, COALESCE_GAP};
        // Wave 1: the 16-byte offset pairs (zero-length marker = skip the read;
        // the id-bounds check below decides the output, so the marker can't be
        // confused with a real empty record).
        let pair_ranges: Vec<(u64, usize)> = ids
            .iter()
            .map(|&id| {
                if id < self.count {
                    (HEADER_SIZE as u64 + id as u64 * 8, 16)
                } else {
                    (0, 0)
                }
            })
            .collect();
        let pairs = read_coalesced(&self.idx, &pair_ranges, COALESCE_GAP).await?;

        // Wave 2: the record slices.
        let mut rec_ranges: Vec<(u64, usize)> = Vec::with_capacity(ids.len());
        for (i, &id) in ids.iter().enumerate() {
            if id >= self.count {
                rec_ranges.push((0, 0));
                continue;
            }
            let start = read_u64(&pairs[i], 0);
            let end = read_u64(&pairs[i], 8);
            if end < start {
                return Err(IndexError::Malformed(
                    "RRSR record offset pair has end < start",
                ));
            }
            let len = usize::try_from(end - start).map_err(|_| {
                IndexError::Malformed("RRSR record length exceeds the address space")
            })?;
            rec_ranges.push((start, len));
        }
        let blobs = read_coalesced(&self.bin, &rec_ranges, COALESCE_GAP).await?;

        let mut out = Vec::with_capacity(ids.len());
        for (&id, blob) in ids.iter().zip(blobs) {
            if id >= self.count {
                out.push(None);
            } else {
                out.push(Some(self.decode(blob)?));
            }
        }
        Ok(out)
    }
}

// Uses the native-only build writers; gated to native so `wasm-pack test` builds.
#[cfg(all(test, not(target_arch = "wasm32")))]
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

    /// Builds a version-2 store by hand from explicit `[tag][payload]` frames, so
    /// the reader's tag handling can be exercised without the encoder. Cumulative
    /// end offsets frame each record; doc `d` is `bin[off[d]..off[d+1]]`.
    fn build_v2(frames: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        idx.extend_from_slice(MAGIC);
        idx.extend_from_slice(&2u16.to_le_bytes()); // version 2
        idx.extend_from_slice(&0u16.to_le_bytes()); // reserved
        idx.extend_from_slice(&(frames.len() as u32).to_le_bytes());
        idx.extend_from_slice(&0u32.to_le_bytes()); // reserved2
        idx.extend_from_slice(&0u64.to_le_bytes()); // off[0]
        let mut off = 0u64;
        for f in frames {
            bin.extend_from_slice(f);
            off += f.len() as u64;
            idx.extend_from_slice(&off.to_le_bytes());
        }
        (idx, bin)
    }

    #[test]
    fn version2_raw_frames_decode_with_tag_stripped() {
        // Tag-0 (raw) frames and a zero-length record (no tag) round-trip with the
        // feature on or off — no codec is involved.
        let frames = vec![
            {
                let mut v = vec![TAG_RAW];
                v.extend_from_slice(b"alpha");
                v
            },
            Vec::new(), // zero-length record stays addressable, no tag byte
            {
                let mut v = vec![TAG_RAW];
                v.extend_from_slice(b"gamma");
                v
            },
        ];
        let (idx, bin) = build_v2(&frames);
        let store = block_on(RecordStore::open(
            MemoryFetch::new(idx),
            MemoryFetch::new(bin),
        ))
        .unwrap();
        assert_eq!(block_on(store.get(0)).unwrap().unwrap(), b"alpha");
        assert_eq!(block_on(store.get(1)).unwrap().unwrap(), b"");
        assert_eq!(block_on(store.get(2)).unwrap().unwrap(), b"gamma");
    }

    #[test]
    fn version1_raw_store_reads_regardless_of_features() {
        // A version-1 (untagged) store is read byte-for-byte: the leading byte of
        // each record is *not* a tag, so it must come back intact. This is the
        // guard that a currently-deployed raw store keeps working under both
        // feature configurations.
        let recs: Vec<Vec<u8>> = vec![b"\x01leading-0x01".to_vec(), b"plain".to_vec()];
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        write_records(&mut bin, &mut idx, &recs).unwrap();
        let store = block_on(RecordStore::open(
            MemoryFetch::new(idx),
            MemoryFetch::new(bin),
        ))
        .unwrap();
        assert_eq!(
            block_on(store.get(0)).unwrap().unwrap(),
            b"\x01leading-0x01"
        );
        assert_eq!(block_on(store.get(1)).unwrap().unwrap(), b"plain");
    }

    /// Without the `zstd` feature a tag-1 frame must surface a clear error, never
    /// panic.
    #[cfg(not(feature = "zstd"))]
    #[test]
    fn tag1_frame_without_feature_errors() {
        let frames = vec![{
            let mut v = vec![TAG_ZSTD_DICT];
            v.extend_from_slice(b"would-be-a-zstd-frame");
            v
        }];
        let (idx, bin) = build_v2(&frames);
        let store = block_on(RecordStore::open(
            MemoryFetch::new(idx),
            MemoryFetch::new(bin),
        ))
        .unwrap();
        let got = block_on(store.get(0));
        assert!(
            matches!(got, Err(IndexError::Malformed(_))),
            "expected Malformed for a compressed record without the feature, got {got:?}"
        );
    }
}
