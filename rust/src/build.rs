//! Native writers for the `RRS` index and `RRSF` facet sidecar — the build-side
//! mirror of [`crate::index`]/[`crate::facet`], emitting the exact byte layout in
//! `FORMAT.md`/`FACETS.md`. Excluded from the wasm reader build.
//!
//! Postings are portable RoaringBitmaps produced with the same `roaring` crate
//! the reader deserializes with, so a build → read round-trip needs zero glue.
//! This lets a single crate both build and read an index (the OpenAlex builder
//! in `examples/openalex/builder` uses it).

use roaring::RoaringBitmap;
use std::io::{self, Write};

/// Default head/tail boundary: docs `[0, 65536)` form the head posting (the first
/// roaring container — the top-ranked docs), the rest the tail. The boundary is a
/// build parameter (see [`split_posting`]) and is recorded in the index header;
/// this is the value used when none is chosen. Should be a multiple of 65536.
pub const DEFAULT_HEAD_BOUNDARY: u32 = 65_536;

/// Internal alias for tests that assert against the default split point.
#[cfg(test)]
pub(crate) const HEAD_BOUNDARY: u32 = DEFAULT_HEAD_BOUNDARY;

/// RRS header size (v3): magic[4] + version[2] + gram[2] + ngrams[4] + stride[4]. The v2
/// `head_boundary[4]` is gone — v3 stores one posting per term (no head/tail split). Kept in
/// sync with the reader's `index::HEADER_SIZE`.
pub(crate) const HEADER_SIZE: usize = 16;
/// RRS format version written into the header. v3 collapsed the head/tail postings into one
/// bitmap per term and shrank the dict entry 24 → 20 B (see `FORMAT.md`).
pub(crate) const FORMAT_VERSION: u16 = 3;
/// RRS v4 header size: the v3 16-byte header with a 2-byte `flags` field appended at offset
/// 16 (no v3 field shifts; the sparse index then starts at 18). Kept in sync with the reader.
pub(crate) const HEADER_SIZE_V4: usize = 18;
/// RRS format version 4: byte-identical to v3 except for the trailing `flags` u16. Emitted
/// only for a **case-sensitive** index; default (case-folding) builds stay v3, so every
/// existing artifact and golden vector is unaffected.
pub(crate) const FORMAT_VERSION_V4: u16 = 4;
/// RRS v4 `flags` bit 0: the index is case-sensitive — its n-gram keys were not lowercased,
/// so a query must skip lowercasing too (mirrored by `index::RRSI_FLAG_CASE_SENSITIVE`).
pub(crate) const RRSI_FLAG_CASE_SENSITIVE: u16 = 1;

use crate::facet::facet_key;

/// Splits `bm` into the head bitmap (docs `[0, head_boundary)`) and tail bitmap
/// (docs `[head_boundary, ∞)`), each serialized as a portable RoaringBitmap.
/// `head_boundary` should be a multiple of 65536 (whole roaring containers); the
/// head holds the top-ranked docs. Mirrors the Go `splitBitmap`: intersect a
/// head-range mask for the head, clone-and-trim for the tail.
pub fn split_posting(bm: &RoaringBitmap, head_boundary: u32) -> (Vec<u8>, Vec<u8>) {
    let mut head = RoaringBitmap::new();
    head.insert_range(0..head_boundary);
    head &= bm;
    let mut tail = bm.clone();
    tail.remove_range(0..head_boundary);

    let mut hb = Vec::with_capacity(head.serialized_size());
    head.serialize_into(&mut hb).expect("serialize head bitmap");
    let mut tb = Vec::with_capacity(tail.serialized_size());
    tail.serialize_into(&mut tb).expect("serialize tail bitmap");
    (hb, tb)
}

/// Default sparse-index stride (matches Go `DefaultStride`).
pub const DEFAULT_STRIDE: u32 = 512;

/// Serializes `bm` as one v3 `RRS` posting — a single portable RoaringBitmap (no head/tail
/// split). The build-side companion of [`write_index`]; replaces [`split_posting`] for the
/// trigram index (which keeps it only for the `RRSF`/`RRTI` formats).
pub fn serialize_posting(bm: &RoaringBitmap) -> Vec<u8> {
    let mut v = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut v).expect("serialize posting bitmap");
    v
}

/// Narrows a serialized **byte** length to the `u32` an on-disk size field holds,
/// erroring rather than silently truncating past the 4 GiB format limit. Entity
/// *counts* are already bounded by the `u32` doc-ID space and need no guard;
/// serialized byte lengths (postings) are the field that isn't, so they go through
/// here. `what` names the field for the error.
fn u32_len(n: usize, what: &str) -> io::Result<u32> {
    u32::try_from(n).map_err(|_| io::Error::other(format!("{what} size exceeds the 32-bit limit")))
}

/// Writes the `RRS` sparse index: every `stride`-th entry's `u64` key, little-
/// endian. Shared by the one-shot [`write_index`] and the chunked
/// [`chunk::merge_partials_to_rrs`] so the two layouts can't drift.
fn write_sparse_index<W: Write, T>(
    w: &mut W,
    entries: &[T],
    sparse_count: usize,
    stride: usize,
    key: impl Fn(&T) -> u64,
) -> io::Result<()> {
    for i in 0..sparse_count {
        w.write_all(&key(&entries[i * stride]).to_le_bytes())?;
    }
    Ok(())
}

/// Writes the v3 `RRS` index for the given postings to `w`. Each entry is `(key, posting_bytes)`
/// — one portable RoaringBitmap per term (from [`serialize_posting`]), no head/tail split.
/// Entries are sorted by key here (the dictionary must be key-sorted). A `stride` of 0 becomes
/// [`DEFAULT_STRIDE`]. See `FORMAT.md`.
pub fn write_index<W: Write>(
    w: W,
    gram_size: u16,
    stride: u32,
    entries: Vec<(u64, Vec<u8>)>,
) -> io::Result<()> {
    write_index_with(w, gram_size, stride, entries, true)
}

