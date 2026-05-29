//! The RRS range-fetchable index reader.
//!
//! [`Index::open`] performs the one-time boot (header + sparse index); each
//! query then issues a few small ranged reads. The layout is the frozen `RRS`
//! contract documented in `FORMAT.md`.
//!
//! Reads are issued in concurrent waves so a query costs a near-constant number
//! of round-trip "waves" regardless of how many n-grams it contains: one wave
//! for the dict blocks, one for the head postings, and at most one more for the
//! tail postings. Within a wave every independent ranged read is constructed up
//! front and the batch is awaited together via [`futures::future::join_all`].

use crate::fetch::{FetchError, RangeFetch};
use crate::ngram::ngram_keys;
use futures::future::join_all;
use roaring::RoaringBitmap;
use std::error::Error;
use std::fmt;

/// `RRS` magic.
const MAGIC: &[u8; 4] = b"RRSI";
/// Header size in bytes.
const HEADER_SIZE: usize = 16;
/// Dictionary entry size in bytes: key(8) + headOffset(8) + headSize(4) + tailSize(4).
const DICT_ENTRY: usize = 24;
/// Doc-ID boundary between head (first roaring container) and tail.
const HEAD_BOUNDARY: u64 = 65536;

/// An error from opening or querying an index.
#[derive(Debug)]
pub enum IndexError {
    /// A ranged read failed.
    Fetch(FetchError),
    /// The header magic was not `RRS`.
    BadMagic([u8; 4]),
    /// The format version was unsupported.
    BadVersion(u16),
    /// A roaring bitmap failed to deserialize.
    Roaring(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Fetch(e) => write!(f, "fetch: {e}"),
            IndexError::BadMagic(m) => write!(f, "bad magic {m:?}, expected RRSI"),
            IndexError::BadVersion(v) => write!(f, "unsupported version {v}"),
            IndexError::Roaring(e) => write!(f, "roaring deserialize: {e}"),
        }
    }
}

impl Error for IndexError {}

impl From<FetchError> for IndexError {
    fn from(e: FetchError) -> Self {
        IndexError::Fetch(e)
    }
}

/// A parsed dictionary entry locating a posting's head and tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DictRec {
    key: u64,
    head_offset: u64,
    head_size: u32,
    tail_size: u32,
}

/// The byte range of a single dictionary block, derived purely from the
/// in-memory sparse index with no fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DictBlock {
    /// Absolute file offset of the block's first byte.
    byte_off: u64,
    /// Number of dictionary entries in the block.
    entries: usize,
}

/// Reads a little-endian `u16` at `buf[off..]`.
pub(crate) fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

/// Reads a little-endian `u32` at `buf[off..]`.
pub(crate) fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Reads a little-endian `u64` at `buf[off..]`.
pub(crate) fn read_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// A range-fetchable `RRS` index. Holds only the header fields and the sparse
/// index in memory; everything else is read on demand via `F`.
pub struct Index<F: RangeFetch> {
    fetch: F,
    /// N-gram window size the index was built with.
    pub gram_size: u16,
    /// Number of dictionary entries.
    ngrams: u32,
    /// Sparse-index stride.
    stride: u32,
    /// First byte of the dictionary block.
    dict_start: u64,
    /// In-memory sparse index: `sparse_keys[i] == dict[i*stride].key`.
    sparse_keys: Vec<u64>,
}

impl<F: RangeFetch> Index<F> {
    /// Boots the index: reads the 16-byte header and the sparse index, keeping
    /// the sparse keys in memory. One ranged read for the header plus one for
    /// the sparse block.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let gram_size = read_u16(&header, 6);
        let ngrams = read_u32(&header, 8);
        let stride = read_u32(&header, 12);

        let sparse_count = sparse_count(ngrams, stride);
        let sparse_bytes = fetch.read(HEADER_SIZE as u64, sparse_count * 8).await?;
        let mut sparse_keys = Vec::with_capacity(sparse_count);
        for i in 0..sparse_count {
            sparse_keys.push(read_u64(&sparse_bytes, i * 8));
        }

