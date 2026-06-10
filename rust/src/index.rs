//! The RRS range-fetchable index reader.
//!
//! [`Index::open`] performs the one-time boot (header + sparse index); each
//! query then issues a few small ranged reads. The layout is the frozen `RRS`
//! contract documented in `FORMAT.md`.
//!
//! Reads are issued in concurrent waves so a query costs a near-constant number
//! of round-trip "waves" regardless of how many n-grams it contains: one wave
//! for the dict blocks, one for each term's eager prefix (the leading container
//! bucket), and then container-level ranged reads to page the higher buckets as
//! a deep page needs them. Within a wave every independent ranged read is
//! constructed up front and awaited together via [`futures::future::join_all`].

use crate::fetch::{FetchError, RangeFetch};
use crate::ngram::ngram_keys;
use futures::future::join_all;
use roaring::RoaringBitmap;
use std::error::Error;
use std::fmt;

/// `RRS` magic.
const MAGIC: &[u8; 4] = b"RRSI";
/// Header size in bytes (v3): magic[4] + version[2] + gram[2] + ngrams[4] + stride[4]. The v2
/// `head_boundary[4]` is gone (one posting per term). Kept in sync with `build::HEADER_SIZE`.
const HEADER_SIZE: usize = 16;
/// Dictionary entry size in bytes (v3): key(8) + offset(8) + size(4).
const DICT_ENTRY: usize = 20;
/// The accepted on-disk format version.
const FORMAT_VERSION: u16 = 3;
/// The eager-prefix bucket count: the cursor fetches the first `EAGER_BUCKETS` container buckets
/// (docs `[0, EAGER_BUCKETS·65536)`) of a term's posting up front for the instant first page +
/// facet counts, then `TailScan` pages the buckets at or above it. `1` (bucket 0 = the top 64K
/// ranked docs) matches the `RRSF` facet head boundary, so a facet-filtered query's eager set and
/// its tail scan partition the doc space consistently with the facet head/tail split.
const EAGER_BUCKETS: u16 = 1;
/// The doc-ID at which the eager prefix ends (`EAGER_BUCKETS · 65536`).
const EAGER_DOC_BOUND: u32 = EAGER_BUCKETS as u32 * 65_536;

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
    /// A header or offset field was internally inconsistent — out of bounds or
    /// overflowing — as from a truncated or tampered file.
    Malformed(&'static str),
    /// A query argument was invalid for this index (e.g. a query vector whose
    /// dimensionality does not match the index's). Distinct from [`Malformed`],
    /// which describes a corrupt file rather than a bad caller argument.
    BadQuery(&'static str),
    /// The operation is well-formed but not supported for this index kind (e.g.
    /// facet-filtered search over a term-bodied split). A capability gap, not a
    /// corrupt file ([`Malformed`]) or a bad argument ([`BadQuery`]).
    Unsupported(&'static str),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Fetch(e) => write!(f, "fetch: {e}"),
            IndexError::BadMagic(m) => write!(f, "bad magic {m:?}, expected RRSI"),
            IndexError::BadVersion(v) => write!(f, "unsupported version {v}"),
            IndexError::Roaring(e) => write!(f, "roaring deserialize: {e}"),
            IndexError::Malformed(m) => write!(f, "malformed index: {m}"),
            IndexError::BadQuery(m) => write!(f, "bad query: {m}"),
            IndexError::Unsupported(m) => write!(f, "unsupported operation: {m}"),
        }
    }
}

impl Error for IndexError {}

impl From<FetchError> for IndexError {
    fn from(e: FetchError) -> Self {
        IndexError::Fetch(e)
    }
}