/// Like [`write_index`] but with an explicit `case_normalization` flag. `true` (the
/// default) lowercases n-gram keys at build time and emits a v3 header byte-identical to
/// before; `false` builds a **case-sensitive** index, keying on the original case and
/// emitting a v4 header whose trailing `flags` field records the choice so the reader skips
/// query-side folding. The caller is responsible for keying `entries` with the matching
/// [`crate::ngram::ngram_keys_with`] case mode.
pub fn write_index_with<W: Write>(
    mut w: W,
    gram_size: u16,
    stride: u32,
    mut entries: Vec<(u64, Vec<u8>)>,
    case_normalization: bool,
) -> io::Result<()> {
    entries.sort_by_key(|e| e.0);
    // Distinct keys are required: a duplicate would make the byte order depend on the
    // sort tie-break and leave the dictionary binary search resolving to one arbitrary
    // of the two. Mirrors the Go WriteIndex guard.
    if entries.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_index requires distinct n-gram keys",
        ));
    }
    let stride = if stride == 0 { DEFAULT_STRIDE } else { stride };
    let ngrams = entries.len() as u32;
    let sparse_count = if ngrams == 0 {
        0
    } else {
        (ngrams as usize).div_ceil(stride as usize)
    };
    let header_size = if case_normalization {
        HEADER_SIZE
    } else {
        HEADER_SIZE_V4
    };
    let dict_start = header_size + sparse_count * 8;
    let postings_start = dict_start + entries.len() * 20;

    // Header: magic, version, gram, ngrams, stride. A case-folding index is v3 (byte-identical
    // to before this flag); a case-sensitive one is v4 with a trailing 2-byte `flags` field.
    w.write_all(b"RRSI")?;
    w.write_all(
        &(if case_normalization {
            FORMAT_VERSION
        } else {
            FORMAT_VERSION_V4
        })
        .to_le_bytes(),
    )?;
    w.write_all(&gram_size.to_le_bytes())?;
    w.write_all(&ngrams.to_le_bytes())?;
    w.write_all(&stride.to_le_bytes())?;
    if !case_normalization {
        w.write_all(&RRSI_FLAG_CASE_SENSITIVE.to_le_bytes())?;
    }

    // Sparse index: every stride-th key.
    write_sparse_index(&mut w, &entries, sparse_count, stride as usize, |e| e.0)?;

    // Dictionary (20 B each): key + absolute posting offset + size.
    let mut off = postings_start as u64;
    for (key, posting) in &entries {
        w.write_all(&key.to_le_bytes())?;
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&u32_len(posting.len(), "RRS posting")?.to_le_bytes())?;
        off += posting.len() as u64;
    }

    // Postings: one bitmap per entry, in dict order.
    for (_, posting) in &entries {
        w.write_all(posting)?;
    }
    Ok(())
}

/// One category prepared for the facet sidecar: display name, its split posting,
/// and full-corpus cardinality.
pub struct FacetCategory {
    /// Category display name.
    pub name: String,
    /// Full-corpus document count (the free, unfiltered facet count).
    pub card: u32,
    /// Head posting bytes (docs `[0, 65536)`).
    pub head: Vec<u8>,
    /// Tail posting bytes (docs `[65536, ∞)`).
    pub tail: Vec<u8>,
}

/// One facet field with its categories (in insertion order; sorted by key here).
pub struct FacetField {
    /// Field display name.
    pub name: String,
    /// The field's categories.
    pub cats: Vec<FacetCategory>,
}

/// Writes the `RRSF` facet sidecar for `fields` to `w`. The string blob is built
/// in field/category insertion order (matching Go `WriteFacets`); each field's
/// categories are sorted by [`facet_key`] for the category table and postings.
/// See `FACETS.md`. Equivalent to [`write_facets_with`] with case folding on (the default).
pub fn write_facets<W: Write>(w: W, fields: Vec<FacetField>) -> io::Result<()> {
    write_facets_with(w, fields, true)
}