        let dict_start = HEADER_SIZE as u64 + (sparse_count as u64) * 8;
        Ok(Index {
            fetch,
            gram_size,
            ngrams,
            stride,
            dict_start,
            sparse_keys,
        })
    }

    /// Number of n-grams in the dictionary.
    pub fn ngram_count(&self) -> u32 {
        self.ngrams
    }

    /// Computes the dictionary block that would contain `key`, purely from the
    /// in-memory sparse index (no fetch). Returns `None` when the dictionary is
    /// empty or `key` precedes the whole dictionary, both of which mean the key
    /// is absent.
    fn dict_block_for(&self, key: u64) -> Option<DictBlock> {
        if self.ngrams == 0 {
            return None;
        }
        // Largest sparse index b with sparse_keys[b] <= key.
        let b = match self.sparse_keys.binary_search(&key) {
            Ok(i) => i,
            Err(0) => return None, // key precedes the whole dictionary
            Err(i) => i - 1,
        };

        let block_start = (b as u64) * (self.stride as u64);
        let remaining = self.ngrams as u64 - block_start;
        let entries = (self.stride as u64).min(remaining) as usize;
        let byte_off = self.dict_start + block_start * DICT_ENTRY as u64;
        Some(DictBlock { byte_off, entries })
    }

    /// Binary-searches an already-fetched dictionary `block` of `entries` entries
    /// for `key`, returning its [`DictRec`] or `None` if absent.
    fn parse_block(block: &[u8], entries: usize, key: u64) -> Option<DictRec> {
        let mut lo = 0usize;
        let mut hi = entries; // exclusive
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_key = read_u64(block, mid * DICT_ENTRY);
            match mid_key.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let base = mid * DICT_ENTRY;
                    return Some(DictRec {
                        key,
                        head_offset: read_u64(block, base + 8),
                        head_size: read_u32(block, base + 16),
                        tail_size: read_u32(block, base + 20),
                    });
                }
            }
        }
        None
    }

    /// Resolves `key` to its dictionary entry with at most one ranged dict-block
    /// read, or `Ok(None)` if the key is absent.
    ///
    /// Performs an in-memory binary search over the sparse keys to pick the
    /// block, reads `min(stride, remaining)` dictionary entries, then
    /// binary-searches within the block.
    async fn lookup(&self, key: u64) -> Result<Option<DictRec>, IndexError> {
        match self.dict_block_for(key) {
            None => Ok(None),
            Some(block) => {
                let bytes = self
                    .fetch
                    .read(block.byte_off, block.entries * DICT_ENTRY)
                    .await?;
                Ok(Self::parse_block(&bytes, block.entries, key))
            }
        }
    }

    /// Returns the head posting (docs `[0, 65536)`) for `key`, or `Ok(None)` if
    /// the key is absent. One ranged dict-block read plus one ranged posting
    /// read.
    pub async fn head(&self, key: u64) -> Result<Option<RoaringBitmap>, IndexError> {
        match self.lookup(key).await? {
            None => Ok(None),
            Some(rec) => {
                let bytes = self
                    .fetch
                    .read(rec.head_offset, rec.head_size as usize)
                    .await?;
                Ok(Some(deserialize(&bytes)?))
            }
        }
    }

    /// Returns the tail posting (docs `[65536, ∞)`) for `key`, or `Ok(None)` if
    /// the key is absent. One ranged dict-block read plus one ranged posting
    /// read.
    pub async fn tail(&self, key: u64) -> Result<Option<RoaringBitmap>, IndexError> {
        match self.lookup(key).await? {
            None => Ok(None),
            Some(rec) => {
                let off = rec.head_offset + rec.head_size as u64;
                let bytes = self.fetch.read(off, rec.tail_size as usize).await?;
                Ok(Some(deserialize(&bytes)?))
            }
        }
    }

    /// Reads the posting bytes for every key in `recs` concurrently and
    /// deserializes each. `range_of` maps a [`DictRec`] to the `(offset, len)`
    /// of the posting half to read (head or tail). All reads are issued before
    /// any is awaited so they proceed as a single concurrent wave.
    async fn fetch_postings(
        &self,
        recs: &[DictRec],
        range_of: impl Fn(&DictRec) -> (u64, usize),
    ) -> Result<Vec<RoaringBitmap>, IndexError> {
        let reads = recs.iter().map(|rec| {
            let (off, len) = range_of(rec);
            self.fetch.read(off, len)
        });
        let results = join_all(reads).await;
        let mut bitmaps = Vec::with_capacity(results.len());
        for bytes in results {
            bitmaps.push(deserialize(&bytes?)?);
        }
        Ok(bitmaps)
    }

    /// Intersects a set of postings smallest-cardinality-first and returns the
    /// accumulated bitmap, or `None` when there are no postings.
    fn intersect(mut bitmaps: Vec<RoaringBitmap>) -> Option<RoaringBitmap> {
        if bitmaps.is_empty() {
            return None;
        }
        bitmaps.sort_by_key(|b| b.len());
        let mut iter = bitmaps.into_iter();
        let mut acc = iter.next().unwrap();
        for bm in iter {
            acc &= bm;
            if acc.is_empty() {
                break;
            }
        }
        Some(acc)
    }

    /// Resolves a query to its top doc IDs.
    ///
    /// Derives the query's n-gram keys, ANDs all of their head postings, and
    /// returns the first `limit` doc IDs in ascending order — ascending doc ID
    /// equals descending popularity rank, so this is the top-`limit` set. If the
    /// head intersection yields fewer than `limit` results, the tail postings
    /// are fetched and ANDed to continue. Returns an empty vector when the query
    /// has no keys or any key is absent.
    ///
    /// The reads are issued in concurrent waves (dict blocks, then heads, then —
    /// only if needed — tails) so a query costs a near-constant number of
    /// round-trip waves regardless of trigram count.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, IndexError> {
        let keys = ngram_keys(query, self.gram_size as usize);
        if keys.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        // WAVE 1: resolve every key's dict block concurrently. Each block's byte
        // range is computed in memory with no fetch, so all reads start at once.
        let blocks: Vec<DictBlock> = match keys.iter().map(|&k| self.dict_block_for(k)).collect() {
            Some(blocks) => blocks,
            None => return Ok(Vec::new()), // a key precedes the dictionary -> absent
        };
        let block_reads = blocks
            .iter()
            .map(|blk| self.fetch.read(blk.byte_off, blk.entries * DICT_ENTRY));
        let block_results = join_all(block_reads).await;

        let mut recs = Vec::with_capacity(keys.len());
        for ((bytes, blk), &key) in block_results.into_iter().zip(&blocks).zip(&keys) {
            match Self::parse_block(&bytes?, blk.entries, key) {
                None => return Ok(Vec::new()), // absent key -> empty result
                Some(rec) => recs.push(rec),
            }
        }

        // WAVE 2: fetch every head posting concurrently, deserialize, intersect.
        let heads = self
            .fetch_postings(&recs, |rec| (rec.head_offset, rec.head_size as usize))
            .await?;
        let head_and = Self::intersect(heads).unwrap_or_default();

        let mut out: Vec<u32> = head_and.iter().take(limit).collect();
        if out.len() >= limit {
            return Ok(out);
        }

        // WAVE 3 (only if the head AND under-fills the limit): fetch every tail
        // posting concurrently, intersect, and append docs >= 65536.
        let tails = self
            .fetch_postings(&recs, |rec| {
                (
                    rec.head_offset + rec.head_size as u64,
                    rec.tail_size as usize,
                )
            })
            .await?;
        if let Some(tail_and) = Self::intersect(tails) {
            for doc in tail_and.iter() {
                if out.len() >= limit {
                    break;
                }
                debug_assert!(doc >= HEAD_BOUNDARY as u32);
                out.push(doc);
            }
        }
        Ok(out)
    }

    /// Opens a stateful pagination cursor for `query`. Does the up-front work
    /// once (one dict-block wave + one head-posting wave, intersected into the
    /// head result set); [`Cursor::next`] then pages through that in-memory set
    /// with no further fetches until the head is exhausted, at which point a
    /// single tail wave is fetched lazily. Requires `F: Clone` so the cursor can
    /// own a fetcher for the lazy tail reads.
    pub async fn search_cursor(
        &self,
        query: &str,
        max_missing: usize,
    ) -> Result<Cursor<F>, IndexError>
    where
        F: Clone,
    {
        self.search_cursor_filtered(query, max_missing, None).await
    }

    /// Like [`Index::search_cursor`] but ANDs an optional facet `filter` into the
    /// result (within-field OR, across-field AND). The filter's head postings are
    /// fetched up front and intersected with the head result; its tail postings
    /// are fetched lazily by the cursor only when pagination crosses into the
    /// tail. A `None` or empty filter is the unfiltered case.
    pub async fn search_cursor_filtered(
        &self,
        query: &str,
        max_missing: usize,
        filter: Option<ResolvedFilter<F>>,
    ) -> Result<Cursor<F>, IndexError>
    where
        F: Clone,
    {
        let keys = ngram_keys(query, self.gram_size as usize);
        let min_match = keys.len().saturating_sub(max_missing).max(1);
        // Resolve only the n-grams present in the dictionary; absent ones simply
        // contribute nothing, which is what tolerating missing n-grams means.
        let present: Vec<(u64, DictBlock)> = keys
            .iter()
            .filter_map(|&k| self.dict_block_for(k).map(|blk| (k, blk)))
            .collect();
        let block_reads = present
            .iter()
            .map(|(_, blk)| self.fetch.read(blk.byte_off, blk.entries * DICT_ENTRY));
        let block_results = join_all(block_reads).await;
        let mut recs = Vec::with_capacity(present.len());
        for (bytes, (key, blk)) in block_results.into_iter().zip(&present) {
            if let Some(rec) = Self::parse_block(&bytes?, blk.entries, *key) {
                recs.push(rec);
            }
        }
        if recs.len() < min_match {
            return Ok(Cursor::empty(self.fetch.clone())); // threshold unreachable
        }
        let heads = self
            .fetch_postings(&recs, |rec| (rec.head_offset, rec.head_size as usize))
            .await?;
        let mut head_result = threshold(heads, min_match).unwrap_or_default();

        // Drop a no-constraint filter so the cursor never does facet tail reads.
        let filter = filter.filter(|f| !f.is_empty());
        if let Some(f) = &filter {
            head_result &= f.head_bitmap().await?;
        }
        let results: Vec<u32> = head_result.iter().collect();
        Ok(Cursor {
            fetch: self.fetch.clone(),
            recs,
            min_match,
            filter,
            head_result,
            results,
            pos: 0,
            tail_done: false,
        })
    }
}

