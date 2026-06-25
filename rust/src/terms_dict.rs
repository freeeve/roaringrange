//! Front-coded block codec for the `RRTI` v2 term dictionary — the
//! Quickwit/`tantivy-sstable`-style blocked dictionary.
//!
//! Terms are grouped into byte-capped, **front-coded** blocks that the reader
//! range-fetches one at a time, while a small resident FST (built by
//! [`crate::terms_build`]) routes only over each block's *last* term. This module
//! is the single source of truth for the on-disk block bytes, shared by the
//! builder and the reader ([`crate::terms`]). It is deliberately pure — no I/O,
//! no `fst`, no async — so the format is unit-testable in isolation and a future
//! Go port is straightforward.
//!
//! A block is a back-to-back sequence of entries, scanned to the block's end
//! (the block's byte length comes from the router FST output):
//!
//! ```text
//! [ shared:     uvarint ]  bytes shared with the previous term in the block (0 for the first)
//! [ suffix_len  uvarint ]  length of the suffix bytes
//! [ suffix      bytes   ]  term = prev_term[..shared] + suffix
//! [ head_off_d  uvarint ]  head_off delta from the previous entry (first entry: absolute)
//! [ head_size   uvarint ]  head posting length in bytes
//! ```
//!
//! All comparisons and shared-prefix lengths are over UTF-8 **bytes**, matching
//! the `BTreeMap<String>` / FST key order the builder drains. `head_off` is the
//! byte offset of the term's posting block within the (unchanged) postings
//! region; it increases monotonically in term order, so its in-block delta is
//! non-negative.

use std::cmp::Ordering;

/// Default dictionary block byte cap (~one cheap ranged GET). A block always holds
/// at least one entry, so a single oversized term may exceed this. Build-side only
/// (the reader takes the block size from the router output / header).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const DEFAULT_DICT_BLOCK_CAP: usize = 4096;

/// Bits of a packed location `u64` reserved for the byte length; the rest hold the
/// byte offset. Shared by the v1 FST output `(head_off << 24) | head_size` and the
/// v2 router FST output `(block_off << 24) | block_len`.
pub(crate) const SIZE_BITS: u32 = 24;
/// Low-bit mask selecting the length out of a packed location.
pub(crate) const SIZE_MASK: u64 = (1 << SIZE_BITS) - 1;

/// Packs `(off, size)` into one `u64` as `(off << SIZE_BITS) | size`. `size` must
/// fit in [`SIZE_BITS`] bits and `off` in the remaining high bits (checked by the
/// caller, which surfaces a descriptive build error). Build-side only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pack_loc(off: u64, size: u64) -> u64 {
    (off << SIZE_BITS) | size
}

/// Inverse of [`pack_loc`]: `(off, size)`.
pub(crate) fn unpack_loc(packed: u64) -> (u64, usize) {
    (packed >> SIZE_BITS, (packed & SIZE_MASK) as usize)
}

/// Appends `v` to `buf` as an unsigned LEB128 varint. Build-side only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Reads an unsigned LEB128 varint at `buf[*pos..]`, advancing `*pos`. Returns
/// `None` on truncation or a varint longer than 64 bits (treated as corrupt).
pub(crate) fn read_uvarint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if shift >= 64 {
            return None;
        }
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

/// Encoded length of `v` as an unsigned varint, without allocating. Build-side only.
#[cfg(not(target_arch = "wasm32"))]
fn uvarint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Length of the byte-wise common prefix of `a` and `b`. Build-side only.
#[cfg(not(target_arch = "wasm32"))]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// One emitted dictionary block: its bytes, the byte offset within the dict region
/// where it lives, and its last term (the router FST's key for the block).
/// Build-side only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Block {
    /// Front-coded block bytes.
    pub bytes: Vec<u8>,
    /// Byte offset of the block within the dict-blocks region.
    pub off: u64,
    /// The block's last term — the router FST routes to this block for any term
    /// `<= last_term` that no earlier block already covers.
    pub last_term: Vec<u8>,
}