/// Like [`write_facets`] but with an explicit `case_normalization` flag. `true` (the default)
/// lowercases field/category names for the `facet_key` hash and writes a byte-identical v1
/// sidecar; `false` keys on the raw bytes (a case-sensitive index) and records the choice in the
/// header's `reserved` field ([`crate::facet::RRSF_FLAG_CASE_SENSITIVE`]) so a split-set's facet
/// pruning recomputes keys the same way. Category display names are stored verbatim either way.
pub fn write_facets_with<W: Write>(
    mut w: W,
    fields: Vec<FacetField>,
    case_normalization: bool,
) -> io::Result<()> {
    struct COut {
        key: u64,
        card: u32,
        name_off: u32,
        name_len: u16,
        head: Vec<u8>,
        tail: Vec<u8>,
    }
    struct FOut {
        name_off: u32,
        name_len: u16,
        cat_start: u32,
        cats: Vec<COut>,
    }

    let mut blob: Vec<u8> = Vec::new();
    // Errors rather than truncating a name past the u16 length field or the u32 blob
    // offset (the reader would otherwise resolve a wrapped span). Mirrors the Go writer.
    let push = |blob: &mut Vec<u8>, s: &str| -> io::Result<(u32, u16)> {
        let off = u32_len(blob.len(), "facet string blob")?;
        let len = u16::try_from(s.len())
            .map_err(|_| io::Error::other("facet name exceeds the 16-bit length limit"))?;
        blob.extend_from_slice(s.as_bytes());
        Ok((off, len))
    };

    let mut fos: Vec<FOut> = Vec::with_capacity(fields.len());
    let mut total_cats: u32 = 0;
    for f in fields {
        let (fno, fnl) = push(&mut blob, &f.name)?;
        let cat_start = total_cats;
        let mut cs: Vec<COut> = Vec::with_capacity(f.cats.len());
        for c in f.cats {
            let (cno, cnl) = push(&mut blob, &c.name)?;
            cs.push(COut {
                key: facet_key(&f.name, &c.name, case_normalization),
                card: c.card,
                name_off: cno,
                name_len: cnl,
                head: c.head,
                tail: c.tail,
            });
        }
        cs.sort_by_key(|c| c.key);
        total_cats += cs.len() as u32;
        fos.push(FOut {
            name_off: fno,
            name_len: fnl,
            cat_start,
            cats: cs,
        });
    }

    let str_blob_off = 24 + fos.len() * 16 + total_cats as usize * 36;
    let postings_start = str_blob_off + blob.len();

    // Header (24 B). `reserved` (offset 6) carries the case-sensitive flag; 0 (case-folding,
    // the default) keeps every existing sidecar byte-identical.
    let reserved: u16 = if case_normalization {
        0
    } else {
        crate::facet::RRSF_FLAG_CASE_SENSITIVE
    };
    w.write_all(b"RRSF")?;
    w.write_all(&1u16.to_le_bytes())?; // version
    w.write_all(&reserved.to_le_bytes())?; // reserved @6 (bit0 = case-sensitive)
    w.write_all(&(fos.len() as u32).to_le_bytes())?;
    w.write_all(&total_cats.to_le_bytes())?;
    w.write_all(&(blob.len() as u32).to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // reserved2

    // Field table (16 B each).
    for fo in &fos {
        w.write_all(&fo.name_off.to_le_bytes())?;
        w.write_all(&fo.name_len.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // pad
        w.write_all(&fo.cat_start.to_le_bytes())?;
        w.write_all(&(fo.cats.len() as u32).to_le_bytes())?;
    }

    // Category table (36 B each) with absolute posting offsets.
    let mut off = postings_start as u64;
    for fo in &fos {
        for c in &fo.cats {
            w.write_all(&c.key.to_le_bytes())?;
            w.write_all(&off.to_le_bytes())?;
            w.write_all(&u32_len(c.head.len(), "RRSF head posting")?.to_le_bytes())?;
            w.write_all(&u32_len(c.tail.len(), "RRSF tail posting")?.to_le_bytes())?;
            w.write_all(&c.card.to_le_bytes())?;
            w.write_all(&c.name_off.to_le_bytes())?;
            w.write_all(&c.name_len.to_le_bytes())?;
            w.write_all(&0u16.to_le_bytes())?; // pad
            off += (c.head.len() + c.tail.len()) as u64;
        }
    }

    w.write_all(&blob)?;

    // Postings: [head][tail] per category, in table order.
    for fo in &fos {
        for c in &fo.cats {
            w.write_all(&c.head)?;
            w.write_all(&c.tail)?;
        }
    }
    Ok(())
}

/// `RRSR` record-store index magic.
pub(crate) const RECORD_MAGIC: &[u8; 4] = b"RRSR";

/// Streaming writer for the `RRSR` record store: record bytes are pushed one at a
/// time in doc-ID order, so a builder that produces records incrementally never
/// has to hold them all in memory. The concatenated record bytes go to `bin` and
/// a range-fetchable offset index to `idx`. Records are opaque to the library —
/// the caller chooses the encoding (JSON, msgpack, …); the store just frames them
/// for O(1) Range lookup by doc ID.
///
/// The `idx` layout (all little-endian) is:
/// - header 16 B: magic `"RRSR"`, version `u16` = 1, reserved `u16`, count `u32`
///   (number of records `N`), reserved2 `u32`;
/// - then `N+1` × `u64` byte offsets into `bin`. Record `d` is
///   `bin[off[d] .. off[d+1]]`, located at `idx[16 + d*8 .. 16 + (d+2)*8]`.
///
/// `count` is written into the header up front, so the caller must know the
/// record total in advance and call [`RecordWriter::write`] exactly that many
/// times (the offset table is sized for `count + 1` entries).
pub struct RecordWriter<W: Write, X: Write> {
    bin: W,
    idx: X,
    /// Cumulative end offset into `bin` (== bytes written so far).
    off: u64,
    /// Number of records written so far.
    written: u32,
    /// Optional reusable zstd compressor with the shared dictionary digested once.
    /// When set, the writer emits the version-2 framed layout: each record becomes
    /// `[tag][payload]`, raw (tag 0) when compression does not shrink it,
    /// zstd-with-dict (tag 1) otherwise. When `None` the writer emits the original
    /// untagged version-1 layout byte-for-byte. The compressor is built once — at
    /// high levels, preparing the dictionary's match-finder tables dominates a small
    /// record's cost, so rebuilding it per record makes a full-corpus store
    /// intractable — and reused for every record. Reuse is byte-identical to a fresh
    /// per-record compressor: each record is an independent one-shot frame over the
    /// same digested dictionary.
    #[cfg(feature = "zstd")]
    zstd: Option<zstd::bulk::Compressor<'static>>,
}

/// Frame tag for a raw (uncompressed) payload in a version-2 store.
#[cfg(feature = "zstd")]
const TAG_RAW: u8 = 0;
/// Frame tag for a zstd frame compressed against the shared dictionary.
#[cfg(feature = "zstd")]
const TAG_ZSTD_DICT: u8 = 1;

impl<W: Write, X: Write> RecordWriter<W, X> {
    /// Opens a streaming record store for `count` records, writing the 16-byte
    /// `RRSR` header and the leading `off[0] = 0` to `idx`. Push each record with
    /// [`RecordWriter::write`] in ascending doc-ID order. Records are stored
    /// uncompressed in the original version-1 (untagged) layout; use
    /// [`RecordWriter::new_zstd`] for the compressed version-2 layout.
    pub fn new(bin: W, mut idx: X, count: u32) -> io::Result<Self> {
        idx.write_all(RECORD_MAGIC)?;
        idx.write_all(&1u16.to_le_bytes())?; // version
        idx.write_all(&0u16.to_le_bytes())?; // reserved
        idx.write_all(&count.to_le_bytes())?; // count
        idx.write_all(&0u32.to_le_bytes())?; // reserved2
        idx.write_all(&0u64.to_le_bytes())?; // off[0] = 0
        Ok(Self {
            bin,
            idx,
            off: 0,
            written: 0,
            #[cfg(feature = "zstd")]
            zstd: None,
        })
    }

    /// Opens a streaming record store for `count` records that **zstd-compresses**
    /// each record against the shared `dict` at compression `level`, writing the
    /// version-2 `RRSR` header (and the leading `off[0] = 0`) to `idx`. Each
    /// record is framed `[tag][payload]`: a record is stored raw (tag 0) when
    /// compression would not shrink it, otherwise as a zstd frame (tag 1). The
    /// `dict` must be shipped to the reader as the `*.dict` sidecar and passed to
    /// [`crate::records::RecordStore::open_with_dict`]. Gated on the `zstd`
    /// feature. Train a dictionary with [`train_record_dict`].
    #[cfg(feature = "zstd")]
    pub fn new_zstd(bin: W, mut idx: X, count: u32, dict: &[u8], level: i32) -> io::Result<Self> {
        idx.write_all(RECORD_MAGIC)?;
        idx.write_all(&2u16.to_le_bytes())?; // version 2: framed records
        idx.write_all(&0u16.to_le_bytes())?; // reserved
        idx.write_all(&count.to_le_bytes())?; // count
        idx.write_all(&0u32.to_le_bytes())?; // reserved2
        idx.write_all(&0u64.to_le_bytes())?; // off[0] = 0
                                             // Build the dictionary-backed compressor once; `write` reuses it per record.
        let compressor = zstd::bulk::Compressor::with_dictionary(level, dict)?;
        Ok(Self {
            bin,
            idx,
            off: 0,
            written: 0,
            zstd: Some(compressor),
        })
    }

    /// Frames one record for the version-2 layout: returns `[tag][payload]`. The
    /// record is compressed against the shared dictionary; if the compressed
    /// payload is not smaller than the raw record, the raw form (tag 0) is kept so
    /// a record never grows. A zero-length record stays zero-length (no tag),
    /// matching the version-1 zero-length convention.
    #[cfg(feature = "zstd")]
    fn frame_zstd(
        compressor: &mut zstd::bulk::Compressor<'static>,
        rec: &[u8],
    ) -> io::Result<Vec<u8>> {
        if rec.is_empty() {
            return Ok(Vec::new());
        }
        let compressed = compressor.compress(rec)?;
        // Both candidate frames pay the same 1-byte tag, so compare payload sizes.
        if compressed.len() < rec.len() {
            let mut framed = Vec::with_capacity(compressed.len() + 1);
            framed.push(TAG_ZSTD_DICT);
            framed.extend_from_slice(&compressed);
            Ok(framed)
        } else {
            let mut framed = Vec::with_capacity(rec.len() + 1);
            framed.push(TAG_RAW);
            framed.extend_from_slice(rec);
            Ok(framed)
        }
    }

    /// Appends one record's bytes to the blob and its cumulative end offset to the
    /// index. A zero-length record (a doc with no stored fields) stays addressable.
    /// A compressing writer (see [`RecordWriter::new_zstd`]) frames the record
    /// first; a plain writer stores the bytes verbatim.
    pub fn write(&mut self, rec: &[u8]) -> io::Result<()> {
        #[cfg(feature = "zstd")]
        let framed = match &mut self.zstd {
            Some(compressor) => Some(Self::frame_zstd(compressor, rec)?),
            None => None,
        };
        #[cfg(feature = "zstd")]
        let rec: &[u8] = match &framed {
            Some(f) => f,
            None => rec,
        };
        self.bin.write_all(rec)?;
        self.off += rec.len() as u64;
        self.idx.write_all(&self.off.to_le_bytes())?;
        self.written += 1;
        Ok(())
    }

    /// Number of records written so far.
    pub fn written(&self) -> u32 {
        self.written
    }

    /// Flushes both underlying writers, surfacing any buffered-write error. Useful
    /// when the writers are buffered (e.g. `BufWriter`) and the caller wants to
    /// propagate a flush failure rather than rely on drop.
    pub fn flush(&mut self) -> io::Result<()> {
        self.bin.flush()?;
        self.idx.flush()
    }
}

/// Writes a record store from an in-memory slice of records, in doc-ID order — a
/// convenience over [`RecordWriter`] for callers that already hold every record.
pub fn write_records<W: Write, X: Write>(bin: W, idx: X, records: &[Vec<u8>]) -> io::Result<()> {
    let mut w = RecordWriter::new(bin, idx, records.len() as u32)?;
    for rec in records {
        w.write(rec)?;
    }
    Ok(())
}

/// Writes a **zstd-compressed** record store from an in-memory slice of records,
/// in doc-ID order — the compressing counterpart of [`write_records`]. Each
/// record is framed and compressed against the shared `dict` at compression
/// `level` (a record that does not shrink is kept raw, so it never grows). The
/// resulting version-2 store reads back through
/// [`crate::records::RecordStore::open_with_dict`] with the same `dict`, which
/// must be shipped to the reader as the `*.dict` sidecar. Gated on the `zstd`
/// feature. Train a `dict` with [`train_record_dict`].
#[cfg(feature = "zstd")]
pub fn write_records_zstd<W: Write, X: Write>(
    bin: W,
    idx: X,
    records: &[Vec<u8>],
    dict: &[u8],
    level: i32,
) -> io::Result<()> {
    let mut w = RecordWriter::new_zstd(bin, idx, records.len() as u32, dict, level)?;
    for rec in records {
        w.write(rec)?;
    }
    Ok(())
}

/// Trains a shared zstd dictionary from representative record `samples`, capped
/// at `max_dict_bytes`. Records are small and self-similar (repeated JSON keys,
/// common venues/authors), so a trained dictionary recovers big-block ratio on
/// per-record units without the fetch amplification of large blocks. Pass the
/// returned dictionary to [`write_records_zstd`] / [`RecordWriter::new_zstd`] at
/// build time and ship it to the reader as the `*.dict` sidecar. Gated on the
/// `zstd` feature. See `RECORDS.md`.
#[cfg(feature = "zstd")]
pub fn train_record_dict(samples: &[&[u8]], max_dict_bytes: usize) -> io::Result<Vec<u8>> {
    zstd::dict::from_samples(samples, max_dict_bytes)
}

/// `RRIL` identifier exact-match index magic.
pub(crate) const LOOKUP_MAGIC: &[u8; 4] = b"RRIL";

/// Writes the `RRIL` identifier exact-match index for `entries` to `w` — the
/// build-side mirror of [`crate::lookup::Lookup`]. Each entry is an
/// `(identifier, doc)` pair: the identifier is normalized with
/// [`crate::lookup::normalize_id`] and double-hashed (FNV-1a primary + verify),
/// so the reader resolves the same identifier byte-for-byte. Entries are sorted
/// by `(hash, doc)` here — the reader binary-searches the hash and scans the
/// matching run in ascending doc (rank) order — and an empty normalized
/// identifier is dropped (it can never be looked up). The `*.rril` layout (all
/// little-endian) is:
/// - header 16 B: magic `"RRIL"`, version `u16` = 1, reserved `u16`, count `u32`
///   (number of records `N`), reserved2 `u32`;
/// - then `N` × `[hash u64][verify u32][doc u32]`, sorted by `(hash, doc)`.
///
/// See `lookup.rs` for the reader and the hashing/normalization details.
pub fn write_lookup<W: Write>(w: W, entries: &[(String, u32)]) -> io::Result<()> {
    write_lookup_streaming(w, entries.iter().cloned())
}

/// Streaming counterpart of [`write_lookup`]: consumes an *iterator* of
/// `(identifier, doc)` pairs, hashing and **dropping each identifier string as it
/// is consumed**, so only the fixed-width `(hash, verify, doc)` triples (16 bytes
/// each) are retained — never the identifier strings. This bounds peak memory to
/// the triple table when a full-corpus builder streams hundreds of millions of
/// identifiers in from disk, where holding every `String` would be many times
/// larger. The output is byte-for-byte identical to [`write_lookup`] over the same
/// pairs in the same order: identifiers are normalized, double-hashed, empties
/// dropped, and the records sorted by `(hash, doc)` exactly as there.
pub fn write_lookup_streaming<W: Write, I: IntoIterator<Item = (String, u32)>>(
    mut w: W,
    entries: I,
) -> io::Result<()> {
    use crate::lookup::{fnv64a_basis, normalize_id, FNV_OFFSET, FNV_VERIFY_BASIS};

    let mut recs: Vec<(u64, u32, u32)> = entries
        .into_iter()
        .filter_map(|(id, doc)| {
            let n = normalize_id(&id);
            if n.is_empty() {
                return None;
            }
            Some((
                fnv64a_basis(&n, FNV_OFFSET),
                fnv64a_basis(&n, FNV_VERIFY_BASIS) as u32,
                doc,
            ))
        })
        .collect();
    recs.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));

    // Header (16 B).
    w.write_all(LOOKUP_MAGIC)?;
    w.write_all(&1u16.to_le_bytes())?; // version
    w.write_all(&0u16.to_le_bytes())?; // reserved
    w.write_all(&(recs.len() as u32).to_le_bytes())?; // count
    w.write_all(&0u32.to_le_bytes())?; // reserved2

    // Records (16 B each): [hash u64][verify u32][doc u32], sorted by (hash, doc).
    for (hash, verify, doc) in &recs {
        w.write_all(&hash.to_le_bytes())?;
        w.write_all(&verify.to_le_bytes())?;
        w.write_all(&doc.to_le_bytes())?;
    }
    Ok(())
}