/// One facet category's posting locations within the facet (`RRSF`) file.
#[derive(Clone, Copy)]
pub struct CatRange {
    /// Absolute offset of the head posting (docs `[0, 65536)`).
    pub head_off: u64,
    /// Head posting length in bytes.
    pub head_size: u32,
    /// Absolute offset of the tail posting (`head_off + head_size`).
    pub tail_off: u64,
    /// Tail posting length in bytes.
    pub tail_size: u32,
}

/// A resolved facet filter: per selected field, the chosen categories' posting
/// ranges. Categories within a field are ORed and fields are ANDed
/// (`result = textMatch AND over fields( OR over that field's categories )`),
/// mirroring roaringsearch's `BitmapFilter`. Carries its own fetcher because the
/// facet file is a separate resource from the index.
pub struct ResolvedFilter<F: RangeFetch> {
    fetch: F,
    fields: Vec<Vec<CatRange>>,
}

impl<F: RangeFetch> ResolvedFilter<F> {
    /// Builds a filter from a fetcher and per-field category ranges. An empty
    /// `fields` means "no constraint".
    pub fn new(fetch: F, fields: Vec<Vec<CatRange>>) -> Self {
        Self { fetch, fields }
    }

    /// Whether the filter imposes no constraint.
    fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The combined head-side filter bitmap.
    async fn head_bitmap(&self) -> Result<RoaringBitmap, IndexError> {
        self.combine(|c| (c.head_off, c.head_size as usize)).await
    }

