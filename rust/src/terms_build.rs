//! Native writer for the `RRTI` term-level inverted index — the build-side mirror
//! of [`crate::terms`], emitting the byte layout in `TERMS.md`.
//!
//! Postings are portable RoaringBitmaps split into head/tail by the same
//! [`crate::build::split_posting`] the trigram index uses, so the term index
//! inherits the rank-ordered head and the whole reader posting path unchanged —
//! only the dictionary (an FST keyed by whole term) is new.

use crate::build::split_posting;
use crate::terms::tokenize;
use fst::MapBuilder;
use roaring::RoaringBitmap;
use std::collections::BTreeMap;
use std::io::{self, Write};

/// `RRTI` magic.
const MAGIC: &[u8; 4] = b"RRTI";
/// Format version written into the header.
const VERSION: u16 = 1;
/// Bits of the FST output reserved for the head posting's byte length; the rest
/// hold the block's byte offset within the postings region (see [`crate::terms`]).
const SIZE_BITS: u32 = 24;

/// Builds an `RRTI` term index over `(doc_id, text)` documents and writes it to
/// `w`. Doc IDs should be the shared rank-order IDs (so facets/records/vector
/// compose); `head_boundary` is the doc-ID head/tail split (a multiple of 65536,
/// e.g. [`crate::build::DEFAULT_HEAD_BOUNDARY`]). The text of each document is
/// tokenized with [`tokenize`] (the same function the reader's query path uses).
pub fn write_term_index<W: Write>(
    mut w: W,
    docs: &[(u32, &str)],
    head_boundary: u32,
) -> io::Result<()> {
    // 1. term -> doc-id bitmap. A BTreeMap keeps terms in byte-lexicographic order
    // (UTF-8 byte order == codepoint order == `str` ordering), which is exactly the
    // strictly-increasing order the FST builder requires.
    let mut postings: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
    for &(doc, text) in docs {
        for term in tokenize(text) {
            postings.entry(term).or_default().insert(doc);
        }
    }

    // 2. Lay out the postings region and record each term's packed FST output.
    // Each block is `[tail_size: u32 LE][head bytes][tail bytes]`; the FST output
    // packs `(head_off << 24) | head_size`.
    let mut region: Vec<u8> = Vec::new();
    let mut outputs: Vec<u64> = Vec::with_capacity(postings.len());
    for (term, bm) in &postings {
        let (head, tail) = split_posting(bm, head_boundary);
        let head_off = region.len() as u64;
        let head_size = head.len();
        if head_size >= (1usize << SIZE_BITS) {
            return Err(io::Error::other(format!(
                "term {term:?}: head posting {head_size} B exceeds the 24-bit size limit"
            )));
        }
        if head_off >= (1u64 << (64 - SIZE_BITS)) {
            return Err(io::Error::other(
                "postings region exceeds the 40-bit offset limit",
            ));
        }
        region.extend_from_slice(&(tail.len() as u32).to_le_bytes());
        region.extend_from_slice(&head);
        region.extend_from_slice(&tail);
        outputs.push((head_off << SIZE_BITS) | head_size as u64);
    }

    // 3. Build the FST term dictionary: term -> packed output, in sorted order.
    let mut builder = MapBuilder::memory();
    for (term, out) in postings.keys().zip(&outputs) {
        builder
            .insert(term.as_bytes(), *out)
            .map_err(|e| io::Error::other(format!("fst insert: {e}")))?;
    }
    let fst_bytes = builder
        .into_inner()
        .map_err(|e| io::Error::other(format!("fst finish: {e}")))?;

    // 4. Header (32 B) + FST dictionary + postings region.
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // flags (reserved)
    w.write_all(&(postings.len() as u32).to_le_bytes())?;
    w.write_all(&head_boundary.to_le_bytes())?;
    w.write_all(&(fst_bytes.len() as u64).to_le_bytes())?;
    w.write_all(&[0u8; 8])?; // reserved — pads the header to 32 bytes
    w.write_all(&fst_bytes)?;
    w.write_all(&region)?;
    Ok(())
}
