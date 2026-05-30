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

/// Head/tail boundary: docs `[0, HEAD_BOUNDARY)` form the head posting (the first
/// roaring container, i.e. the top-ranked docs), the rest form the tail.
pub const HEAD_BOUNDARY: u32 = 65536;

/// FNV-1a 64-bit offset basis / prime (shared by the facet key derivation).
const FNV_OFFSET64: u64 = 14695981039346656037;
const FNV_PRIME64: u64 = 1099511628211;

/// Splits `bm` into the head bitmap (docs `[0, 65536)`) and tail bitmap (docs
/// `[65536, ∞)`), each serialized as a portable RoaringBitmap. Mirrors the Go
/// `splitBitmap`: intersect a head-range mask for the head, clone-and-trim for
/// the tail (avoids materializing a full-range mask per posting).
pub fn split_posting(bm: &RoaringBitmap) -> (Vec<u8>, Vec<u8>) {
    let mut head = RoaringBitmap::new();
    head.insert_range(0..HEAD_BOUNDARY);
    head &= bm;
    let mut tail = bm.clone();
    tail.remove_range(0..HEAD_BOUNDARY);

    let mut hb = Vec::with_capacity(head.serialized_size());
    head.serialize_into(&mut hb).expect("serialize head bitmap");
    let mut tb = Vec::with_capacity(tail.serialized_size());
    tail.serialize_into(&mut tb).expect("serialize tail bitmap");
    (hb, tb)
}

/// Derives the facet category key: FNV-1a 64-bit over `lower(field)`, a `0x1f`
/// separator byte, then `lower(category)`. Mirrors Go `FacetKey` (see
/// `FACETS.md`). Informational for the Phase-1 sidecar reader (which resolves by
/// name), but written so the file matches the spec and sorts deterministically.
pub fn facet_key(field: &str, category: &str) -> u64 {
    let mut h = FNV_OFFSET64;
    for b in field.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME64);
    }
    h ^= 0x1f;
    h = h.wrapping_mul(FNV_PRIME64);
    for b in category.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME64);
    }
    h
}

/// Default sparse-index stride (matches Go `DefaultStride`).
pub const DEFAULT_STRIDE: u32 = 512;

/// Writes the `RRS` index for the given postings to `w`. Each entry is
/// `(key, head_bytes, tail_bytes)` from [`split_posting`]; entries are sorted by
/// key here (the dictionary must be key-sorted). A `stride` of 0 becomes
/// [`DEFAULT_STRIDE`]. See `FORMAT.md`.
pub fn write_rrs<W: Write>(
    mut w: W,
    gram_size: u16,
    stride: u32,
    mut entries: Vec<(u64, Vec<u8>, Vec<u8>)>,
) -> io::Result<()> {
    entries.sort_by_key(|e| e.0);
    let stride = if stride == 0 { DEFAULT_STRIDE } else { stride };
    let ngrams = entries.len() as u32;
    let sparse_count = if ngrams == 0 {
        0
    } else {
        (ngrams as usize).div_ceil(stride as usize)
    };
    let dict_start = 16 + sparse_count * 8;
    let postings_start = dict_start + entries.len() * 24;

    // Header (16 B).
    w.write_all(b"RRSI")?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&gram_size.to_le_bytes())?;
    w.write_all(&ngrams.to_le_bytes())?;
    w.write_all(&stride.to_le_bytes())?;

    // Sparse index: dict[i*stride].key.
    for i in 0..sparse_count {
        w.write_all(&entries[i * stride as usize].0.to_le_bytes())?;
    }

    // Dictionary (24 B each) with absolute posting offsets.
    let mut off = postings_start as u64;
    for (key, head, tail) in &entries {
        w.write_all(&key.to_le_bytes())?;
        w.write_all(&off.to_le_bytes())?;
        w.write_all(&(head.len() as u32).to_le_bytes())?;
        w.write_all(&(tail.len() as u32).to_le_bytes())?;
        off += (head.len() + tail.len()) as u64;
    }

    // Postings: [head][tail] per entry, in dict order.
    for (_, head, tail) in &entries {
        w.write_all(head)?;
        w.write_all(tail)?;
    }
    Ok(())
}