/// `RRSC` sort-column store magic.
pub(crate) const SORTCOLS_MAGIC: &[u8; 4] = b"RRSC";

/// One sort column's dense values, in doc-ID order. The variant selects the
/// on-disk value type (`u16`/`u32`/`i32`/`f32`); every column in a store must hold
/// the same number of values (one per doc). See `SORTCOLS.md`.
pub enum ColumnValues {
    /// Unsigned 16-bit values.
    U16(Vec<u16>),
    /// Unsigned 32-bit values.
    U32(Vec<u32>),
    /// Signed 32-bit values.
    I32(Vec<i32>),
    /// IEEE-754 32-bit float values.
    F32(Vec<f32>),
}

impl ColumnValues {
    /// Number of values (== the store's row/doc count).
    fn len(&self) -> usize {
        match self {
            ColumnValues::U16(v) => v.len(),
            ColumnValues::U32(v) => v.len(),
            ColumnValues::I32(v) => v.len(),
            ColumnValues::F32(v) => v.len(),
        }
    }

    /// The on-disk value-type code (`1`=u16, `2`=u32, `3`=i32, `4`=f32).
    fn type_code(&self) -> u8 {
        match self {
            ColumnValues::U16(_) => 1,
            ColumnValues::U32(_) => 2,
            ColumnValues::I32(_) => 3,
            ColumnValues::F32(_) => 4,
        }
    }