    /// The combined tail-side filter bitmap.
    async fn tail_bitmap(&self) -> Result<RoaringBitmap, IndexError> {
        self.combine(|c| (c.tail_off, c.tail_size as usize)).await
    }

    /// Fetches every selected category posting in one concurrent wave, ORs the
    /// postings within each field, then ANDs the fields smallest-first.
    async fn combine(
        &self,
        range_of: impl Fn(&CatRange) -> (u64, usize),
    ) -> Result<RoaringBitmap, IndexError> {
        let mut flat: Vec<(usize, (u64, usize))> = Vec::new();
        for (fi, cats) in self.fields.iter().enumerate() {
            for c in cats {
                flat.push((fi, range_of(c)));
            }
        }
        let reads = flat.iter().map(|(_, (off, len))| self.fetch.read(*off, *len));
        let results = join_all(reads).await;

        let mut per_field: Vec<RoaringBitmap> =
            (0..self.fields.len()).map(|_| RoaringBitmap::new()).collect();
        for ((fi, _), bytes) in flat.iter().zip(results) {
            per_field[*fi] |= deserialize(&bytes?)?;
        }
        per_field.sort_by_key(|b| b.len());
        let mut iter = per_field.into_iter();
        let mut acc = iter.next().unwrap_or_default();
        for b in iter {
            acc &= b;
            if acc.is_empty() {
                break;
            }
        }
        Ok(acc)
    }
}