/// One category prepared for the facet sidecar: display name, its split posting,
/// and full-corpus cardinality.
pub struct FacetCatOut {
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
pub struct FacetFieldOut {
    /// Field display name.
    pub name: String,
    /// The field's categories.
    pub cats: Vec<FacetCatOut>,
}

/// Writes the `RRSF` facet sidecar for `fields` to `w`. The string blob is built
/// in field/category insertion order (matching Go `WriteFacets`); each field's
/// categories are sorted by [`facet_key`] for the category table and postings.
/// See `FACETS.md`.
pub fn write_rrsf<W: Write>(mut w: W, fields: Vec<FacetFieldOut>) -> io::Result<()> {
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
    let push = |blob: &mut Vec<u8>, s: &str| -> (u32, u16) {
        let off = blob.len() as u32;
        blob.extend_from_slice(s.as_bytes());
        (off, s.len() as u16)
    };

    let mut fos: Vec<FOut> = Vec::with_capacity(fields.len());
    let mut total_cats: u32 = 0;
    for f in fields {
        let (fno, fnl) = push(&mut blob, &f.name);
        let cat_start = total_cats;
        let mut cs: Vec<COut> = Vec::with_capacity(f.cats.len());
        for c in f.cats {
            let (cno, cnl) = push(&mut blob, &c.name);
            cs.push(COut {
                key: facet_key(&f.name, &c.name),
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

    // Header (24 B).
    w.write_all(b"RRSF")?;
    w.write_all(&1u16.to_le_bytes())?; // version
    w.write_all(&0u16.to_le_bytes())?; // reserved
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
            w.write_all(&(c.head.len() as u32).to_le_bytes())?;
            w.write_all(&(c.tail.len() as u32).to_le_bytes())?;
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
pub const RECORD_MAGIC: &[u8; 4] = b"RRSR";

/// Writes a record store: the concatenated record bytes to `bin` (in doc-ID
/// order) and a range-fetchable offset index to `idx`. Records are opaque to the
/// library — the caller chooses the encoding (JSON, msgpack, …); the store just
/// frames them for O(1) Range lookup by doc ID.
///
/// The `idx` layout (all little-endian) is:
/// - header 16 B: magic `"RRSR"`, version `u16` = 1, reserved `u16`, count `u32`
///   (number of records `N`), reserved2 `u32`;
/// - then `N+1` × `u64` byte offsets into `bin`. Record `d` is
///   `bin[off[d] .. off[d+1]]`, located at `idx[16 + d*8 .. 16 + (d+2)*8]`.
pub fn write_records<W: Write, X: Write>(
    mut bin: W,
    mut idx: X,
    records: &[Vec<u8>],
) -> io::Result<()> {
    idx.write_all(RECORD_MAGIC)?;
    idx.write_all(&1u16.to_le_bytes())?; // version
    idx.write_all(&0u16.to_le_bytes())?; // reserved
    idx.write_all(&(records.len() as u32).to_le_bytes())?; // count
    idx.write_all(&0u32.to_le_bytes())?; // reserved2

    idx.write_all(&0u64.to_le_bytes())?; // off[0] = 0
    let mut off: u64 = 0;
    for rec in records {
        bin.write_all(rec)?;
        off += rec.len() as u64;
        idx.write_all(&off.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facet::FacetIndex;
    use crate::index::Index;
    use crate::ngram::ngram_keys;
    use crate::MemoryFetch;
    use futures::executor::block_on;

    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    fn rrs(entries: &[(u64, RoaringBitmap)]) -> Vec<u8> {
        let posts: Vec<(u64, Vec<u8>, Vec<u8>)> = entries
            .iter()
            .map(|(k, b)| {
                let (h, t) = split_posting(b);
                (*k, h, t)
            })
            .collect();
        let mut out = Vec::new();
        write_rrs(&mut out, 3, 2, posts).unwrap();
        out
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
        assert_eq!(idx.gram_size, 3);
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
                let (head, tail) = split_posting(&b);
                FacetCatOut {
                    name: name.to_string(),
                    card,
                    head,
                    tail,
                }
            };
            let fields = vec![
                FacetFieldOut {
                    name: "format".to_string(),
                    cats: vec![mk("ebook", 3, bm(&[1, 3, 5])), mk("audio", 2, bm(&[2, 4]))],
                },
                FacetFieldOut {
                    name: "lang".to_string(),
                    cats: vec![mk("en", 3, bm(&[1, 2, 3]))],
                },
            ];
            write_rrsf(&mut out, fields).unwrap();
            out
        };
        let facets = block_on(FacetIndex::open(MemoryFetch::new(buf))).unwrap();
        assert_eq!(facets.fields.len(), 2);
        let fmt = facets.fields.iter().find(|f| f.name == "format").unwrap();
        let ebook = fmt.categories.iter().find(|c| c.name == "ebook").unwrap();
        assert_eq!(ebook.count, 3);
        let lang = facets.fields.iter().find(|f| f.name == "lang").unwrap();
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
}