    /// Width in bytes of one stored value.
    fn width(&self) -> usize {
        match self {
            ColumnValues::U16(_) => 2,
            _ => 4,
        }
    }

    /// Writes the dense values to `w` in doc-ID order, little-endian.
    fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            ColumnValues::U16(v) => {
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
            ColumnValues::U32(v) => {
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
            ColumnValues::I32(v) => {
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
            ColumnValues::F32(v) => {
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }
}

/// One named sort column: a display name plus its dense values in doc-ID order.
pub struct SortColumn {
    /// Column display name.
    pub name: String,
    /// Dense values, one per doc, in doc-ID order.
    pub values: ColumnValues,
}

/// Writes the `RRSC` sort-column store for `cols` to `w` — the build-side mirror of
/// [`crate::sortcols::SortCols`]. Columns are laid out contiguously after a name
/// string blob, each a dense fixed-width array indexed by doc ID. Every column must
/// hold the same number of values (one per doc); otherwise this errors. See
/// `SORTCOLS.md`.
pub fn write_sortcols<W: Write>(mut w: W, cols: Vec<SortColumn>) -> io::Result<()> {
    let rows = cols.first().map(|c| c.values.len()).unwrap_or(0);
    if cols.iter().any(|c| c.values.len() != rows) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sortcols columns must all have the same length",
        ));
    }