/// A parsed dictionary entry locating a term's posting `[offset, offset+size)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DictRec {
    key: u64,
    offset: u64,
    size: u32,
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
    gram_size: u16,
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
        if version != FORMAT_VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let gram_size = read_u16(&header, 6);
        let ngrams = read_u32(&header, 8);
        let stride = read_u32(&header, 12);

        let (sparse_count, sparse_len) = sparse_layout(ngrams, stride)?;
        let sparse_bytes = fetch.read(HEADER_SIZE as u64, sparse_len).await?;
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

    /// Boots from a **resident** boot region instead of fetching it — the header (16 B) plus
    /// the sparse index, i.e. the bytes `[0, dictStart)`. This is the zero-round-trip open a
    /// boot accelerator (an `RRHC` bundle, or a split set's inlined tier-0 boots) uses: the
    /// caller already holds those bytes, so only the per-query dict/posting reads go through
    /// `fetch`. Equivalent to [`open`](Self::open) but with no boot fetch. Errors if `boot` is
    /// shorter than the header + sparse index it declares.
    pub fn from_boot(boot: &[u8], fetch: F) -> Result<Self, IndexError> {
        if boot.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRS boot region"));
        }
        if &boot[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&boot[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(boot, 4);
        if version != FORMAT_VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let gram_size = read_u16(boot, 6);
        let ngrams = read_u32(boot, 8);
        let stride = read_u32(boot, 12);

        let (sparse_count, sparse_len) = sparse_layout(ngrams, stride)?;
        let dict_start = HEADER_SIZE
            .checked_add(sparse_len)
            .ok_or(IndexError::Malformed("RRS boot region length overflows"))?;
        if boot.len() < dict_start {
            return Err(IndexError::Malformed(
                "RRS boot region missing sparse index",
            ));
        }
        let mut sparse_keys = Vec::with_capacity(sparse_count);
        for i in 0..sparse_count {
            sparse_keys.push(read_u64(boot, HEADER_SIZE + i * 8));
        }
        Ok(Index {
            fetch,
            gram_size,
            ngrams,
            stride,
            dict_start: dict_start as u64,
            sparse_keys,
        })
    }

    /// The byte length of this index's boot region (`[0, dictStart)`) — the header plus the
    /// sparse index. A bundle builder copies exactly these bytes so a reader can
    /// [`from_boot`](Self::from_boot) the index with no fetch.
    pub fn boot_len(&self) -> u64 {
        self.dict_start
    }

    /// N-gram window size the index was built with (e.g. `3` for trigrams).
    pub fn gram_size(&self) -> u16 {
        self.gram_size
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
                        offset: read_u64(block, base + 8),
                        size: read_u32(block, base + 16),
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

    /// Estimated client-side bytes a search for `query` would fetch — the summed
    /// posting sizes of its n-gram keys, resolved with **dictionary reads only**
    /// (KBs, none of the postings themselves). The dictionary records every
    /// posting's byte length, so a caller can price a query *before* fetching and
    /// route an expensive one to a server-side search instead. Returns `0` when
    /// any key is absent (the strict-AND search short-circuits to empty the same
    /// way). The dict blocks read here are the ones a subsequent local search
    /// reads anyway, so a fall-through to client-side search re-uses them via the
    /// range cache.
    pub async fn query_cost(&self, query: &str) -> Result<u64, IndexError> {
        let keys = ngram_keys(query, self.gram_size as usize);
        if keys.is_empty() {
            return Ok(0);
        }
        let lookups = join_all(keys.iter().map(|&k| self.lookup(k))).await;
        let mut total = 0u64;
        for rec in lookups {
            match rec? {
                Some(rec) => total += rec.size as u64,
                None => return Ok(0),
            }
        }
        Ok(total)
    }

    /// Exact match count for `key` without fetching its posting body — one
    /// dict-block read plus the posting's descriptive header (KBs; roaring
    /// stores per-container cardinalities there). `Ok(None)` when absent.
    pub async fn term_count(&self, key: u64) -> Result<Option<u64>, IndexError> {
        match self.lookup(key).await? {
            None => Ok(None),
            Some(rec) => Ok(Some(
                crate::posting::posting_cardinality(&self.fetch, rec.offset, rec.size as usize)
                    .await?,
            )),
        }
    }

    /// Exact-or-bounded match count for a strict-AND `query`, without fetching
    /// any posting body: `(count, exact)`. A query with one n-gram key gets its
    /// **exact** cardinality; several keys get the smallest per-key count — an
    /// **upper bound** on the intersection (`exact == false`). `(0, true)` when
    /// the query has no keys or any key is absent (the strict AND is empty).
    /// Not valid for fuzzy (`max_missing > 0`) matching, where the min is no
    /// longer a bound.
    pub async fn count_estimate(&self, query: &str) -> Result<(u64, bool), IndexError> {
        let keys = ngram_keys(query, self.gram_size as usize);
        if keys.is_empty() {
            return Ok((0, true));
        }
        let counts = join_all(keys.iter().map(|&k| self.term_count(k))).await;
        let mut min = u64::MAX;
        for c in counts {
            match c? {
                None => return Ok((0, true)),
                Some(n) => min = min.min(n),
            }
        }
        Ok((min, keys.len() == 1))
    }

    /// Returns the full posting (all docs) for `key`, or `Ok(None)` if the key is absent. One
    /// ranged dict-block read plus one ranged posting read.
    pub async fn posting(&self, key: u64) -> Result<Option<RoaringBitmap>, IndexError> {
        match self.lookup(key).await? {
            None => Ok(None),
            Some(rec) => {
                let bytes = self.fetch.read(rec.offset, rec.size as usize).await?;
                Ok(Some(deserialize(&bytes)?))
            }
        }
    }

    /// Fetches each term's **eager prefix** — the first [`EAGER_BUCKETS`] container buckets of
    /// its posting (docs `[0, EAGER_DOC_BOUND)`) — concurrently, as one wave. This is the v3
    /// replacement for the directly-addressed head blob: the candidate set for the instant first
    /// page (and facet counts), intersected by the caller; the rest is paged by [`TailScan`].
    async fn fetch_head_prefixes(
        &self,
        recs: &[DictRec],
    ) -> Result<Vec<RoaringBitmap>, IndexError> {
        let reads = recs.iter().map(|rec| {
            crate::posting::fetch_head_prefix(
                &self.fetch,
                rec.offset,
                rec.size as usize,
                EAGER_BUCKETS,
            )
        });
        let results = join_all(reads).await;
        let mut bitmaps = Vec::with_capacity(results.len());
        for r in results {
            bitmaps.push(r?);
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

        // WAVE 2: fetch every term's eager prefix (first EAGER_BUCKETS buckets) concurrently,
        // intersect — the cheap top-ranked candidates for the common case.
        let heads = self.fetch_head_prefixes(&recs).await?;
        let head_and = Self::intersect(heads).unwrap_or_default();

        let mut out: Vec<u32> = head_and.iter().take(limit).collect();
        if out.len() >= limit {
            return Ok(out);
        }

        // WAVE 3 (only if the eager prefix under-fills the limit): intersect the whole postings
        // with container-level ranged reads — the rarest posting in full, then only the
        // containers of the rest that overlap surviving candidates — so a rare phrase of common
        // trigrams costs KB, not every full posting. Append docs at/above the eager prefix.
        let ranges: Vec<(u64, usize)> = recs
            .iter()
            .map(|rec| (rec.offset, rec.size as usize))
            .collect();
        let full_and = crate::posting::tail_intersect_and(&self.fetch, &ranges).await?;
        for doc in full_and.iter() {
            if doc < EAGER_DOC_BOUND {
                continue; // already covered by the eager prefix above
            }
            if out.len() >= limit {
                break;
            }
            out.push(doc);
        }
        Ok(out)
    }

    /// Resolves `query` to candidate doc IDs by intersecting only the `k` rarest
    /// of its trigram postings (ranked by posting size). The result is a
    /// *superset* of the strict-AND result — every true match contains all
    /// trigrams, so it contains the `k` rarest — which the caller then verifies
    /// against each candidate's stored text, skipping the common trigrams' (often
    /// multi-MB) posting fetches. Candidates come back in ascending doc-ID order;
    /// an absent trigram (the strict AND is then empty) returns an empty vector.
    pub async fn search_candidates(&self, query: &str, k: usize) -> Result<Vec<u32>, IndexError> {
        let keys = ngram_keys(query, self.gram_size as usize);
        if keys.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
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
                None => return Ok(Vec::new()), // absent key -> strict AND empty
                Some(rec) => recs.push(rec),
            }
        }
        // Seed from the k rarest postings (smallest serialized size).
        recs.sort_by_key(|r| r.size as u64);
        recs.truncate(k.min(recs.len()));
        let mut postings = Vec::with_capacity(recs.len());
        for rec in &recs {
            let bytes = self.fetch.read(rec.offset, rec.size as usize).await?;
            postings.push(deserialize(&bytes)?);
        }
        Ok(Self::intersect(postings)
            .unwrap_or_default()
            .iter()
            .collect())
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
        let heads = self.fetch_head_prefixes(&recs).await?;
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
            tail_scan: None,
            tail_scan_tried: false,
        })
    }
}