/// Accumulates `(term, head_off, head_size)` entries pushed in sorted
/// (byte-lexicographic) order and front-codes them into byte-capped [`Block`]s.
/// The builder pushes every term, then drains [`finish`](Self::finish) to write
/// the dict region and build the router FST over each block's `last_term`.
/// Build-side only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct BlockWriter {
    cap: usize,
    cur: Vec<u8>,
    last_term: Vec<u8>,
    prev_term: Vec<u8>,
    prev_head_off: u64,
    count: usize,
    blocks: Vec<Block>,
    dict_len: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl BlockWriter {
    /// Creates a writer flushing a block once it would exceed `cap` bytes (`0`
    /// selects [`DEFAULT_DICT_BLOCK_CAP`]).
    pub(crate) fn new(cap: usize) -> Self {
        BlockWriter {
            cap: if cap == 0 {
                DEFAULT_DICT_BLOCK_CAP
            } else {
                cap
            },
            cur: Vec::new(),
            last_term: Vec::new(),
            prev_term: Vec::new(),
            prev_head_off: 0,
            count: 0,
            blocks: Vec::new(),
            dict_len: 0,
        }
    }

    /// Records `term -> (head_off, head_size)`. Terms must arrive in ascending
    /// byte order (the builder drains a `BTreeMap`). `head_off` must be
    /// non-decreasing across calls (postings are appended in term order).
    pub(crate) fn push(&mut self, term: &[u8], head_off: u64, head_size: u64) {
        // If front-coding this entry against the current block's previous term
        // would push the block past the cap, seal the block first; the entry then
        // starts a fresh block as its (full, unshared) first term.
        if self.count > 0 {
            let shared = common_prefix(&self.prev_term, term);
            let delta = head_off - self.prev_head_off;
            let entry_len = uvarint_len(shared as u64)
                + uvarint_len((term.len() - shared) as u64)
                + (term.len() - shared)
                + uvarint_len(delta)
                + uvarint_len(head_size);
            if self.cur.len() + entry_len > self.cap {
                self.flush();
            }
        }

        let first = self.count == 0;
        let shared = if first {
            0
        } else {
            common_prefix(&self.prev_term, term)
        };
        let head_off_d = if first {
            head_off
        } else {
            head_off - self.prev_head_off
        };
        write_uvarint(&mut self.cur, shared as u64);
        write_uvarint(&mut self.cur, (term.len() - shared) as u64);
        self.cur.extend_from_slice(&term[shared..]);
        write_uvarint(&mut self.cur, head_off_d);
        write_uvarint(&mut self.cur, head_size);

        self.last_term.clear();
        self.last_term.extend_from_slice(term);
        self.prev_term.clear();
        self.prev_term.extend_from_slice(term);
        self.prev_head_off = head_off;
        self.count += 1;
    }

    /// Seals the open block (if any), recording its offset and last term.
    fn flush(&mut self) {
        if self.count == 0 {
            return;
        }
        let bytes = std::mem::take(&mut self.cur);
        let off = self.dict_len;
        self.dict_len += bytes.len() as u64;
        self.blocks.push(Block {
            bytes,
            off,
            last_term: std::mem::take(&mut self.last_term),
        });
        self.count = 0;
        self.prev_term.clear();
        self.prev_head_off = 0;
    }

    /// Flushes the open block and returns every block in term order.
    pub(crate) fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.blocks
    }
}

/// Iterates a front-coded block, yielding `(term, head_off, head_size)` in order.
/// Stops (yields `None`) at the block's end or on the first malformed entry.
pub(crate) struct BlockIter<'a> {
    block: &'a [u8],
    pos: usize,
    prev: Vec<u8>,
    head_off: u64,
    first: bool,
}

impl Iterator for BlockIter<'_> {
    type Item = (Vec<u8>, u64, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.block.len() {
            return None;
        }
        let shared = read_uvarint(self.block, &mut self.pos)? as usize;
        let suffix_len = read_uvarint(self.block, &mut self.pos)? as usize;
        // `pos + suffix_len` can overflow on a corrupted varint; `checked_add` plus
        // `get` stop cleanly instead of panicking or slicing out of bounds.
        let end = self.pos.checked_add(suffix_len)?;
        let suffix = self.block.get(self.pos..end)?;
        self.pos = end;
        let head_off_d = read_uvarint(self.block, &mut self.pos)?;
        let head_size = read_uvarint(self.block, &mut self.pos)? as usize;

        // `shared` is an untrusted varint, but only `shared.min(prev.len())` bytes
        // are copied from `prev`. Cap the capacity hint there — a raw `shared` (up
        // to u64) would request a multi-terabyte allocation and OOM the reader.
        let shared = shared.min(self.prev.len());
        let mut term = Vec::with_capacity(shared + suffix_len);
        term.extend_from_slice(&self.prev[..shared]);
        term.extend_from_slice(suffix);
        self.head_off = if self.first {
            head_off_d
        } else {
            self.head_off.saturating_add(head_off_d)
        };
        self.first = false;
        self.prev = term.clone();
        Some((term, self.head_off, head_size))
    }
}

/// Iterates the entries of a front-coded `block` (see [`BlockIter`]).
pub(crate) fn iter_block(block: &[u8]) -> BlockIter<'_> {
    BlockIter {
        block,
        pos: 0,
        prev: Vec::new(),
        head_off: 0,
        first: true,
    }
}