/// A stateful pagination cursor over a query's intersected result set.
///
/// Built by [`Index::search_cursor`]. It holds the head intersection in memory
/// (ascending doc IDs == descending popularity). [`Cursor::next`] returns the
/// next page with no fetches until the head is exhausted, then lazily fetches
/// and appends the tail intersection (docs >= 65536, which sort after the head).
pub struct Cursor<F: RangeFetch> {
    fetch: F,
    recs: Vec<DictRec>,
    /// Minimum n-grams a doc must match (== recs.len() for strict AND, fewer for fuzzy).
    min_match: usize,
    /// Optional facet filter ANDed into both head and tail results. Its head was
    /// already applied at construction; its tail is applied lazily in `ensure`.
    filter: Option<ResolvedFilter<F>>,
    /// The head intersection (post-filter) as a bitmap, kept for facet counts.
    head_result: RoaringBitmap,
    results: Vec<u32>,
    pos: usize,
    tail_done: bool,
}

impl<F: RangeFetch> Cursor<F> {
    /// An exhausted cursor with no results (empty or absent query).
    fn empty(fetch: F) -> Self {
        Cursor {
            fetch,
            recs: Vec::new(),
            min_match: 1,
            filter: None,
            head_result: RoaringBitmap::new(),
            results: Vec::new(),
            pos: 0,
            tail_done: true,
        }
    }

    /// The query's head result as a bitmap (post-filter). Used to compute
    /// search-filtered facet counts without re-running the query.
    pub fn head_bitmap(&self) -> &RoaringBitmap {
        &self.head_result
    }

    /// Ensures at least `need` doc IDs are materialized, fetching the tail
    /// intersection once (a single concurrent wave) if the head doesn't reach
    /// `need`. Tail doc IDs are all >= 65536 > every head doc ID, so appending
    /// preserves global ascending (popularity) order.
    async fn ensure(&mut self, need: usize) -> Result<(), IndexError> {
        if self.tail_done || self.recs.is_empty() || need <= self.results.len() {
            return Ok(());
        }
        let ranges: Vec<(u64, usize)> = self
            .recs
            .iter()
            .map(|rec| {
                (
                    rec.head_offset + rec.head_size as u64,
                    rec.tail_size as usize,
                )
            })
            .collect();
        let reads = ranges.iter().map(|&(off, len)| self.fetch.read(off, len));
        let results = join_all(reads).await;
        let mut tails = Vec::with_capacity(results.len());
        for bytes in results {
            tails.push(deserialize(&bytes?)?);
        }
        if let Some(mut tail_and) = threshold(tails, self.min_match) {
            if let Some(f) = &self.filter {
                tail_and &= f.tail_bitmap().await?;
            }
            self.results.extend(tail_and.iter());
        }
        self.tail_done = true;
        Ok(())
    }