/// One facet category's posting locations within the facet (`RRSF`) file.
#[derive(Clone, Copy)]
pub(crate) struct CatRange {
    /// Absolute offset of the head posting (docs `[0, 65536)`).
    pub(crate) head_off: u64,
    /// Head posting length in bytes.
    pub(crate) head_size: u32,
    /// Absolute offset of the tail posting (`head_off + head_size`).
    pub(crate) tail_off: u64,
    /// Tail posting length in bytes.
    pub(crate) tail_size: u32,
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
    pub(crate) fn new(fetch: F, fields: Vec<Vec<CatRange>>) -> Self {
        Self { fetch, fields }
    }

    /// Whether the filter imposes no constraint.
    pub fn is_empty(&self) -> bool {
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

    /// The full filter bitmap over both head and tail postings — the complete set
    /// of doc IDs satisfying the selected facets. Used to filter an arbitrary
    /// (e.g. vector-search) doc-ID list, which can touch the tail; the trigram
    /// cursor applies head and tail separately as it paginates.
    pub async fn full_bitmap(&self) -> Result<RoaringBitmap, IndexError> {
        let mut b = self.head_bitmap().await?;
        b |= self.tail_bitmap().await?;
        Ok(b)
    }

    /// The filter restricted to `ids` — the candidates-aware sibling of
    /// [`full_bitmap`](Self::full_bitmap). Where that fetches every selected
    /// posting whole (a broad category over a large corpus runs to tens of MB),
    /// this answers a *membership* question about a small ranked candidate list
    /// by reading each tail posting at **container granularity** — only the
    /// 64K-doc buckets the candidates occupy, via the same offset-table seek the
    /// tail scan uses — plus the (single-bucket, small) head posting only when a
    /// candidate sits below the head boundary. Small or non-seekable postings
    /// fall back to whole reads inside the subset reader, so the result equals
    /// `full_bitmap() ∩ ids` by construction.
    pub async fn membership_bitmap(
        &self,
        ids: &RoaringBitmap,
    ) -> Result<RoaringBitmap, IndexError> {
        if ids.is_empty() {
            return Ok(RoaringBitmap::new());
        }
        // Distinct container buckets the candidates span (ascending, since the
        // bitmap iterates in order). Bucket 0 selects a category's head posting;
        // the rest live in its tail.
        let mut keys: Vec<u16> = Vec::new();
        for id in ids.iter() {
            let k = (id >> 16) as u16;
            if keys.last() != Some(&k) {
                keys.push(k);
            }
        }
        let head_needed = keys.first() == Some(&0);
        let tail_keys: Vec<u16> = keys.into_iter().filter(|&k| k != 0).collect();

        // One concurrent wave across every selected category, like `combine`.
        let fetch = &self.fetch;
        let tail_keys = &tail_keys;
        let mut futs = Vec::new();
        for (fi, cats) in self.fields.iter().enumerate() {
            for c in cats {
                futs.push(async move {
                    let mut bm = RoaringBitmap::new();
                    if head_needed && c.head_size > 0 {
                        bm |= deserialize(&fetch.read(c.head_off, c.head_size as usize).await?)?;
                    }
                    if !tail_keys.is_empty() && c.tail_size > 0 {
                        bm |= crate::posting::read_posting_subset(
                            fetch,
                            c.tail_off,
                            c.tail_size as usize,
                            tail_keys,
                        )
                        .await?;
                    }
                    Ok::<(usize, RoaringBitmap), IndexError>((fi, bm))
                });
            }
        }
        let results = join_all(futs).await;
        let mut per_field: Vec<RoaringBitmap> = (0..self.fields.len())
            .map(|_| RoaringBitmap::new())
            .collect();
        for r in results {
            let (fi, bm) = r?;
            per_field[fi] |= bm;
        }
        per_field.sort_by_key(|b| b.len());
        let mut acc = ids.clone();
        for b in per_field {
            acc &= b;
            if acc.is_empty() {
                break;
            }
        }
        Ok(acc)
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
        let reads = flat
            .iter()
            .map(|(_, (off, len))| self.fetch.read(*off, *len));
        let results = join_all(reads).await;

        let mut per_field: Vec<RoaringBitmap> = (0..self.fields.len())
            .map(|_| RoaringBitmap::new())
            .collect();
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
    /// Incremental tail scanner for the strict-AND, unfiltered path: built on first
    /// tail access so a deep page fetches only the container buckets it spans.
    /// `None` (with `tail_scan_tried`) means the whole-tail load path is used.
    tail_scan: Option<crate::posting::TailScan>,
    tail_scan_tried: bool,
}

/// Candidate container buckets (each a 64K-doc range) intersected per incremental
/// tail step. Larger means fewer round-trips but more over-read past a filled page.
const TAIL_KEY_BATCH: usize = 8;

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
            tail_scan: None,
            tail_scan_tried: true,
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
            .map(|rec| (rec.offset, rec.size as usize))
            .collect();

        // Incremental path: a strict AND pages the tail in doc-ID (rank) order,
        // intersecting only the container buckets needed to reach `need` and stopping
        // there — so a deep page costs the buckets it spans, not the whole
        // (possibly hundreds-of-MB) tail. A facet filter is applied per bucket, so
        // filtered queries page incrementally too. Only fuzzy threshold (and a
        // non-seekable posting layout) fall back to the whole-tail load below.
        if self.min_match == self.recs.len() {
            if !self.tail_scan_tried {
                let facet_fields: Vec<Vec<(u64, usize)>> = match &self.filter {
                    Some(f) => f
                        .fields
                        .iter()
                        .map(|cats| {
                            cats.iter()
                                .map(|c| (c.tail_off, c.tail_size as usize))
                                .collect()
                        })
                        .collect(),
                    None => Vec::new(),
                };
                let facet_fetch = self.filter.as_ref().map(|f| &f.fetch);
                self.tail_scan = crate::posting::TailScan::open(
                    &self.fetch,
                    &ranges,
                    facet_fetch,
                    &facet_fields,
                    EAGER_BUCKETS,
                )
                .await?;
                self.tail_scan_tried = true;
            }
            if self.tail_scan.is_some() {
                let mut scan = self.tail_scan.take().unwrap();
                let facet_fetch = self.filter.as_ref().map(|f| &f.fetch);
                while self.results.len() < need && !scan.exhausted() {
                    let win = scan
                        .next_window(&self.fetch, facet_fetch, TAIL_KEY_BATCH)
                        .await?;
                    self.results.extend(win.iter());
                }
                if scan.exhausted() {
                    self.tail_done = true;
                }
                self.tail_scan = Some(scan);
                return Ok(());
            }
            // tail_scan is None: not seekable — fall through to the whole-tail load.
        }

        // Whole-tail load. Strict AND still seeks at container granularity (a rare
        // phrase of common trigrams costs KB, not every full posting); fuzzy
        // threshold needs each full posting; a facet tail is ANDed in after.
        let mut tail_and = if self.min_match == self.recs.len() {
            crate::posting::tail_intersect_and(&self.fetch, &ranges).await?
        } else {
            let reads = ranges.iter().map(|&(off, len)| self.fetch.read(off, len));
            let results = join_all(reads).await;
            let mut tails = Vec::with_capacity(results.len());
            for bytes in results {
                tails.push(deserialize(&bytes?)?);
            }
            threshold(tails, self.min_match).unwrap_or_default()
        };
        // The ranges are whole postings now, so drop the eager prefix the head already covers.
        tail_and.remove_range(0..EAGER_DOC_BOUND);
        if !tail_and.is_empty() {
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

    /// Number of head (popular) results — available with no tail fetch.
    pub fn head_count(&self) -> usize {
        self.head_result.len() as usize
    }

    /// Whether an unfetched tail intersection could still add results.
    pub fn pending_tail(&self) -> bool {
        !self.tail_done && !self.recs.is_empty()
    }

    /// Forces the lazy tail intersection to be fetched; afterwards `loaded` and
    /// `page` span the full result set. A no-op once the tail is loaded.
    pub async fn load_tail(&mut self) -> Result<(), IndexError> {
        self.ensure(usize::MAX).await
    }
}

/// The boot-region byte length of a serialized `RRS` from its leading header bytes — the
/// 20-byte header plus the sparse index (`sparse_count(ngrams, stride) * 8`), i.e. the
/// `[0, dictStart)` region [`Index::from_boot`] consumes. A bundle builder
/// ([`crate::splitset_bundle`]) calls this to slice each split's boot region without
/// materializing the whole index. `header` must hold at least the 16-byte header; errors on a
/// short header, bad magic, or an unexpected version (the same checks as [`Index::from_boot`]).
pub fn rrs_boot_len(header: &[u8]) -> Result<usize, IndexError> {
    if header.len() < HEADER_SIZE {
        return Err(IndexError::Malformed("short RRS header"));
    }
    if &header[0..4] != MAGIC {
        let mut m = [0u8; 4];
        m.copy_from_slice(&header[0..4]);
        return Err(IndexError::BadMagic(m));
    }
    let version = read_u16(header, 4);
    if version != FORMAT_VERSION {
        return Err(IndexError::BadVersion(version));
    }
    let ngrams = read_u32(header, 8);
    let stride = read_u32(header, 12);
    let (_, sparse_len) = sparse_layout(ngrams, stride)?;
    HEADER_SIZE
        .checked_add(sparse_len)
        .ok_or(IndexError::Malformed("RRS boot region length overflows"))
}

/// Number of sparse-index entries: `ceil(ngrams / stride)`.
fn sparse_count(ngrams: u32, stride: u32) -> usize {
    if ngrams == 0 || stride == 0 {
        return 0;
    }
    ngrams.div_ceil(stride) as usize
}

/// Validates a parsed RRS header's `ngrams`/`stride` pair and returns the sparse
/// index's `(entry count, byte length)`. `stride == 0` with a non-empty dictionary
/// is corruption — `sparse_count` would silently be 0 and every query would come
/// back empty rather than erroring. The byte length is checked so a hostile count
/// cannot wrap 32-bit `usize` arithmetic on wasm32 and defeat the boot-length
/// bounds checks downstream.
fn sparse_layout(ngrams: u32, stride: u32) -> Result<(usize, usize), IndexError> {
    if stride == 0 && ngrams != 0 {
        return Err(IndexError::Malformed("RRS stride is zero"));
    }
    let count = sparse_count(ngrams, stride);
    let bytes = count
        .checked_mul(8)
        .ok_or(IndexError::Malformed("RRS sparse index length overflows"))?;
    Ok((count, bytes))
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
pub(crate) fn deserialize(bytes: &[u8]) -> Result<RoaringBitmap, IndexError> {
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

    /// Paging the cursor in small steps must reconstruct exactly the ordered result
    /// list that forcing the whole tail (load_tail) yields — head docs (< 65536)
    /// first, then the tail buckets in ascending (rank) order — and finish with the
    /// tail no longer pending. Exercises the incremental ensure() path end to end.
    #[test]
    fn cursor_pages_match_full_tail_load() {
        use crate::build::{serialize_posting, write_index};
        use crate::ngram::ngram_keys;
        use crate::MemoryFetch;
        use futures::executor::block_on;
        use roaring::RoaringBitmap;

        fn bm(docs: &[u32]) -> RoaringBitmap {
            let mut b = RoaringBitmap::new();
            for &d in docs {
                b.insert(d);
            }
            b
        }
        fn rrs(entries: &[(u64, RoaringBitmap)]) -> MemoryFetch {
            let posts: Vec<(u64, Vec<u8>)> = entries
                .iter()
                .map(|(k, b)| (*k, serialize_posting(b)))
                .collect();
            let mut out = Vec::new();
            write_index(&mut out, 3, 2, posts).unwrap();
            MemoryFetch::new(out)
        }

        // "aaab" -> trigrams "aaa","aab"; a doc matches only when in BOTH (strict AND).
        let keys = ngram_keys("aaab", 3);
        assert_eq!(keys.len(), 2);
        let aaa = bm(&[0, 1, 2, 3, 70000, 70001, 70002, 140000, 200000, 5_000_000]);
        let aab = bm(&[0, 1, 2, 99, 70000, 70001, 70003, 140000, 200000, 5_000_000]);
        let idx = block_on(Index::open(rrs(&[(keys[0], aaa), (keys[1], aab)]))).unwrap();

        let mut full = block_on(idx.search_cursor("aaab", 0)).unwrap();
        block_on(full.load_tail()).unwrap();
        let want = block_on(full.page(0, 1000)).unwrap();
        assert_eq!(want, vec![0, 1, 2, 70000, 70001, 140000, 200000, 5_000_000]);

        let mut cur = block_on(idx.search_cursor("aaab", 0)).unwrap();
        assert_eq!(cur.head_count(), 3); // eager prefix = bucket 0 (< 65536): docs 0,1,2
        assert!(cur.pending_tail());
        let mut got = Vec::new();
        let mut off = 0;
        loop {
            let pg = block_on(cur.page(off, 3)).unwrap();
            if pg.is_empty() {
                break;
            }
            got.extend(pg);
            off += 3;
        }
        assert_eq!(got, want);
        assert!(!cur.pending_tail());
        assert_eq!(cur.loaded(), want.len());
    }
}