/// Linear-scans a front-coded `block` for `term`, returning its
/// `(head_off, head_size)` or `None` if absent. Stops early once a term sorts past
/// `term` (entries are ascending).
pub(crate) fn scan_block(block: &[u8], term: &[u8]) -> Option<(u64, usize)> {
    for (t, head_off, head_size) in iter_block(block) {
        match t.as_slice().cmp(term) {
            Ordering::Equal => return Some((head_off, head_size)),
            Ordering::Greater => return None,
            Ordering::Less => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips every term through the writer and `scan_block`, asserting the
    /// router-key block selection (last term per block) and per-entry payloads.
    fn roundtrip(terms: &[(&str, u64, u64)], cap: usize) {
        let mut w = BlockWriter::new(cap);
        for &(t, off, size) in terms {
            w.push(t.as_bytes(), off, size);
        }
        let blocks = w.finish();

        // Blocks partition the terms contiguously and last_terms are sorted.
        let mut flat: Vec<(Vec<u8>, u64, usize)> = Vec::new();
        let mut prev_last: Option<Vec<u8>> = None;
        let mut running_off = 0u64;
        for b in &blocks {
            assert_eq!(b.off, running_off, "block offsets must be contiguous");
            running_off += b.bytes.len() as u64;
            if let Some(p) = &prev_last {
                assert!(p.as_slice() < b.last_term.as_slice(), "last_terms sorted");
            }
            let entries: Vec<_> = iter_block(&b.bytes).collect();
            assert_eq!(
                entries.last().unwrap().0,
                b.last_term,
                "router key is the block's last term"
            );
            flat.extend(entries);
            prev_last = Some(b.last_term.clone());
        }

        assert_eq!(flat.len(), terms.len(), "every term present exactly once");
        for (i, &(t, off, size)) in terms.iter().enumerate() {
            assert_eq!(flat[i].0, t.as_bytes(), "term order preserved");
            assert_eq!(
                (flat[i].1, flat[i].2),
                (off, size as usize),
                "payload preserved"
            );
        }
        // scan_block on the owning block finds each term; a different block does not.
        for &(t, off, size) in terms {
            let owner = blocks
                .iter()
                .find(|b| t.as_bytes() <= b.last_term.as_slice())
                .expect("a block whose last term >= the query");
            assert_eq!(
                scan_block(&owner.bytes, t.as_bytes()),
                Some((off, size as usize))
            );
        }
    }

    #[test]
    fn uvarint_roundtrip() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            1 << 20,
            1 << 40,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write_uvarint(&mut buf, v);
            assert_eq!(buf.len(), uvarint_len(v));
            let mut pos = 0;
            assert_eq!(read_uvarint(&buf, &mut pos), Some(v));
            assert_eq!(pos, buf.len());
        }
        // Truncated input yields None rather than panicking.
        let mut pos = 0;
        assert_eq!(read_uvarint(&[0x80], &mut pos), None);
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let (off, size) = (1_234_567u64, 9_999usize);
        assert_eq!(unpack_loc(pack_loc(off, size as u64)), (off, size));
    }

    #[test]
    fn single_block_shared_prefixes() {
        // Long shared prefixes (scholarly vocab) front-code to small suffixes.
        roundtrip(
            &[
                ("reinforce", 0, 10),
                ("reinforced", 14, 12),
                ("reinforcement", 30, 8),
                ("reinforcing", 42, 9),
            ],
            4096,
        );
    }

    #[test]
    fn many_blocks_tiny_cap() {
        // A tiny cap forces one entry per block, exercising block boundaries and
        // first-entry (absolute head_off, shared=0) encoding repeatedly.
        let terms: Vec<(String, u64, u64)> = (0..50)
            .map(|i| (format!("term{i:04}"), (i as u64) * 100, 7))
            .collect();
        let refs: Vec<(&str, u64, u64)> =
            terms.iter().map(|(t, o, s)| (t.as_str(), *o, *s)).collect();
        roundtrip(&refs, 8);
        // With an 8-byte cap, "term0000" alone exceeds it, so every term is its own block.
        let mut w = BlockWriter::new(8);
        for (t, o, s) in &terms {
            w.push(t.as_bytes(), *o, *s);
        }
        assert_eq!(w.finish().len(), 50);
    }

    #[test]
    fn multibyte_utf8_terms() {
        roundtrip(
            &[
                ("café", 0, 4),
                ("cafés", 8, 4),
                ("naïve", 16, 5),
                ("naïveté", 25, 6),
            ],
            4096,
        );
    }

    #[test]
    fn empty_and_single() {
        assert!(BlockWriter::new(4096).finish().is_empty());
        roundtrip(&[("solo", 7, 3)], 4096);
    }

    #[test]
    fn scan_absent_term() {
        let mut w = BlockWriter::new(4096);
        for (t, o, s) in [("alpha", 0u64, 1u64), ("gamma", 2, 1), ("omega", 4, 1)] {
            w.push(t.as_bytes(), o, s);
        }
        let blocks = w.finish();
        // "beta" sorts between alpha and gamma; absent -> None (early stop on Greater).
        assert_eq!(scan_block(&blocks[0].bytes, b"beta"), None);
        assert_eq!(scan_block(&blocks[0].bytes, b"zzz"), None);
    }

    #[test]
    fn front_coding_actually_shrinks() {
        // A run of shared-prefix terms encodes smaller than the raw term bytes.
        let terms = [
            "internationalization",
            "internationalize",
            "internationalized",
        ];
        let mut w = BlockWriter::new(1 << 20);
        for (i, t) in terms.iter().enumerate() {
            w.push(t.as_bytes(), i as u64, 1);
        }
        let blocks = w.finish();
        assert_eq!(blocks.len(), 1);
        let raw: usize = terms.iter().map(|t| t.len()).sum();
        assert!(
            blocks[0].bytes.len() < raw,
            "front-coded block ({}) should beat raw term bytes ({raw})",
            blocks[0].bytes.len()
        );
    }
}