    /// Returns the next `n` doc IDs, advancing an internal position. Pages
    /// within the materialized set cost no fetches.
    pub async fn next(&mut self, n: usize) -> Result<Vec<u32>, IndexError> {
        self.ensure(self.pos + n).await?;
        let end = (self.pos + n).min(self.results.len());
        let out = self.results[self.pos..end].to_vec();
        self.pos = end;
        Ok(out)
    }

    /// Random-access page: up to `limit` doc IDs starting at `offset`. Going
    /// backward (or to any already-materialized window) never fetches; going
    /// past the head fetches the tail once, after which all pages are free.
    pub async fn page(&mut self, offset: usize, limit: usize) -> Result<Vec<u32>, IndexError> {
        self.ensure(offset + limit).await?;
        let start = offset.min(self.results.len());
        let end = (offset + limit).min(self.results.len());
        Ok(self.results[start..end].to_vec())
    }

    /// Number of doc IDs materialized so far (head, plus tail once fetched).
    pub fn loaded(&self) -> usize {
        self.results.len()
    }
}

/// Number of sparse-index entries: `ceil(ngrams / stride)`.
fn sparse_count(ngrams: u32, stride: u32) -> usize {
    if ngrams == 0 || stride == 0 {
        return 0;
    }
    ngrams.div_ceil(stride) as usize
}

/// Returns the docs present in at least `min_match` of the postings. With
/// `min_match == bitmaps.len()` this is a strict AND; smaller values are the
/// "fuzzy" search that tolerates missing n-grams. Returns `None` when nothing
/// qualifies (including the impossible `min_match > len`).
fn threshold(bitmaps: Vec<RoaringBitmap>, min_match: usize) -> Option<RoaringBitmap> {
    let n = bitmaps.len();
    if n == 0 || min_match == 0 || min_match > n {
        return None;
    }
    if min_match == 1 {
        let mut acc = RoaringBitmap::new();
        for b in &bitmaps {
            acc |= b;
        }
        return (!acc.is_empty()).then_some(acc);
    }
    if min_match == n {
        let mut bms = bitmaps;
        bms.sort_by_key(|b| b.len());
        let mut iter = bms.into_iter();
        let mut acc = iter.next().unwrap();
        for b in iter {
            acc &= b;
            if acc.is_empty() {
                break;
            }
        }
        return (!acc.is_empty()).then_some(acc);
    }
    // Cascading counters: c[k] = docs seen in >= k postings so far. Processing
    // high-to-low keeps each posting from being counted twice within a step.
    let t = min_match;
    let mut c: Vec<RoaringBitmap> = (0..=t).map(|_| RoaringBitmap::new()).collect();
    for b in &bitmaps {
        for k in (1..=t).rev() {
            if k == 1 {
                c[1] |= b;
            } else {
                let mut inc = c[k - 1].clone();
                inc &= b;
                c[k] |= inc;
            }
        }
    }
    let res = std::mem::take(&mut c[t]);
    (!res.is_empty()).then_some(res)
}

/// Deserializes a portable RoaringBitmap.
fn deserialize(bytes: &[u8]) -> Result<RoaringBitmap, IndexError> {
    RoaringBitmap::deserialize_from(bytes).map_err(|e| IndexError::Roaring(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_count_is_ceil() {
        assert_eq!(sparse_count(0, 512), 0);
        assert_eq!(sparse_count(1, 512), 1);
        assert_eq!(sparse_count(512, 512), 1);
        assert_eq!(sparse_count(513, 512), 2);
        assert_eq!(sparse_count(5, 2), 3);
    }
}