    // Column count is a u16 header field; reject rather than truncate. Mirrors the Go writer.
    if cols.len() > u16::MAX as usize {
        return Err(io::Error::other(
            "sortcols column count exceeds the 16-bit limit",
        ));
    }
    // String blob of column names, in column order. Name length (u16) and blob offset
    // (u32) are on-disk fields; error rather than silently truncating either.
    let mut blob: Vec<u8> = Vec::new();
    let mut name_spans: Vec<(u32, u16)> = Vec::with_capacity(cols.len());
    for c in &cols {
        let off = u32_len(blob.len(), "sortcols string blob")?;
        let name_len = u16::try_from(c.name.len()).map_err(|_| {
            io::Error::other("sortcols column name exceeds the 16-bit length limit")
        })?;
        blob.extend_from_slice(c.name.as_bytes());
        name_spans.push((off, name_len));
    }
    let blob_len = u32_len(blob.len(), "sortcols string blob")?;

    let str_blob_off = HEADER_SIZE_SORTCOLS + cols.len() * COL_ENTRY_SORTCOLS;
    let data_start = (str_blob_off + blob.len()) as u64;

    // Header (16 B).
    w.write_all(SORTCOLS_MAGIC)?;
    w.write_all(&1u16.to_le_bytes())?; // version
    w.write_all(&(cols.len() as u16).to_le_bytes())?;
    w.write_all(&(rows as u32).to_le_bytes())?;
    w.write_all(&blob_len.to_le_bytes())?;

    // Column table (24 B each) with absolute data offsets, in column order.
    let mut off = data_start;
    for (c, (name_off, name_len)) in cols.iter().zip(&name_spans) {
        w.write_all(&name_off.to_le_bytes())?;
        w.write_all(&name_len.to_le_bytes())?;
        w.write_all(&[c.values.type_code()])?;
        w.write_all(&[0u8])?; // pad
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&(rows as u32).to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // reserved
        off += (rows * c.values.width()) as u64;
    }

    w.write_all(&blob)?;

    // Data: each column's dense values, in column order (matching the offsets).
    for c in &cols {
        c.values.write_to(&mut w)?;
    }
    Ok(())
}

/// Writes a one-column `u32` `RRSC` store named `"primary"` mapping a secondary
/// doc-ID space back to the primary one: `primary_of_secondary[secondary_id]` is the
/// primary doc ID. This is the permutation a [secondary full index](SORTCOLS.md)
/// uses to fetch primary-keyed records/facets for its results.
pub fn write_perm<W: Write>(w: W, primary_of_secondary: Vec<u32>) -> io::Result<()> {
    write_sortcols(
        w,
        vec![SortColumn {
            name: "primary".to_string(),
            values: ColumnValues::U32(primary_of_secondary),
        }],
    )
}

/// `RRSC` header size in bytes.
const HEADER_SIZE_SORTCOLS: usize = 16;
/// `RRSC` column-table entry size in bytes.
const COL_ENTRY_SORTCOLS: usize = 24;

/// Chunked build: doc-ID-range partials + merge into one standard RRS.
///
/// For a corpus whose index exceeds RAM, the builder partitions the doc-ID space
/// into contiguous chunks, builds each chunk's index in bounded memory, and writes
/// a key-sorted *partial* per chunk. [`merge_partials_to_rrs`] folds the partials
/// into one ordinary RRS: because chunks hold disjoint doc-ID ranges, a key's full
/// posting is just the union of its per-chunk postings. The merge streams by key,
/// so peak memory is one key's postings plus a small dictionary — not the whole
/// index — and the reader/format are unchanged.
pub mod chunk {
    use super::{serialize_posting, DEFAULT_STRIDE, FORMAT_VERSION, HEADER_SIZE};
    use roaring::RoaringBitmap;
    use std::fs::File;
    use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    /// Writes one chunk's partial index to `w`: `[count u32]` then key-sorted
    /// `[(key u64)(size u32)(posting bytes)]`. The posting is the chunk's whole
    /// bitmap for the key (not yet split head/tail — the merge does that).
    pub fn write_partial<W: Write>(mut w: W, mut entries: Vec<(u64, Vec<u8>)>) -> io::Result<()> {
        entries.sort_by_key(|e| e.0);
        w.write_all(&(entries.len() as u32).to_le_bytes())?;
        for (k, b) in &entries {
            w.write_all(&k.to_le_bytes())?;
            w.write_all(&super::u32_len(b.len(), "RRS partial posting")?.to_le_bytes())?;
            w.write_all(b)?;
        }
        Ok(())
    }

    /// Streaming cursor over a partial, exposing the front entry in key order.
    struct PartialCursor {
        r: BufReader<File>,
        remaining: u32,
        front: Option<(u64, Vec<u8>)>,
    }

    impl PartialCursor {
        fn open(path: &PathBuf) -> io::Result<Self> {
            let mut r = BufReader::new(File::open(path)?);
            let mut c = [0u8; 4];
            r.read_exact(&mut c)?;
            let mut cur = PartialCursor {
                r,
                remaining: u32::from_le_bytes(c),
                front: None,
            };
            cur.advance()?;
            Ok(cur)
        }
        fn advance(&mut self) -> io::Result<()> {
            if self.remaining == 0 {
                self.front = None;
                return Ok(());
            }
            let mut kb = [0u8; 8];
            self.r.read_exact(&mut kb)?;
            let mut sb = [0u8; 4];
            self.r.read_exact(&mut sb)?;
            let mut bytes = vec![0u8; u32::from_le_bytes(sb) as usize];
            self.r.read_exact(&mut bytes)?;
            self.remaining -= 1;
            self.front = Some((u64::from_le_bytes(kb), bytes));
            Ok(())
        }
        fn front_key(&self) -> Option<u64> {
            self.front.as_ref().map(|(k, _)| *k)
        }
    }

    /// Reads only the keys from a partial (skipping posting bytes) into `sink`.
    fn scan_partial_keys(path: &PathBuf, sink: &mut impl FnMut(u64)) -> io::Result<()> {
        let mut r = BufReader::new(File::open(path)?);
        let mut c = [0u8; 4];
        r.read_exact(&mut c)?;
        for _ in 0..u32::from_le_bytes(c) {
            let mut kb = [0u8; 8];
            r.read_exact(&mut kb)?;
            let mut sb = [0u8; 4];
            r.read_exact(&mut sb)?;
            sink(u64::from_le_bytes(kb));
            r.seek(SeekFrom::Current(u32::from_le_bytes(sb) as i64))?;
        }
        Ok(())
    }

    /// Merges chunk partials (key-sorted, disjoint doc-ID sets) into one standard
    /// RRS at `out`. Streams by key — peak memory is one key's postings plus a
    /// per-key dictionary, never the whole index. Requires a seekable output.
    pub fn merge_partials_to_rrs(
        paths: &[PathBuf],
        gram_size: u16,
        stride: u32,
        out: &mut File,
    ) -> io::Result<()> {
        let stride = if stride == 0 { DEFAULT_STRIDE } else { stride };

        // Pass A: the union of keys (posting bytes skipped) → dictionary sizing.
        let mut keyset = std::collections::BTreeSet::new();
        for p in paths {
            scan_partial_keys(p, &mut |k| {
                keyset.insert(k);
            })?;
        }
        let n = keyset.len();
        drop(keyset);
        let sparse_count = if n == 0 {
            0
        } else {
            n.div_ceil(stride as usize)
        };
        let dict_start = HEADER_SIZE + sparse_count * 8;
        let postings_start = (dict_start + n * 20) as u64;

        // Pass B: k-way merge by key, writing one posting per key; record dict entries in order.
        let mut cursors: Vec<PartialCursor> = paths
            .iter()
            .map(PartialCursor::open)
            .collect::<io::Result<_>>()?;
        let mut dict: Vec<(u64, u64, u32)> = Vec::with_capacity(n);
        out.seek(SeekFrom::Start(postings_start))?;
        let mut off = postings_start;

        while let Some(key) = cursors.iter().filter_map(|c| c.front_key()).min() {
            let mut merged = RoaringBitmap::new();
            for c in &mut cursors {
                if c.front_key() == Some(key) {
                    let (_, bytes) = c.front.take().unwrap();
                    merged |= RoaringBitmap::deserialize_from(&bytes[..])
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    c.advance()?;
                }
            }
            let posting = serialize_posting(&merged);
            out.write_all(&posting)?;
            dict.push((key, off, super::u32_len(posting.len(), "RRS posting")?));
            off += posting.len() as u64;
        }

        // Header + sparse index + dictionary (dict is already key-sorted).
        out.seek(SeekFrom::Start(0))?;
        out.write_all(b"RRSI")?;
        out.write_all(&FORMAT_VERSION.to_le_bytes())?;
        out.write_all(&gram_size.to_le_bytes())?;
        out.write_all(&(n as u32).to_le_bytes())?;
        out.write_all(&stride.to_le_bytes())?;
        super::write_sparse_index(out, &dict, sparse_count, stride as usize, |d| d.0)?;
        for (key, off, size) in &dict {
            out.write_all(&key.to_le_bytes())?;
            out.write_all(&off.to_le_bytes())?;
            out.write_all(&size.to_le_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facet::FacetIndex;
    use crate::index::Index;
    use crate::ngram::ngram_keys;
    use crate::MemoryFetch;
    use futures::executor::block_on;
    use std::fs::File;

    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    fn rrs(entries: &[(u64, RoaringBitmap)]) -> Vec<u8> {
        let posts: Vec<(u64, Vec<u8>)> = entries
            .iter()
            .map(|(k, b)| (*k, serialize_posting(b)))
            .collect();
        let mut out = Vec::new();
        write_index(&mut out, 3, 2, posts).unwrap();
        out
    }

    #[test]
    fn write_index_rejects_duplicate_keys() {
        // Two entries with the same key would make the sorted byte order tie-break-
        // dependent and the dictionary binary search ambiguous, so it must error.
        let posts = vec![
            (7u64, serialize_posting(&bm(&[0]))),
            (7u64, serialize_posting(&bm(&[1]))),
        ];
        let err = write_index(&mut Vec::new(), 3, 2, posts).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rrs_round_trips_through_reader() {
        let abc = ngram_keys("abc", 3)[0];
        let bcd = ngram_keys("bcd", 3)[0];
        let buf = rrs(&[
            (abc, bm(&[1, 3, 5, HEAD_BOUNDARY + 1])),
            (bcd, bm(&[3, 5, HEAD_BOUNDARY + 1])),
        ]);
        let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();
        assert_eq!(idx.gram_size(), 3);
        assert_eq!(idx.ngram_count(), 2);
        // Single trigram, ascending (= rank), spanning head into tail.
        assert_eq!(
            block_on(idx.search("abc", 10)).unwrap(),
            vec![1, 3, 5, HEAD_BOUNDARY + 1]
        );
        // AND of both trigrams.
        assert_eq!(
            block_on(idx.search("abcd", 10)).unwrap(),
            vec![3, 5, HEAD_BOUNDARY + 1]
        );
    }

    #[test]
    fn rrsf_round_trips_through_reader() {
        let buf = {
            let mut out = Vec::new();
            let mk = |name: &str, card: u32, b: RoaringBitmap| {
                let (head, tail) = split_posting(&b, HEAD_BOUNDARY);
                FacetCategory {
                    name: name.to_string(),
                    card,
                    head,
                    tail,
                }
            };
            let fields = vec![
                FacetField {
                    name: "format".to_string(),
                    cats: vec![mk("ebook", 3, bm(&[1, 3, 5])), mk("audio", 2, bm(&[2, 4]))],
                },
                FacetField {
                    name: "lang".to_string(),
                    cats: vec![mk("en", 3, bm(&[1, 2, 3]))],
                },
            ];
            write_facets(&mut out, fields).unwrap();
            out
        };
        let facets = block_on(FacetIndex::open(MemoryFetch::new(buf))).unwrap();
        assert_eq!(facets.fields().len(), 2);
        let fmt = facets.fields().iter().find(|f| f.name == "format").unwrap();
        let ebook = fmt.categories.iter().find(|c| c.name == "ebook").unwrap();
        assert_eq!(ebook.count, 3);
        let lang = facets.fields().iter().find(|f| f.name == "lang").unwrap();
        assert_eq!(lang.categories[0].name, "en");
    }

    #[test]
    fn record_store_frames_for_range_lookup() {
        let recs: Vec<Vec<u8>> = vec![
            br#"{"id":"A","c":9}"#.to_vec(),
            Vec::new(), // a doc with no record stays addressable (zero-length)
            b"hello".to_vec(),
        ];
        let mut bin = Vec::new();
        let mut idx = Vec::new();
        write_records(&mut bin, &mut idx, &recs).unwrap();

        assert_eq!(&idx[0..4], RECORD_MAGIC);
        assert_eq!(u16::from_le_bytes(idx[4..6].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(idx[8..12].try_into().unwrap()), 3);

        let off =
            |d: usize| u64::from_le_bytes(idx[16 + d * 8..24 + d * 8].try_into().unwrap()) as usize;
        for (d, rec) in recs.iter().enumerate() {
            assert_eq!(&bin[off(d)..off(d + 1)], rec.as_slice());
        }
    }

    #[test]
    fn write_lookup_streaming_matches_slice_writer() {
        // Mixed casing, URL-prefixed and bare DOIs, an empty identifier (dropped),
        // and duplicate hashes across docs (exercising the (hash, doc) tiebreak).
        let entries: Vec<(String, u32)> = vec![
            ("10.1234/AbCd".to_string(), 7),
            ("https://doi.org/10.1/x".to_string(), 2),
            ("".to_string(), 99),
            ("10.1234/abcd".to_string(), 1),
            ("10.5/zzz".to_string(), 5),
        ];
        let mut want = Vec::new();
        write_lookup(&mut want, &entries).unwrap();
        let mut got = Vec::new();
        // Owned iterator (no backing slice retained) — the streaming path.
        write_lookup_streaming(&mut got, entries).unwrap();
        assert_eq!(got, want, "streaming lookup differs from slice writer");
    }

    #[test]
    fn merge_partials_round_trips_through_reader() {
        use crate::index::Index;
        use crate::ngram::ngram_keys;
        use crate::MemoryFetch;
        use futures::executor::block_on;
        use std::io::Read as _;

        fn full(docs: &[u32]) -> Vec<u8> {
            let mut b = RoaringBitmap::new();
            for &d in docs {
                b.insert(d);
            }
            let mut v = Vec::new();
            b.serialize_into(&mut v).unwrap();
            v
        }

        let abc = ngram_keys("abc", 3)[0];
        let bcd = ngram_keys("bcd", 3)[0];
        let dir = std::env::temp_dir();
        let p0 = dir.join("rr_merge_p0.partial");
        let p1 = dir.join("rr_merge_p1.partial");
        let op = dir.join("rr_merge_out.rrs");

        // chunk 0: docs in [0, 65536); chunk 1: docs >= 65536 — disjoint ranges.
        chunk::write_partial(
            File::create(&p0).unwrap(),
            vec![(abc, full(&[1, 3])), (bcd, full(&[3]))],
        )
        .unwrap();
        chunk::write_partial(
            File::create(&p1).unwrap(),
            vec![(abc, full(&[65536, 65540]))],
        )
        .unwrap();

        let mut out = File::create(&op).unwrap();
        chunk::merge_partials_to_rrs(&[p0.clone(), p1.clone()], 3, 2, &mut out).unwrap();
        drop(out);

        let mut buf = Vec::new();
        File::open(&op).unwrap().read_to_end(&mut buf).unwrap();
        let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();
        assert_eq!(idx.ngram_count(), 2);
        // "abc" spans both chunks: head {1,3} then tail {65536,65540}, ascending.
        assert_eq!(
            block_on(idx.search("abc", 10)).unwrap(),
            vec![1, 3, 65536, 65540]
        );
        // "abcd" = abc ∩ bcd = {3}.
        assert_eq!(block_on(idx.search("abcd", 10)).unwrap(), vec![3]);

        for p in [p0, p1, op] {
            let _ = std::fs::remove_file(p);
        }
    }
}
