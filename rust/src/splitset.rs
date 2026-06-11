//! The `RRSS` split-set manifest reader — names many immutable `RRS` splits and carries
//! the cross-split **pruning metadata** the monolith can't.
//!
//! A split set is a Quickwit-style manifest: each split is a vanilla [`crate::index::Index`]
//! (`RRS`) over a doc subset, in its own data file, and the `.rrss` manifest adds only what
//! is needed to query across them — the rank **tier**, the **doc-id range**, the byte size,
//! and a supersession **epoch** per split, plus the **base/delta** boundary and (reserved
//! for the enrichment step) per-split term-Bloom / facet-presence / time summaries. The
//! split objects stay plain `RRS`, so one split is exactly today's monolith. See
//! `SPLITSET.md` for the frozen byte layout and `tasks/007_split_set_index.in-progress.md`
//! for the design.
//!
//! Layout (`SPLITSET.md`): `[header][split entries][string blob][summary blob]`.
//! [`SplitSet::open`] makes the whole manifest resident in two ranged reads (header, then
//! body) — like [`crate::hotcache::Hotcache`] — and is the only fetch the manifest issues;
//! the per-split `RRS` objects and their boot bytes are opened lazily by the query path.

use crate::facet::{facet_key, FacetIndex};
use crate::fetch::RangeFetch;
use crate::index::{deserialize, read_u16, read_u32, read_u64, Index, IndexError};
use crate::ngram::ngram_keys;
use crate::sortcols::SortCols;
#[cfg(feature = "terms")]
use crate::terms::TermIndex;
use futures::future::join_all;
use roaring::RoaringBitmap;
use std::collections::BTreeMap;

/// `RRSS` magic.
const MAGIC: &[u8; 4] = b"RRSS";
/// Header size in bytes (see `SPLITSET.md`). Kept in sync with the builder.
const HEADER_SIZE: usize = 64;
/// Split-entry size in bytes. Kept in sync with the builder.
const ENTRY_SIZE: usize = 56;
/// Format version written into / accepted from the header.
const VERSION: u16 = 1;

/// Header flag bit: per-split term Bloom-filter summaries are present in the summary blob.
pub const FLAG_BLOOM: u16 = 1 << 0;
/// Header flag bit: per-split facet-presence bitset summaries are present.
pub const FLAG_FACET: u16 = 1 << 1;
/// Header flag bit: per-split time min/max summaries are present.
pub const FLAG_TIME: u16 = 1 << 2;
/// Header flag bit: per-split tombstone postings are present (delta-over-base supersession).
pub const FLAG_TOMBSTONES: u16 = 1 << 3;

/// Per-split flag bit: this split carries a tombstone posting in its summary region.
pub const SPLIT_FLAG_HAS_TOMBSTONE: u16 = 1 << 0;
/// Per-split flag bit: the split stores **absolute global** doc IDs (`global = local`) rather
/// than local 0-based IDs offset by `docIdLo`. Set by [compaction](crate::splitset_write)
/// when surviving IDs are gappy and must stay stable (no renumbering); `docIdLo`/`docIdHi`
/// then bound the global range present.
pub const SPLIT_FLAG_ABSOLUTE_IDS: u16 = 1 << 1;

/// Summary TLV tag for a term Bloom filter (skip a split whose vocabulary can't contain a
/// query n-gram). Matches `SPLITSET.md` §summary blob.
pub(crate) const SUMMARY_TAG_BLOOM: u8 = 1;
/// Summary TLV tag for a facet-presence list (`[count u32 LE][key u64 LE]*`, sorted) — the
/// `facet_key`s of the categories present in the split, so a facet-filtered query skips a split
/// that can't satisfy a selected field. Matches `SPLITSET.md` §summary blob.
pub(crate) const SUMMARY_TAG_FACET: u8 = 2;
/// Summary TLV tag for a tombstone posting (a portable RoaringBitmap of superseded **global**
/// doc IDs). Matches `SPLITSET.md` §summary blob.
pub(crate) const SUMMARY_TAG_TOMBSTONE: u8 = 4;

/// Sort-column descriptor flag bit: rank is descending (higher value = better rank).
pub const SORTCOL_FLAG_DESCENDING: u8 = 1 << 0;

/// Manifest body-kind (header byte 9): the per-split data files are trigram `RRS` indexes —
/// the default, so older manifests (which wrote `0` there) read back as trigram.
pub const BODY_KIND_TRIGRAM: u8 = 0;
/// Manifest body-kind (header byte 9): the per-split data files are term-level `RRTI` (FST)
/// indexes instead of trigram `RRS`. The manifest layout is otherwise identical; only how the
/// reader opens each split changes (see [`SplitBody`]). Requires the `terms` feature to read.
pub const BODY_KIND_TERM: u8 = 1;

/// How the base splits were assembled — recorded in the header so the reader adapts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Docs assigned to splits by rank; a split's rank range is its `[docIdLo, docIdHi]`
    /// (ascending doc id == descending popularity), so a top-K query reads only the top
    /// tier. Rank drift is absorbed by re-tiering at base compaction.
    Tiered,
    /// Docs assigned by ingest order / stable id; rank is a query-time fast field in the
    /// `RRSC` named by the [`SortColDescriptor`]. Drift-immune, but loses the top-K prune.
    StableKey,
}

impl Policy {
    /// Maps the on-disk `u8` policy code to a [`Policy`], or `None` for an unknown code.
    fn from_u8(code: u8) -> Option<Self> {
        match code {
            0 => Some(Policy::Tiered),
            1 => Some(Policy::StableKey),
            _ => None,
        }
    }

    /// The on-disk `u8` policy code (`0`=tiered, `1`=stable-key) — the builder's encoding.
    pub fn to_u8(self) -> u8 {
        match self {
            Policy::Tiered => 0,
            Policy::StableKey => 1,
        }
    }
}

/// The stable-key rank source: an `RRSC` sort-column store (by name) and the column within
/// it that holds the rank, plus its direction. Present only when the header named one (the
/// stable-key policy resolves top-K through it via [`crate::sortcols::SortCols::topk`]).
#[derive(Debug, Clone)]
pub struct SortColDescriptor {
    /// The `RRSC` data-file name holding the rank column.
    pub name: String,
    /// Column index within that `RRSC`.
    pub column: u16,
    /// Whether a higher value ranks better (descending sort).
    pub descending: bool,
}

/// One split named by the manifest: the data file its `RRS` lives in, its rank tier, its
/// doc-id range, byte size, supersession epoch, and (when present) the location of its
/// summary region within the manifest's summary blob.
#[derive(Debug, Clone)]
pub struct Split {
    /// The split's `RRS` data-file name (or URL) — where its per-query reads go.
    pub data_file: String,
    /// Rank tier (tiered policy); `0` for stable-key and for delta splits.
    pub tier: u16,
    /// Number of docs in the split.
    pub doc_count: u32,
    /// Minimum doc id present (inclusive).
    pub doc_id_lo: u32,
    /// Maximum doc id present (inclusive).
    pub doc_id_hi: u32,
    /// Per-split flags (`bit0` = has-tombstone summary).
    pub flags: u16,
    /// The split `.rrs` file size in bytes (byte-cap assert + total-size accounting).
    pub byte_size: u64,
    /// Flush/build epoch — supersession ordering (`0` for an additions-only base).
    pub epoch: u64,
    /// Offset of this split's summary region within the summary blob (when `summary_len > 0`).
    summary_off: u64,
    /// Length of this split's summary region in bytes (`0` when none).
    summary_len: u32,
}

impl Split {
    /// Whether this split carries a tombstone posting (a delta masking base doc IDs).
    pub fn has_tombstone(&self) -> bool {
        self.flags & SPLIT_FLAG_HAS_TOMBSTONE != 0
    }

    /// Whether the split stores absolute global doc IDs (vs local IDs offset by `docIdLo`).
    pub fn absolute_ids(&self) -> bool {
        self.flags & SPLIT_FLAG_ABSOLUTE_IDS != 0
    }

    /// Maps a split-local doc ID returned by the split's `RRS` to the global ID space:
    /// `global = docIdLo + local`, or `global = local` for an [absolute-id](Self::absolute_ids)
    /// split. For the tiered policy the global ID is the rank.
    pub fn to_global(&self, local: u32) -> u32 {
        if self.absolute_ids() {
            local
        } else {
            self.doc_id_lo.saturating_add(local)
        }
    }

    /// Whether `global` falls in this split's doc-id range `[docIdLo, docIdHi]`. A zero-doc
    /// split (a deletes-only flush) holds no documents — its range is nominal, claiming the
    /// then-unallocated next id — so it contains nothing, and never shadows the real split
    /// that later receives that id.
    pub fn contains(&self, global: u32) -> bool {
        self.doc_count > 0 && global >= self.doc_id_lo && global <= self.doc_id_hi
    }

    /// Inverse of [`to_global`](Self::to_global): maps a global doc ID back to this split's local
    /// ID (`local = global - docIdLo`, or `local = global` for an
    /// [absolute-id](Self::absolute_ids) split). The caller ensures [`contains`](Self::contains).
    pub fn to_local(&self, global: u32) -> u32 {
        if self.absolute_ids() {
            global
        } else {
            global.saturating_sub(self.doc_id_lo)
        }
    }
}

/// Per-field facet counts over a result set, aggregated across splits by category name — the
/// return shape of [`SplitSet::facet_counts`]. One entry per field that had at least one
/// non-zero category count.
#[derive(Debug, Clone)]
pub struct FieldCounts {
    /// Field name (e.g. `"year"`, `"type"`).
    pub field: String,
    /// `(category name, summed document count)` for the categories with a non-zero count, in
    /// first-seen order across the contributing splits.
    pub categories: Vec<(String, u64)>,
}

/// Resolves a split (or sort-column) data-file name to a [`RangeFetch`] for it. The manifest
/// only *names* files; the caller supplies how to open each one — a [`crate::MemoryFetch`]
/// over resident bytes in tests, an HTTP-Range fetcher keyed by URL in the browser. The call
/// is synchronous (it constructs the fetcher handle); the reads it performs are async.
pub trait SplitFetcher {
    /// The per-file [`RangeFetch`] this resolver hands out.
    type Fetch: RangeFetch + Clone;
    /// Opens a fetcher for the file named `name` (a split's `data_file`, or the stable-key
    /// sort-column's name).
    fn fetch_named(&self, name: &str) -> Self::Fetch;

    /// Optional **resident** boot bytes for `split` — its `RRS` header + sparse index
    /// (`[0, dictStart)`), held by the caller in a boot bundle (an `RRHC` of the manifest +
    /// the top tier's split boots). When `Some`, the query opens the split with
    /// [`Index::from_boot`] (no boot fetch); the default `None` opens it with a fetch. This is
    /// how the top-tier opens are amortized to a 1–2 round-trip cold boot.
    fn boot(&self, _split: &Split) -> Option<Vec<u8>> {
        None
    }

    /// The data-file name of an optional **global term Bloom** sidecar covering the whole
    /// set's vocabulary (the `bloom_build` layout), **range-probed** — `k` single-byte reads
    /// per key — never downloaded. The tiered query paths consult it lazily, only after the
    /// top tier yields nothing (the rare/absent-term signal), so a definite absence ends the
    /// tier descent instead of opening every remaining split; present-term queries never pay
    /// for it. The default `None` disables the probe.
    fn global_bloom_name(&self) -> Option<String> {
        None
    }
}

/// A term Bloom filter probed over [`RangeFetch`] **without downloading it**: the 8-byte
/// `[k u32][nbits u32]` header is read once at open, then each key costs `k` single-byte
/// reads at its hash positions (issued as one concurrent wave). `false` answers are
/// definitive — the Bloom-filter guarantee — which is what makes a multi-hundred-MB filter
/// usable from a browser: an absent-term check is ~`k` tiny ranged reads, not a download.
pub struct RemoteBloom<F: RangeFetch> {
    fetch: F,
    k: u32,
    nbits: u64,
}

impl<F: RangeFetch> RemoteBloom<F> {
    /// Reads and validates the 8-byte header.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let h = fetch.read(0, 8).await?;
        let k = read_u32(&h, 0);
        let nbits = read_u32(&h, 4) as u64;
        if k == 0 || k > 64 || nbits == 0 {
            return Err(IndexError::Malformed("bloom sidecar header invalid"));
        }
        Ok(Self { fetch, k, nbits })
    }

    /// Whether **every** key is possibly present — the strict-AND prune. All keys'
    /// bit positions are read in one concurrent wave; any definitely-absent key
    /// makes the whole conjunction `false`.
    pub async fn contains_all(&self, keys: &[u64]) -> Result<bool, IndexError> {
        let mut ranges: Vec<(u64, usize)> = Vec::with_capacity(keys.len() * self.k as usize);
        let mut positions: Vec<u64> = Vec::with_capacity(ranges.capacity());
        for &key in keys {
            for p in bloom_positions(key, self.k, self.nbits) {
                ranges.push((8 + p / 8, 1));
                positions.push(p);
            }
        }
        let bytes = crate::fetch::read_coalesced(&self.fetch, &ranges, 0).await?;
        Ok(positions
            .iter()
            .zip(&bytes)
            .all(|(&p, b)| b[0] & (1u8 << (p % 8)) != 0))
    }
}

/// A parsed `RRSS` manifest. Holds the split entries, the optional stable-key sort-column
/// descriptor, and the resident summary blob in memory — all made resident by the two
/// ranged reads in [`SplitSet::open`]. The per-split `RRS` objects are opened lazily by the
/// query path, not here.
#[derive(Debug, Clone)]
pub struct SplitSet {
    policy: Policy,
    flags: u16,
    tier_count: u16,
    base_count: u32,
    byte_cap: u64,
    /// N-gram window the splits were built with — lets the reader derive a query's keys for
    /// Bloom pruning without opening a split. `0` when unset (older/unspecified manifests).
    gram_size: u16,
    /// How each split's data file is encoded: [`BODY_KIND_TRIGRAM`] (`RRS`) or
    /// [`BODY_KIND_TERM`] (`RRTI`). Decides how [`open_split`] opens a split.
    body_kind: u8,
    sortcol: Option<SortColDescriptor>,
    splits: Vec<Split>,
    /// Concatenated per-split summary regions; sliced per split by `(summary_off, summary_len)`.
    summary_blob: Vec<u8>,
}

impl SplitSet {
    /// Boots from two ranged reads of the `.rrss`: the 64-byte header pins the section
    /// sizes, then the remaining `splitCount*56 + strBytes + summaryBytes` bytes make the
    /// whole manifest resident — split entries, string blob, and summary blob in memory.
    /// This is the only fetch the manifest itself issues.
    pub async fn open<F: RangeFetch>(rrss: F) -> Result<SplitSet, IndexError> {
        let header = rrss.read(0, HEADER_SIZE).await?;
        let body_len = Self::body_len(&header)?;
        let body = rrss.read(HEADER_SIZE as u64, body_len).await?;
        if body.len() < body_len {
            return Err(IndexError::Malformed("short RRSS body"));
        }
        Self::parse(&header, &body)
    }

    /// Parses a whole `.rrss` manifest already resident in `buf` (header + body) — the
    /// synchronous counterpart of [`open`](Self::open) for native callers that hold the bytes,
    /// e.g. [`crate::splitset_write::SplitSetWriter::resume`] reopening the prior manifest.
    pub fn from_bytes(buf: &[u8]) -> Result<SplitSet, IndexError> {
        let body_len = Self::body_len(buf)?;
        let body = buf
            .get(HEADER_SIZE..HEADER_SIZE + body_len)
            .ok_or(IndexError::Malformed("short RRSS body"))?;
        Self::parse(&buf[..HEADER_SIZE], body)
    }

    /// Validates the header magic/version and the base/delta boundary, and returns the body
    /// length (`splitCount*56 + strBytes + summaryBytes`) so a reader knows how much to fetch.
    fn body_len(header: &[u8]) -> Result<usize, IndexError> {
        if header.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRSS header"));
        }
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(header, 4);
        if version != VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let split_count = read_u32(header, 12) as usize;
        let base_count = read_u32(header, 16) as usize;
        let str_bytes = read_u32(header, 20) as usize;
        let summary_bytes = read_u64(header, 24) as usize;
        if base_count > split_count {
            return Err(IndexError::Malformed("RRSS base count exceeds split count"));
        }
        split_count
            .checked_mul(ENTRY_SIZE)
            .and_then(|m| m.checked_add(str_bytes))
            .and_then(|n| n.checked_add(summary_bytes))
            .ok_or(IndexError::Malformed("RRSS body size overflow"))
    }

    /// Parses the validated `header` (64 B) and `body` (split entries + string blob + summary
    /// blob) into a [`SplitSet`]. Shared by [`open`](Self::open) and [`from_bytes`](Self::from_bytes).
    fn parse(header: &[u8], body: &[u8]) -> Result<SplitSet, IndexError> {
        let flags = read_u16(header, 6);
        let policy =
            Policy::from_u8(header[8]).ok_or(IndexError::Malformed("RRSS unknown policy"))?;
        let body_kind = header[9]; // body-kind: 0 = trigram RRS, 1 = term RRTI
        let tier_count = read_u16(header, 10);
        let split_count = read_u32(header, 12) as usize;
        let base_count = read_u32(header, 16);
        let str_bytes = read_u32(header, 20) as usize;
        let summary_bytes = read_u64(header, 24) as usize;
        let byte_cap = read_u64(header, 32);
        let sortcol_name_off = read_u32(header, 40) as usize;
        let sortcol_name_len = read_u16(header, 44) as usize;
        let sortcol_column = read_u16(header, 46);
        let sortcol_flags = header[48];
        let gram_size = read_u16(header, 49);
        // header[51..56] is pad1, header[56..64] is reserved (all 0).

        let manifest_bytes = split_count * ENTRY_SIZE;
        if body.len() < manifest_bytes + str_bytes + summary_bytes {
            return Err(IndexError::Malformed("short RRSS body"));
        }

        // Section offsets within `body` (which starts at file offset HEADER_SIZE).
        let str_start = manifest_bytes;
        let summary_start = str_start + str_bytes;
        let string_blob = &body[str_start..str_start + str_bytes];
        let summary_blob = body[summary_start..summary_start + summary_bytes].to_vec();

        // Resolves a `(off, len)` name span against the string blob, or errors out of bounds.
        let read_name = |off: usize, len: usize| -> Result<String, IndexError> {
            let end = off
                .checked_add(len)
                .filter(|&e| e <= string_blob.len())
                .ok_or(IndexError::Malformed("RRSS name out of string blob"))?;
            String::from_utf8(string_blob[off..end].to_vec())
                .map_err(|_| IndexError::Malformed("RRSS non-UTF-8 name"))
        };

        let sortcol = if sortcol_name_len > 0 {
            Some(SortColDescriptor {
                name: read_name(sortcol_name_off, sortcol_name_len)?,
                column: sortcol_column,
                descending: sortcol_flags & SORTCOL_FLAG_DESCENDING != 0,
            })
        } else {
            None
        };

        let mut splits = Vec::with_capacity(split_count);
        for i in 0..split_count {
            let base = i * ENTRY_SIZE;
            let name_off = read_u32(body, base) as usize;
            let name_len = read_u16(body, base + 4) as usize;
            let tier = read_u16(body, base + 6);
            let doc_count = read_u32(body, base + 8);
            let doc_id_lo = read_u32(body, base + 12);
            let doc_id_hi = read_u32(body, base + 16);
            let split_flags = read_u16(body, base + 20);
            // base + 22..24 is pad (reserved 0).
            let byte_size = read_u64(body, base + 24);
            let epoch = read_u64(body, base + 32);
            let summary_off = read_u64(body, base + 40);
            let summary_len = read_u32(body, base + 48);
            // base + 52..56 is reserved (0).

            let data_file = read_name(name_off, name_len)?;
            if summary_len > 0 {
                let end = summary_off
                    .checked_add(summary_len as u64)
                    .ok_or(IndexError::Malformed("RRSS summary range overflow"))?;
                if end > summary_blob.len() as u64 {
                    return Err(IndexError::Malformed("RRSS summary out of summary blob"));
                }
            }
            splits.push(Split {
                data_file,
                tier,
                doc_count,
                doc_id_lo,
                doc_id_hi,
                flags: split_flags,
                byte_size,
                epoch,
                summary_off,
                summary_len,
            });
        }

        Ok(SplitSet {
            policy,
            flags,
            tier_count,
            base_count,
            byte_cap,
            gram_size,
            body_kind,
            sortcol,
            splits,
            summary_blob,
        })
    }

    /// The base-split assembly policy.
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// How each split's data file is encoded: [`BODY_KIND_TRIGRAM`] (`RRS`) or
    /// [`BODY_KIND_TERM`] (`RRTI`).
    pub fn body_kind(&self) -> u8 {
        self.body_kind
    }

    /// The header summary-presence flags (`FLAG_BLOOM` | `FLAG_FACET` | …).
    pub fn flags(&self) -> u16 {
        self.flags
    }

    /// Number of rank tiers (tiered policy); `0` for stable-key.
    pub fn tier_count(&self) -> u16 {
        self.tier_count
    }

    /// Number of base splits — splits `[0, base_count)` are base, the rest are delta.
    pub fn base_count(&self) -> u32 {
        self.base_count
    }

    /// The configured per-split byte cap the builder sealed at (informational).
    pub fn byte_cap(&self) -> u64 {
        self.byte_cap
    }

    /// The n-gram window the splits were built with (`0` if the manifest did not record it).
    pub fn gram_size(&self) -> u16 {
        self.gram_size
    }

    /// The stable-key rank source, if the manifest named one.
    pub fn sortcol(&self) -> Option<&SortColDescriptor> {
        self.sortcol.as_ref()
    }

    /// All splits, in manifest order (base splits first, then delta splits).
    pub fn splits(&self) -> &[Split] {
        &self.splits
    }

    /// The base splits — the bulk set rebuilt on the full-build cadence.
    pub fn base_splits(&self) -> &[Split] {
        &self.splits[..self.base_count as usize]
    }

    /// The delta splits — flushed since the base; empty for an additions-only/base-only set.
    pub fn delta_splits(&self) -> &[Split] {
        &self.splits[self.base_count as usize..]
    }

    /// The resident summary bytes for `split`, or `None` when it has no summaries. The slice
    /// is exactly `split.summary_len` bytes (the TLV region documented in `SPLITSET.md`).
    pub fn summary(&self, split: &Split) -> Option<&[u8]> {
        if split.summary_len == 0 {
            return None;
        }
        let start = split.summary_off as usize;
        let end = start + split.summary_len as usize;
        Some(&self.summary_blob[start..end])
    }

    /// Total on-disk size of every split (the split set's footprint, for the side-by-side
    /// total-size comparison against the monolith).
    pub fn total_byte_size(&self) -> u64 {
        self.splits.iter().map(|s| s.byte_size).sum()
    }

    /// The split's term Bloom filter (summary TLV tag 1), or `None` when it has none. Used by
    /// the query path to skip a split whose vocabulary cannot contain a query n-gram.
    fn bloom(&self, split: &Split) -> Option<&[u8]> {
        find_tlv(self.summary(split)?, SUMMARY_TAG_BLOOM)
            .ok()
            .flatten()
    }

    /// Whether `split` can be pruned for `keys`: it carries a Bloom filter and at least one
    /// query key is **definitely absent** from it (Bloom filters have no false negatives, so an
    /// absent key means no doc in the split has that n-gram, hence no strict-AND match). A
    /// split with no Bloom is never pruned. `keys` empty → never prune.
    fn pruned_by_bloom(&self, split: &Split, keys: &[u64]) -> bool {
        match self.bloom(split) {
            Some(bloom) => keys.iter().any(|&k| !bloom_contains(bloom, k)),
            None => false,
        }
    }

    /// Resolves `query` to its top-`k` global doc IDs, dispatching on the manifest policy and
    /// on whether any delta splits are present. Splits store local 0-based ids (or absolute
    /// ids); results are remapped to the global id space via [`Split::to_global`], which for
    /// the tiered policy is the rank order.
    ///
    /// With no delta splits this takes the fast path — the tiered short-circuit (read only as
    /// many tiers as fill the page) or the stable-key `SortCols` sort. When delta splits are
    /// present it falls back to the thorough base+delta merge with supersession, which costs
    /// more — the documented incentive to compact deltas back into the base.
    pub async fn search<R: SplitFetcher>(
        &self,
        resolver: &R,
        query: &str,
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Derive the query's n-gram keys once (for Bloom pruning); empty when the manifest
        // didn't record a gram size or the query is too short, which disables pruning safely.
        let keys = ngram_keys(query, self.gram_size as usize);
        if !self.delta_splits().is_empty() {
            return self.search_with_delta(resolver, query, &keys, limit).await;
        }
        match self.policy {
            Policy::Tiered => self.search_tiered(resolver, query, &keys, limit).await,
            Policy::StableKey => self.search_stable_key(resolver, query, &keys, limit).await,
        }
    }

    /// Like [`search`](Self::search) but ANDs a facet `filter` (a list of `(field, category)`
    /// selections — within-field OR, across-field AND) into the result. Each surviving split's
    /// own `RRSF` sidecar (named `‹split›.rrf`) resolves the filter; a split is pruned without a
    /// fetch when its **facet-presence** summary shows it holds none of a selected field's
    /// categories (or it has no facets at all). An empty filter is exactly [`search`](Self::search).
    pub async fn search_filtered<R: SplitFetcher>(
        &self,
        resolver: &R,
        query: &str,
        filter: &[(String, String)],
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if filter.is_empty() {
            return self.search(resolver, query, limit).await;
        }
        let keys = ngram_keys(query, self.gram_size as usize);
        // The filtered category keys grouped by field — a split is pruned if, for any field, none
        // of its selected categories are present.
        let mut by_field: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for (field, cat) in filter {
            by_field
                .entry(field.as_str())
                .or_default()
                .push(facet_key(field, cat));
        }
        let fields: Vec<Vec<u64>> = by_field.into_values().collect();

        // Whether `split` survives both the term-Bloom prune and the facet-presence prune.
        let survives = |split: &Split| -> Result<bool, IndexError> {
            Ok(!self.pruned_by_bloom(split, &keys) && !self.facet_pruned(split, &fields)?)
        };

        // Tiered, base-only: keep the short-circuit — splits are in rank order, so accumulate
        // each surviving split's filtered (rank-ordered) hits until the page is full.
        if self.delta_splits().is_empty() && self.policy == Policy::Tiered {
            let mut out: Vec<u32> = Vec::with_capacity(limit);
            for (i, split) in self.base_splits().iter().enumerate() {
                if out.len() >= limit {
                    break;
                }
                // Same lazy global-Bloom gate as the unfiltered tiered loop: an empty
                // top tier + a definitively-absent term ends the descent.
                if i == 1
                    && out.is_empty()
                    && !self.global_bloom_says_present(resolver, &keys).await
                {
                    return Ok(out);
                }
                if !survives(split)? {
                    continue;
                }
                for id in self
                    .search_split_filtered(resolver, split, query, filter)
                    .await?
                {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(id);
                }
            }
            return Ok(out);
        }

        // Stable-key or base+delta: search every surviving split, merge, supersede, rank.
        let mut base: Vec<u32> = Vec::new();
        for split in self.base_splits() {
            if survives(split)? {
                base.extend(
                    self.search_split_filtered(resolver, split, query, filter)
                        .await?,
                );
            }
        }
        let mut delta: Vec<u32> = Vec::new();
        for split in self.delta_splits() {
            if survives(split)? {
                delta.extend(
                    self.search_split_filtered(resolver, split, query, filter)
                        .await?,
                );
            }
        }
        let mut dead = RoaringBitmap::new();
        for split in self.splits() {
            if let Some(tb) = self.tombstone(split)? {
                dead |= tb;
            }
        }
        base.retain(|id| !dead.contains(*id));
        delta.retain(|id| !dead.contains(*id));

        let mut ranked = match self.policy {
            Policy::Tiered => {
                base.sort_unstable();
                base
            }
            Policy::StableKey => {
                let n = base.len();
                self.rank_stable_key(resolver, base, n).await?
            }
        };
        delta.sort_unstable();
        ranked.extend(delta);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Per-(field, category) facet counts over `ids` (global doc IDs — e.g. a query's ranked
    /// result page), aggregated across the splits those IDs fall in. The split set has no global
    /// facet table (each split carries its own `RRSF` sidecar), so this groups the IDs by split,
    /// counts each split's local matches against its own `‹split›.rrf`, and **sums by field and
    /// category name**. Fields and categories appear in first-seen order; a category with a zero
    /// count is omitted (the caller renders missing keys as `0`). Splits a result ID never lands
    /// in — and those lacking a facet sidecar — contribute nothing. One `.rrf` open per
    /// contributing split (counts are over each split's head postings, like the monolith's
    /// in-memory facet counts).
    pub async fn facet_counts<R: SplitFetcher>(
        &self,
        resolver: &R,
        ids: &[u32],
    ) -> Result<Vec<FieldCounts>, IndexError> {
        // Group the global result IDs by the split they belong to, as split-local IDs.
        let mut per_split: BTreeMap<usize, RoaringBitmap> = BTreeMap::new();
        for &gid in ids {
            if let Some((si, split)) = self
                .splits
                .iter()
                .enumerate()
                .find(|(_, s)| s.contains(gid))
            {
                per_split.entry(si).or_default().insert(split.to_local(gid));
            }
        }

        // Aggregate each contributing split's counts into name-keyed fields (first-seen order).
        let mut fields: Vec<FieldCounts> = Vec::new();
        let mut field_pos: BTreeMap<String, usize> = BTreeMap::new();
        for (si, local) in per_split {
            let split = &self.splits[si];
            let facets =
                match FacetIndex::open(resolver.fetch_named(&facet_file_name(&split.data_file)))
                    .await
                {
                    Ok(f) => f,
                    Err(_) => continue, // a split with no facet sidecar contributes nothing
                };
            let counts = facets.counts(&local); // Vec<Vec<u64>> aligned to facets.fields
            for (fi, field) in facets.fields.iter().enumerate() {
                let fp = *field_pos.entry(field.name.clone()).or_insert_with(|| {
                    fields.push(FieldCounts {
                        field: field.name.clone(),
                        categories: Vec::new(),
                    });
                    fields.len() - 1
                });
                for (ci, cat) in field.categories.iter().enumerate() {
                    let c = counts[fi][ci];
                    if c == 0 {
                        continue;
                    }
                    let cats = &mut fields[fp].categories;
                    match cats.iter_mut().find(|(n, _)| *n == cat.name) {
                        Some((_, existing)) => *existing += c,
                        None => cats.push((cat.name.clone(), c)),
                    }
                }
            }
        }
        Ok(fields)
    }

    /// Tiered top-K: the base splits are in `(tier, docIdLo)` order — i.e. rank order — and
    /// hold disjoint, increasing global id ranges, so the global top-K is just the
    /// concatenation of each split's local top-`remaining`. The loop **stops opening splits**
    /// the moment the page is filled, so a top-K query that fits in tier 0 reads only tier 0.
    /// This is the bandwidth win; Bloom-pruned splits are skipped without a fetch.
    async fn search_tiered<R: SplitFetcher>(
        &self,
        resolver: &R,
        query: &str,
        keys: &[u64],
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        let mut out: Vec<u32> = Vec::with_capacity(limit);
        for (i, split) in self.base_splits().iter().enumerate() {
            if out.len() >= limit {
                break; // tiered short-circuit — the cold tiers leave the hot path
            }
            // An empty top tier is the rare/absent-term signal: consult the optional
            // global Bloom once (k byte-probes per key) — a term absent from the whole
            // set's vocabulary ends the descent here instead of opening every split.
            if i == 1 && out.is_empty() && !self.global_bloom_says_present(resolver, keys).await {
                return Ok(out);
            }
            if self.pruned_by_bloom(split, keys) {
                continue; // the split's vocabulary can't contain a query n-gram — no fetch
            }
            let remaining = limit - out.len();
            let idx = open_split(resolver, split, self.body_kind).await?;
            let local = idx.search(query, remaining).await?;
            out.extend(local.into_iter().map(|l| split.to_global(l)));
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Probes the resolver's optional global term Bloom for `keys`: `false` only on a
    /// **definitive** absence (every other outcome — no sidecar configured, the sidecar
    /// unreadable, keys empty, all keys possibly present — answers `true`, so the probe can
    /// only ever skip work that provably cannot match).
    async fn global_bloom_says_present<R: SplitFetcher>(&self, resolver: &R, keys: &[u64]) -> bool {
        if keys.is_empty() {
            return true;
        }
        let Some(name) = resolver.global_bloom_name() else {
            return true;
        };
        match RemoteBloom::open(resolver.fetch_named(&name)).await {
            Ok(bloom) => bloom.contains_all(keys).await.unwrap_or(true),
            Err(_) => true, // a missing/unreadable sidecar never breaks search
        }
    }

    /// Stable-key top-K: rank is not the id order, so every surviving split must be searched
    /// (the splits are opened and searched in one concurrent wave), their matches merged into
    /// the global id space, and the top-`k` taken by the sort-column the manifest names
    /// ([`SortCols::topk`]). With no sort-column descriptor the candidates fall back to
    /// ascending global-id (ingest) order. Bloom-pruned splits are skipped without a fetch.
    async fn search_stable_key<R: SplitFetcher>(
        &self,
        resolver: &R,
        query: &str,
        keys: &[u64],
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        let candidates = self
            .search_all(resolver, self.base_splits(), query, keys)
            .await?;
        self.rank_stable_key(resolver, candidates, limit).await
    }

    /// Base + delta merge with supersession. Searches *all* base and delta splits (no
    /// short-circuit), removes any doc masked by a delta tombstone, ranks the base survivors
    /// by policy, then appends the delta survivors in ingest order — so freshly-added docs are
    /// findable after the base until compaction folds them in (where they earn real
    /// ranks/tiers). Truncates to `k`.
    async fn search_with_delta<R: SplitFetcher>(
        &self,
        resolver: &R,
        query: &str,
        keys: &[u64],
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        let base = self
            .search_all(resolver, self.base_splits(), query, keys)
            .await?;
        let mut delta = self
            .search_all(resolver, self.delta_splits(), query, keys)
            .await?;

        // Dead set: the union of every split's tombstone posting (typically the deltas').
        let mut dead = RoaringBitmap::new();
        for split in self.splits() {
            if let Some(tb) = self.tombstone(split)? {
                dead |= tb;
            }
        }
        let live = |ids: Vec<u32>| -> Vec<u32> {
            ids.into_iter().filter(|id| !dead.contains(*id)).collect()
        };

        let mut ranked = match self.policy {
            Policy::Tiered => {
                let mut b = live(base);
                b.sort_unstable(); // global id == rank
                b
            }
            Policy::StableKey => {
                let b = live(base);
                let n = b.len();
                self.rank_stable_key(resolver, b, n).await?
            }
        };
        delta = live(delta);
        delta.sort_unstable(); // newest tier appended in ingest order
        ranked.extend(delta);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Opens and searches every split in `splits` in one concurrent wave (full strict-AND, no
    /// per-split limit), returning all matches remapped to the global id space. Bloom-pruned
    /// splits are skipped without a fetch.
    async fn search_all<R: SplitFetcher>(
        &self,
        resolver: &R,
        splits: &[Split],
        query: &str,
        keys: &[u64],
    ) -> Result<Vec<u32>, IndexError> {
        let body_kind = self.body_kind;
        let opens = splits
            .iter()
            .filter(|split| !self.pruned_by_bloom(split, keys))
            .map(|split| async move {
                let idx = open_split(resolver, split, body_kind).await?;
                let local = idx.search(query, usize::MAX).await?;
                Ok::<Vec<u32>, IndexError>(local.into_iter().map(|l| split.to_global(l)).collect())
            });
        let mut out: Vec<u32> = Vec::new();
        for res in join_all(opens).await {
            out.extend(res?);
        }
        Ok(out)
    }

    /// Ranks global-id `candidates` by the stable-key sort-column (top-`k`), or by ascending
    /// global id when the manifest names no sort-column.
    async fn rank_stable_key<R: SplitFetcher>(
        &self,
        resolver: &R,
        mut candidates: Vec<u32>,
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        match &self.sortcol {
            Some(desc) => {
                let sc = SortCols::open(resolver.fetch_named(&desc.name)).await?;
                sc.topk(desc.column as usize, &candidates, limit, desc.descending)
                    .await
            }
            None => {
                candidates.sort_unstable();
                candidates.truncate(limit);
                Ok(candidates)
            }
        }
    }

    /// The split's tombstone posting (superseded **global** doc IDs), parsed from its summary
    /// TLV region, or `None` when it has none.
    fn tombstone(&self, split: &Split) -> Result<Option<RoaringBitmap>, IndexError> {
        match find_tlv(
            self.summary(split).unwrap_or_default(),
            SUMMARY_TAG_TOMBSTONE,
        )? {
            Some(bytes) => Ok(Some(deserialize(bytes)?)),
            None => Ok(None),
        }
    }

    /// The split's facet-presence keys (the sorted `facet_key`s of the categories it holds),
    /// parsed from summary TLV tag 2, or `None` when it has none.
    fn facet_keys(&self, split: &Split) -> Result<Option<Vec<u64>>, IndexError> {
        let Some(bytes) = find_tlv(self.summary(split).unwrap_or_default(), SUMMARY_TAG_FACET)?
        else {
            return Ok(None);
        };
        if bytes.len() < 4 {
            return Err(IndexError::Malformed(
                "RRSS facet-presence summary too short",
            ));
        }
        let count = read_u32(bytes, 0) as usize;
        count
            .checked_mul(8)
            .and_then(|n| n.checked_add(4))
            .filter(|&e| e <= bytes.len())
            .ok_or(IndexError::Malformed(
                "RRSS facet-presence summary out of range",
            ))?;
        Ok(Some(
            (0..count).map(|i| read_u64(bytes, 4 + i * 8)).collect(),
        ))
    }

    /// Whether `split` can be pruned for a facet filter: the manifest carries facet-presence
    /// summaries (`FLAG_FACET`) and either this split has none (it indexed no facet values) or
    /// for some selected field none of that field's category keys are present (the across-field
    /// AND can never be satisfied). Without `FLAG_FACET` the manifest has no facet information
    /// — summaries absent wholesale (e.g. stripped to slim the boot read) must not read as "no
    /// facets", so nothing is pruned and the filter resolves against each split's `.rrf`.
    /// `fields` is the per-field list of selected category keys.
    fn facet_pruned(&self, split: &Split, fields: &[Vec<u64>]) -> Result<bool, IndexError> {
        if fields.is_empty() || self.flags & FLAG_FACET == 0 {
            return Ok(false);
        }
        match self.facet_keys(split)? {
            None => Ok(true),
            Some(present) => Ok(fields
                .iter()
                .any(|cat_keys| !cat_keys.iter().any(|ck| present.binary_search(ck).is_ok()))),
        }
    }

    /// Searches `split` for `query` ANDed with the facet `filter` resolved against the split's
    /// own `RRSF` sidecar, returning all matching global ids (rank order within the split).
    async fn search_split_filtered<R: SplitFetcher>(
        &self,
        resolver: &R,
        split: &Split,
        query: &str,
        filter: &[(String, String)],
    ) -> Result<Vec<u32>, IndexError> {
        // Resolve the filter from the sidecar's META alone (KBs) and bail BEFORE opening
        // anything else: filtering never reads head postings, and an unsatisfiable arm — a
        // field this split's sidecar doesn't carry — skips the split without touching its
        // body. The previous order (open split, then `FacetIndex::open` with its eager
        // whole-region head load) paid ~MBs × splits to answer such a filter with zero hits.
        let facets =
            FacetIndex::open_meta(resolver.fetch_named(&facet_file_name(&split.data_file))).await?;
        let resolved = facets.resolve(filter);
        if resolved.has_empty_arm() {
            return Ok(Vec::new());
        }
        // Only the trigram body exposes the filtered cursor; the `Term` arm is absent without the
        // `terms` feature, so the match collapses to one infallible pattern there.
        #[allow(clippy::infallible_destructuring_match)]
        let idx = match open_split(resolver, split, self.body_kind).await? {
            SplitBody::Trigram(idx) => idx,
            #[cfg(feature = "terms")]
            SplitBody::Term(_) => {
                return Err(IndexError::Unsupported(
                    "facet-filtered search is not yet supported on term-bodied splits",
                ))
            }
        };
        let mut cursor = idx.search_cursor_filtered(query, 0, Some(resolved)).await?;
        cursor.load_tail().await?;
        let local = cursor.page(0, usize::MAX).await?;
        Ok(local.into_iter().map(|l| split.to_global(l)).collect())
    }
}

/// The facet sidecar file name for a split's data-file name: `‹stem›.rrf`. Handles both the
/// trigram `.rrs` and term `.rrt` extensions; an unrecognized name just gets `.rrf` appended.
fn facet_file_name(data_file: &str) -> String {
    match data_file
        .strip_suffix(".rrs")
        .or_else(|| data_file.strip_suffix(".rrt"))
    {
        Some(stem) => format!("{stem}.rrf"),
        None => format!("{data_file}.rrf"),
    }
}

/// An opened split body — either a trigram [`Index`] (`RRS`) or, when the manifest's body-kind
/// is [`BODY_KIND_TERM`], a term-level [`TermIndex`] (`RRTI`). Both expose the same
/// `search(query, limit) -> local doc IDs` contract, so the query paths stay body-agnostic.
enum SplitBody<F: RangeFetch> {
    Trigram(Index<F>),
    #[cfg(feature = "terms")]
    Term(TermIndex<F>),
}

impl<F: RangeFetch> SplitBody<F> {
    /// Up to `limit` matching **local** doc IDs in rank (ascending-id) order — the uniform
    /// per-split search both body kinds implement.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, IndexError> {
        match self {
            SplitBody::Trigram(idx) => idx.search(query, limit).await,
            #[cfg(feature = "terms")]
            SplitBody::Term(idx) => idx.search(query, limit).await,
        }
    }
}

/// Opens `split` via `resolver` according to `body_kind`. A trigram split uses its inlined boot
/// bytes ([`SplitFetcher::boot`]) when present (a zero-round-trip [`Index::from_boot`]) and a
/// fetched [`Index::open`] otherwise; a term split always cold-opens via [`TermIndex::open`]
/// (the term index has no boot path yet). Errors if a term split is named but the `terms`
/// feature is off.
async fn open_split<R: SplitFetcher>(
    resolver: &R,
    split: &Split,
    body_kind: u8,
) -> Result<SplitBody<R::Fetch>, IndexError> {
    let fetch = resolver.fetch_named(&split.data_file);
    if body_kind == BODY_KIND_TERM {
        #[cfg(feature = "terms")]
        return Ok(SplitBody::Term(TermIndex::open(fetch).await?));
        #[cfg(not(feature = "terms"))]
        return Err(IndexError::Malformed(
            "term-bodied split set requires the `terms` feature",
        ));
    }
    match resolver.boot(split) {
        Some(boot) => Ok(SplitBody::Trigram(Index::from_boot(&boot, fetch)?)),
        None => Ok(SplitBody::Trigram(Index::open(fetch).await?)),
    }
}

/// Scans a summary TLV region (`[tag u8][len u32 LE][bytes]` records) for the first record with
/// `tag`, returning its payload slice or `None`. Errors on a malformed (out-of-range) record.
pub(crate) fn find_tlv(summary: &[u8], tag: u8) -> Result<Option<&[u8]>, IndexError> {
    let mut off = 0usize;
    while off + 5 <= summary.len() {
        let rec_tag = summary[off];
        let len = read_u32(summary, off + 1) as usize;
        let start = off + 5;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= summary.len())
            .ok_or(IndexError::Malformed("RRSS summary TLV out of range"))?;
        if rec_tag == tag {
            return Ok(Some(&summary[start..end]));
        }
        off = end;
    }
    Ok(None)
}

/// Frames `payload` as a `[tag u8][len u32 LE][payload]` summary TLV record. A build-side
/// helper (the reader reads TLVs with [`find_tlv`]), so it is excluded from the wasm reader.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn tlv_record(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A 64-bit mixer (splitmix64) — the deterministic, portable hash the Bloom filter derives its
/// two base hashes from (so the Go builder can reproduce a filter byte-for-byte).
fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The `k` bit positions an n-gram `key` maps to in an `nbits`-bit Bloom filter, by double
/// hashing two splitmix64 derivations of the key (`h1 + i*h2 mod nbits`).
fn bloom_positions(key: u64, k: u32, nbits: u64) -> impl Iterator<Item = u64> {
    let h1 = splitmix64(key);
    let h2 = splitmix64(key ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (0..k as u64).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % nbits)
}

/// The number of hash functions `k` for a target `bits_per_key` (`≈ bits_per_key·ln2`,
/// clamped to `1..=16`). A build-side helper (the reader reads `k` from the filter header), so
/// it is excluded from the wasm reader.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn bloom_k(bits_per_key: u32) -> u32 {
    ((bits_per_key as f64 * std::f64::consts::LN_2).round() as u32).clamp(1, 16)
}

/// Builds a term Bloom filter over `keys` at roughly `bits_per_key` bits per key, serialized as
/// `[k u32 LE][nbits u32 LE][ceil(nbits/8) bytes]`. Deterministic so the native builder and the
/// Go builder produce identical bytes; a build-side helper, so it is excluded from the wasm
/// reader. The same layout serves a per-split summary, or a standalone **global** sidecar
/// probed remotely by [`RemoteBloom`].
#[cfg(not(target_arch = "wasm32"))]
pub fn bloom_build(keys: &[u64], bits_per_key: u32) -> Vec<u8> {
    let n = (keys.len() as u64).max(1);
    // The serialized nbits field is u32: clamp to the largest 8-multiple that
    // fits, so a pathological vocabulary degrades to a higher false-positive
    // rate instead of a filter whose truncated stored modulus disagrees with the
    // build modulus — that disagreement yields FALSE NEGATIVES, and a Bloom
    // false negative prunes splits that hold real matches. (Mirrors the Go
    // builder's bloomBuild byte-for-byte.)
    let nbits = (n * bits_per_key as u64)
        .max(64)
        .next_multiple_of(8)
        .min(u32::MAX as u64 & !7);
    let k = bloom_k(bits_per_key);
    let mut bits = vec![0u8; (nbits / 8) as usize];
    for &key in keys {
        for pos in bloom_positions(key, k, nbits) {
            bits[(pos / 8) as usize] |= 1u8 << (pos % 8);
        }
    }
    let mut out = Vec::with_capacity(8 + bits.len());
    out.extend_from_slice(&k.to_le_bytes());
    out.extend_from_slice(&(nbits as u32).to_le_bytes());
    out.extend_from_slice(&bits);
    out
}

/// Tests whether `key` is **possibly present** in a [`bloom_build`] filter. A `false` is
/// definitive (the key was never inserted); a `true` may be a false positive. A malformed or
/// truncated filter conservatively returns `true` (never prune on bad data).
pub(crate) fn bloom_contains(bloom: &[u8], key: u64) -> bool {
    if bloom.len() < 8 {
        return true;
    }
    let k = read_u32(bloom, 0);
    let nbits = read_u32(bloom, 4) as u64;
    if nbits == 0 {
        return true;
    }
    let bits = &bloom[8..];
    for pos in bloom_positions(key, k, nbits) {
        let byte = (pos / 8) as usize;
        if byte >= bits.len() {
            return true; // truncated filter -> conservative (never prune on bad data)
        }
        if bits[byte] & (1u8 << (pos % 8)) == 0 {
            return false; // this n-gram was never inserted -> the split cannot match
        }
    }
    true
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::splitset_build::{write_splitset, SortColSpec, SplitSetConfig, SplitSpec};
    use futures::executor::block_on;

    /// A split spec with the common fields filled and no summary.
    fn spec(name: &str, tier: u16, lo: u32, hi: u32, bytes: u64) -> SplitSpec {
        SplitSpec {
            data_file: name.to_string(),
            tier,
            doc_count: hi - lo + 1,
            doc_id_lo: lo,
            doc_id_hi: hi,
            epoch: 0,
            byte_size: bytes,
            flags: 0,
            summary: Vec::new(),
        }
    }

    /// Builds an in-memory `.rrss` over `splits` and opens it.
    fn build(splits: &[SplitSpec], config: &SplitSetConfig) -> SplitSet {
        let mut buf = Vec::new();
        write_splitset(&mut buf, splits, config).unwrap();
        block_on(SplitSet::open(MemoryFetch::new(buf))).unwrap()
    }

    fn tiered(base_count: u32, tier_count: u16) -> SplitSetConfig {
        SplitSetConfig {
            policy: Policy::Tiered,
            tier_count,
            base_count,
            byte_cap: 32 << 20,
            gram_size: 3,
            body_kind: BODY_KIND_TRIGRAM,
            sortcol: None,
            flags: 0,
        }
    }

    #[test]
    fn round_trips_tiered_splits() {
        // Two tiers: tier 0 holds the top-cited docs (low ids), tier 1 the next band.
        let splits = vec![
            spec("corpus-s00000.rrs", 0, 0, 65_535, 30 << 20),
            spec("corpus-s00001.rrs", 1, 65_536, 200_000, 28 << 20),
            spec("corpus-s00002.rrs", 1, 200_001, 350_000, 25 << 20),
        ];
        let ss = build(&splits, &tiered(3, 2));

        assert_eq!(ss.policy(), Policy::Tiered);
        assert_eq!(ss.tier_count(), 2);
        assert_eq!(ss.base_count(), 3);
        assert_eq!(ss.byte_cap(), 32 << 20);
        assert!(ss.sortcol().is_none());
        assert_eq!(ss.splits().len(), 3);
        assert_eq!(ss.delta_splits().len(), 0);

        let names: Vec<&str> = ss.splits().iter().map(|s| s.data_file.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "corpus-s00000.rrs",
                "corpus-s00001.rrs",
                "corpus-s00002.rrs"
            ]
        );
        let s1 = &ss.splits()[1];
        assert_eq!(s1.tier, 1);
        assert_eq!(s1.doc_id_lo, 65_536);
        assert_eq!(s1.doc_id_hi, 200_000);
        assert_eq!(s1.doc_count, 200_000 - 65_536 + 1);
        assert_eq!(s1.byte_size, 28 << 20);
        assert!(!s1.has_tombstone());
        assert_eq!(ss.total_byte_size(), (30 + 28 + 25) << 20);
    }

    #[test]
    fn round_trips_stable_key_with_sortcol() {
        let config = SplitSetConfig {
            policy: Policy::StableKey,
            tier_count: 0,
            base_count: 2,
            byte_cap: 16 << 20,
            gram_size: 3,
            body_kind: BODY_KIND_TRIGRAM,
            sortcol: Some(SortColSpec {
                name: "corpus.rrsc".to_string(),
                column: 3,
                descending: true,
            }),
            flags: 0,
        };
        let splits = vec![
            spec("corpus-s00000.rrs", 0, 0, 99_999, 16 << 20),
            spec("corpus-s00001.rrs", 0, 100_000, 180_000, 14 << 20),
        ];
        let ss = build(&splits, &config);

        assert_eq!(ss.policy(), Policy::StableKey);
        let sc = ss.sortcol().expect("sortcol descriptor present");
        assert_eq!(sc.name, "corpus.rrsc");
        assert_eq!(sc.column, 3);
        assert!(sc.descending);
    }

    #[test]
    fn partitions_base_and_delta() {
        // Two base splits (tiered) + one ingest-ordered delta split in the high id range.
        let mut splits = vec![
            spec("base-s00000.rrs", 0, 0, 65_535, 30 << 20),
            spec("base-s00001.rrs", 1, 65_536, 250_000, 27 << 20),
        ];
        let mut delta = spec("delta-s00000.rrs", 0, 4_000_000_000, 4_000_010_000, 1 << 20);
        delta.epoch = 7;
        delta.flags = SPLIT_FLAG_HAS_TOMBSTONE;
        splits.push(delta);

        let ss = build(&splits, &tiered(2, 2));
        assert_eq!(ss.base_count(), 2);
        assert_eq!(ss.base_splits().len(), 2);
        assert_eq!(ss.delta_splits().len(), 1);

        let d = &ss.delta_splits()[0];
        assert_eq!(d.data_file, "delta-s00000.rrs");
        assert_eq!(d.epoch, 7);
        assert_eq!(d.doc_id_lo, 4_000_000_000);
        assert!(d.has_tombstone());
    }

    #[test]
    fn summaries_round_trip_through_blob() {
        // One split carries opaque summary bytes (the reserved TLV region); the other none.
        let summary = vec![1u8, 4, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef]; // tag=1,len=4,bytes
        let splits = vec![
            SplitSpec {
                summary: summary.clone(),
                ..spec("corpus-s00000.rrs", 0, 0, 65_535, 30 << 20)
            },
            spec("corpus-s00001.rrs", 1, 65_536, 130_000, 20 << 20),
        ];
        let mut config = tiered(2, 2);
        config.flags = FLAG_BLOOM;

        let ss = build(&splits, &config);
        assert_eq!(ss.flags() & FLAG_BLOOM, FLAG_BLOOM);
        assert_eq!(ss.summary(&ss.splits()[0]), Some(summary.as_slice()));
        assert_eq!(ss.summary(&ss.splits()[1]), None);
    }

    #[test]
    fn empty_manifest_round_trips() {
        let ss = build(&[], &tiered(0, 0));
        assert!(ss.splits().is_empty());
        assert_eq!(ss.base_count(), 0);
        assert_eq!(ss.total_byte_size(), 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let bogus = MemoryFetch::new(vec![0u8; HEADER_SIZE]);
        assert!(matches!(
            block_on(SplitSet::open(bogus)),
            Err(IndexError::BadMagic(_))
        ));
    }

    #[test]
    fn rejects_bad_version() {
        let splits = vec![spec("a.rrs", 0, 0, 1, 4)];
        let mut buf = Vec::new();
        write_splitset(&mut buf, &splits, &tiered(1, 1)).unwrap();
        buf[4..6].copy_from_slice(&999u16.to_le_bytes());
        assert!(matches!(
            block_on(SplitSet::open(MemoryFetch::new(buf))),
            Err(IndexError::BadVersion(999))
        ));
    }

    #[test]
    fn policy_round_trips_through_u8() {
        for p in [Policy::Tiered, Policy::StableKey] {
            assert_eq!(Policy::from_u8(p.to_u8()), Some(p));
        }
        assert_eq!(Policy::from_u8(2), None);
    }

    #[test]
    fn malformed_inputs_error_without_panic() {
        let splits = vec![spec("a.rrs", 0, 0, 1, 4), spec("b.rrs", 1, 2, 3, 4)];
        let mut buf = Vec::new();
        write_splitset(&mut buf, &splits, &tiered(2, 2)).unwrap();

        // Truncating past the declared section sizes must error, not read out of bounds.
        let truncated = buf[..buf.len() - 3].to_vec();
        assert!(block_on(SplitSet::open(MemoryFetch::new(truncated))).is_err());

        // A header whose splitCount the buffer cannot satisfy errors on the short body.
        let mut hdr = buf[..HEADER_SIZE].to_vec();
        hdr[12..16].copy_from_slice(&1000u32.to_le_bytes());
        assert!(block_on(SplitSet::open(MemoryFetch::new(hdr))).is_err());

        // base_count > split_count is rejected.
        let mut bad = buf.clone();
        bad[16..20].copy_from_slice(&9u32.to_le_bytes());
        assert!(matches!(
            block_on(SplitSet::open(MemoryFetch::new(bad))),
            Err(IndexError::Malformed(_))
        ));
    }

    // ---- End-to-end: SplitSetBuilder (step 2) → SplitSet::search (step 3) ----

    use crate::build::{write_sortcols, ColumnValues, SortColumn};
    use crate::splitset_build::{BuiltSplitSet, SplitBuildConfig, SplitSetBuilder};
    use std::collections::HashMap;

    /// A [`SplitFetcher`] over an in-memory name→bytes map (the split blobs the builder
    /// emitted, plus any sort-column store), returning a [`MemoryFetch`] per file.
    struct MapResolver(HashMap<String, Vec<u8>>);

    impl SplitFetcher for MapResolver {
        type Fetch = MemoryFetch;
        fn fetch_named(&self, name: &str) -> MemoryFetch {
            MemoryFetch::new(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    /// A resolver over a built split set plus extra named files (e.g. the rank `RRSC`).
    fn resolver_from(built: &BuiltSplitSet, extra: &[(&str, Vec<u8>)]) -> MapResolver {
        let mut m = HashMap::new();
        for (name, bytes) in &built.splits {
            m.insert(name.clone(), bytes.clone());
        }
        for (name, bytes) in extra {
            m.insert((*name).to_string(), bytes.clone());
        }
        MapResolver(m)
    }

    fn open_built(built: &BuiltSplitSet) -> SplitSet {
        block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap()
    }

    #[test]
    fn tiered_build_query_returns_global_rank_order_across_splits() {
        // 30 docs all containing "abc" (so the query matches every doc), each with a unique
        // token so distinct trigrams accumulate and a small byte cap forces several splits.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 4096,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        let n = 60u32;
        for i in 0..n {
            // Fed in rank order, so doc i has global id i == rank i.
            assert_eq!(b.add_text(&format!("abc tok{i:04}")).unwrap(), i);
        }
        let built = b.finish().unwrap();
        assert!(
            built.splits.len() > 1,
            "byte cap should force multiple splits"
        );

        let ss = open_built(&built);
        assert_eq!(ss.policy(), Policy::Tiered);
        assert_eq!(ss.tier_count() as usize, built.splits.len());
        // Every split sealed at or under the cap (the estimate is an upper bound).
        assert!(ss.splits().iter().all(|s| s.byte_size <= ss.byte_cap()));
        // Splits hold disjoint, increasing global id ranges that tile [0, n).
        assert_eq!(ss.splits()[0].doc_id_lo, 0);
        assert_eq!(ss.splits().last().unwrap().doc_id_hi, n - 1);

        let resolver = resolver_from(&built, &[]);
        // Top-3 are the three highest-ranked docs (global ids 0,1,2) — from tier 0 only.
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 3)).unwrap(),
            vec![0, 1, 2]
        );
        // A large page returns every match in ascending global (rank) order.
        let all = block_on(ss.search(&resolver, "abc", 1000)).unwrap();
        assert_eq!(all, (0..n).collect::<Vec<u32>>());
        // An absent term yields nothing.
        assert!(block_on(ss.search(&resolver, "zzz9999", 10))
            .unwrap()
            .is_empty());
    }

    #[cfg(feature = "terms")]
    #[test]
    fn term_bodied_tiered_build_query_returns_global_rank_order() {
        use crate::splitset_build::{TermSplitBuildConfig, TermSplitSetBuilder};
        // 60 docs all containing the token "abc" (matches every doc), each with a unique token so
        // distinct terms accumulate and a small cap forces several RRTI splits.
        let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 600,
            head_boundary: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            language: None,
            stopwords: false,
        });
        let n = 60u32;
        for i in 0..n {
            // Fed in rank order, so doc i has global id i == rank i.
            assert_eq!(b.add_text(&format!("abc tok{i:04}")).unwrap(), i);
        }
        let built = b.finish().unwrap();
        assert!(
            built.splits.len() > 1,
            "byte cap should force multiple splits"
        );
        // Term split data files use the `.rrt` extension; the manifest records the term body kind.
        assert!(built.splits.iter().all(|(name, _)| name.ends_with(".rrt")));

        let ss = open_built(&built);
        assert_eq!(ss.policy(), Policy::Tiered);
        assert_eq!(ss.body_kind(), BODY_KIND_TERM);
        assert_eq!(ss.tier_count() as usize, built.splits.len());
        assert_eq!(ss.splits()[0].doc_id_lo, 0);
        assert_eq!(ss.splits().last().unwrap().doc_id_hi, n - 1);

        let resolver = resolver_from(&built, &[]);
        // Top-3 are the three highest-ranked docs (global ids 0,1,2) — from tier 0 only.
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 3)).unwrap(),
            vec![0, 1, 2]
        );
        // A large page returns every match in ascending global (rank) order.
        let all = block_on(ss.search(&resolver, "abc", 1000)).unwrap();
        assert_eq!(all, (0..n).collect::<Vec<u32>>());
        // A unique token resolves to exactly its doc — whole-token matching, not trigram.
        assert_eq!(
            block_on(ss.search(&resolver, "tok0007", 10)).unwrap(),
            vec![7]
        );
        // An absent term yields nothing.
        assert!(block_on(ss.search(&resolver, "zzz9999", 10))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stable_key_build_query_ranks_by_sortcol() {
        // Six docs all matching "abc"; a small cap splits them across two splits so the
        // cross-split merge runs. Rank comes from an RRSC the manifest names.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::StableKey,
            byte_cap: 300,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: Some(SortColSpec {
                name: "ranks.rrsc".to_string(),
                column: 0,
                descending: true,
            }),
            bloom_bits_per_key: 0,
        });
        // All six match "abc"; distinct tokens grow the vocabulary so the cap splits them.
        for i in 0..6u32 {
            b.add_text(&format!("abc tok{i}")).unwrap();
        }
        let built = b.finish().unwrap();
        assert!(built.splits.len() > 1, "cap should split the six docs");

        // Rank scores keyed by global id 0..6; higher = better (descending).
        let scores = vec![10u32, 60, 30, 50, 20, 40];
        let mut rrsc = Vec::new();
        write_sortcols(
            &mut rrsc,
            vec![SortColumn {
                name: "rank".to_string(),
                values: ColumnValues::U32(scores),
            }],
        )
        .unwrap();

        let ss = open_built(&built);
        assert_eq!(ss.policy(), Policy::StableKey);
        assert_eq!(ss.tier_count(), 0);
        let resolver = resolver_from(&built, &[("ranks.rrsc", rrsc)]);
        // Top-3 by score desc: 60(id1), 50(id3), 40(id5).
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 3)).unwrap(),
            vec![1, 3, 5]
        );
        // Full order: 60,50,40,30,20,10 -> ids 1,3,5,2,4,0.
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 100)).unwrap(),
            vec![1, 3, 5, 2, 4, 0]
        );
    }

    #[test]
    fn builder_rejects_single_doc_over_cap() {
        // One document whose postings alone exceed a tiny cap is a degenerate corpus.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 10,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        b.add_text("abcdefghij").unwrap();
        assert!(b.finish().is_err());
    }

    #[test]
    fn keyword_less_doc_still_consumes_an_id() {
        // A doc too short for any trigram keeps the global id space dense (alignment with
        // records/facets), but never appears in results.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 1 << 20,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        assert_eq!(b.add_text("abc").unwrap(), 0);
        assert_eq!(b.add_text("xy").unwrap(), 1); // too short -> no trigram, id 1 consumed
        assert_eq!(b.add_text("abc").unwrap(), 2);
        assert_eq!(b.doc_count(), 3);
        let built = b.finish().unwrap();
        let ss = open_built(&built);
        let resolver = resolver_from(&built, &[]);
        // "abc" matches docs 0 and 2 only; the keyword-less doc 1 is absent.
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 10)).unwrap(),
            vec![0, 2]
        );
    }

    /// A [`SplitFetcher`] that counts how many splits a query opens — to prove Bloom pruning
    /// skips splits without a fetch.
    struct CountingResolver {
        files: HashMap<String, Vec<u8>>,
        opens: std::cell::Cell<usize>,
    }

    impl SplitFetcher for CountingResolver {
        type Fetch = MemoryFetch;
        fn fetch_named(&self, name: &str) -> MemoryFetch {
            self.opens.set(self.opens.get() + 1);
            MemoryFetch::new(self.files.get(name).cloned().unwrap_or_default())
        }
    }

    /// A [`SplitFetcher`] carrying an optional global-Bloom sidecar name.
    struct GlobalBloomResolver {
        files: HashMap<String, Vec<u8>>,
        opens: std::cell::Cell<usize>,
        bloom: Option<String>,
    }

    impl SplitFetcher for GlobalBloomResolver {
        type Fetch = MemoryFetch;
        fn fetch_named(&self, name: &str) -> MemoryFetch {
            self.opens.set(self.opens.get() + 1);
            MemoryFetch::new(self.files.get(name).cloned().unwrap_or_default())
        }
        fn global_bloom_name(&self) -> Option<String> {
            self.bloom.clone()
        }
    }

    #[test]
    fn remote_bloom_probes_without_downloading() {
        let keys: Vec<u64> = (0..500u64).map(|i| i * 7 + 3).collect();
        let bloom = bloom_build(&keys, 10);
        let r = RemoteBloom {
            fetch: MemoryFetch::new(bloom.clone()),
            k: read_u32(&bloom, 0),
            nbits: read_u32(&bloom, 4) as u64,
        };
        assert!(block_on(r.contains_all(&[3, 10, 24])).unwrap());
        // An inserted-keys-only conjunction is possibly-present; adding one
        // definitely-absent key flips the whole strict AND to false.
        assert!(!block_on(r.contains_all(&[3, 10, 1_000_003])).unwrap());
        // open() round-trips the header.
        let opened = block_on(RemoteBloom::open(MemoryFetch::new(bloom))).unwrap();
        assert!(block_on(opened.contains_all(&keys)).unwrap());
    }

    #[test]
    fn global_bloom_ends_the_descent_for_absent_terms() {
        // A summary-less (stripped-manifest-style) set: no per-split Blooms, so
        // an absent term would otherwise descend through every split.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 4096,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        let mut vocab = std::collections::BTreeSet::new();
        for i in 0..200u32 {
            let text = format!("abc tok{i:04}");
            vocab.extend(ngram_keys(&text, 3));
            b.add_text(&text).unwrap();
        }
        let built = b.finish().unwrap();
        assert!(built.splits.len() > 2, "need several splits for a descent");
        let ss = open_built(&built);
        let keys: Vec<u64> = vocab.into_iter().collect();
        let mut files: HashMap<String, Vec<u8>> = built.splits.iter().cloned().collect();
        files.insert("global.bloom".to_string(), bloom_build(&keys, 10));
        let resolver = |bloom: Option<&str>| GlobalBloomResolver {
            files: files.clone(),
            opens: std::cell::Cell::new(0),
            bloom: bloom.map(String::from),
        };

        // Absent term, no global Bloom: every split opens before giving up.
        let r = resolver(None);
        assert!(block_on(ss.search(&r, "zzzqqq", 10)).unwrap().is_empty());
        assert_eq!(r.opens.get(), ss.splits().len());

        // Absent term, with the global Bloom: the top split opens, the probe answers
        // definitively absent, and the descent ends — one split + one Bloom fetch.
        let r = resolver(Some("global.bloom"));
        assert!(block_on(ss.search(&r, "zzzqqq", 10)).unwrap().is_empty());
        assert_eq!(
            r.opens.get(),
            2,
            "first split + the Bloom probe, nothing else"
        );

        // A common present term fills from the top tiers: the probe never runs and
        // results are identical to the no-Bloom path.
        let want = block_on(ss.search(&resolver(None), "abc", 100)).unwrap();
        let got = block_on(ss.search(&resolver(Some("global.bloom")), "abc", 100)).unwrap();
        assert_eq!(got, want);

        // A rare-but-present term (only in a deep split): the probe runs once (top
        // tier empty), says possibly-present, and the descent continues to find it.
        let want = block_on(ss.search(&resolver(None), "tok0199", 10)).unwrap();
        assert_eq!(want.len(), 1);
        let got = block_on(ss.search(&resolver(Some("global.bloom")), "tok0199", 10)).unwrap();
        assert_eq!(got, want);

        // A configured-but-missing sidecar must never break search.
        let r = resolver(Some("nope.bloom"));
        let got = block_on(ss.search(&r, "tok0199", 10)).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn bloom_prunes_splits_without_the_term() {
        // Same 60-doc corpus built twice — without and with per-split Bloom filters.
        let build_ss = |bloom: u32| {
            let mut b = SplitSetBuilder::new(SplitBuildConfig {
                byte_cap_max: 0,
                policy: Policy::Tiered,
                byte_cap: 4096,
                gram_size: 3,
                head_boundary: 0,
                stride: 0,
                name_prefix: "corpus".to_string(),
                sortcol: None,
                bloom_bits_per_key: bloom,
            });
            for i in 0..60u32 {
                b.add_text(&format!("abc tok{i:04}")).unwrap();
            }
            b.finish().unwrap()
        };
        let plain = build_ss(0);
        let bloomed = build_ss(10);
        let files =
            |b: &BuiltSplitSet| -> HashMap<String, Vec<u8>> { b.splits.iter().cloned().collect() };
        let plain_ss = open_built(&plain);
        let bloom_ss = open_built(&bloomed);
        assert_eq!(bloom_ss.flags() & FLAG_BLOOM, FLAG_BLOOM);
        assert_eq!(plain_ss.flags() & FLAG_BLOOM, 0);

        // An absent term: without Bloom the tiered scan can never fill k, so it opens *every*
        // base split; with Bloom every split is pruned, so it opens none — and both return [].
        let q = "zzzqqq";
        let plain_r = CountingResolver {
            files: files(&plain),
            opens: std::cell::Cell::new(0),
        };
        let bloom_r = CountingResolver {
            files: files(&bloomed),
            opens: std::cell::Cell::new(0),
        };
        assert!(block_on(plain_ss.search(&plain_r, q, 10))
            .unwrap()
            .is_empty());
        assert!(block_on(bloom_ss.search(&bloom_r, q, 10))
            .unwrap()
            .is_empty());
        assert_eq!(plain_r.opens.get(), plain_ss.splits().len());
        assert_eq!(bloom_r.opens.get(), 0, "Bloom should skip every split");

        // A present term: results must be identical with and without Bloom, and Bloom must not
        // open more splits than the plain scan.
        let q = "abc";
        let plain_r = CountingResolver {
            files: files(&plain),
            opens: std::cell::Cell::new(0),
        };
        let bloom_r = CountingResolver {
            files: files(&bloomed),
            opens: std::cell::Cell::new(0),
        };
        let plain_hits = block_on(plain_ss.search(&plain_r, q, 5)).unwrap();
        let bloom_hits = block_on(bloom_ss.search(&bloom_r, q, 5)).unwrap();
        assert_eq!(plain_hits, vec![0, 1, 2, 3, 4]);
        assert_eq!(bloom_hits, plain_hits);
        assert!(bloom_r.opens.get() <= plain_r.opens.get());
    }

    /// A [`RangeFetch`] over resident bytes that counts the reads it serves (shared counter so
    /// clones tally together).
    #[derive(Clone)]
    struct ReadCountFetch {
        bytes: std::rc::Rc<Vec<u8>>,
        reads: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl RangeFetch for ReadCountFetch {
        async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, crate::FetchError> {
            self.reads.set(self.reads.get() + 1);
            let (s, e) = (offset as usize, offset as usize + len);
            Ok(self.bytes[s..e].to_vec())
        }
    }

    /// A resolver that counts reads and optionally supplies each split's resident boot bytes
    /// (the whole split file — `from_boot` reads only the header+sparse prefix).
    struct BootResolver {
        files: HashMap<String, std::rc::Rc<Vec<u8>>>,
        reads: std::rc::Rc<std::cell::Cell<usize>>,
        supply_boot: bool,
    }

    impl SplitFetcher for BootResolver {
        type Fetch = ReadCountFetch;
        fn fetch_named(&self, name: &str) -> ReadCountFetch {
            ReadCountFetch {
                bytes: self
                    .files
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| std::rc::Rc::new(Vec::new())),
                reads: std::rc::Rc::clone(&self.reads),
            }
        }
        fn boot(&self, split: &Split) -> Option<Vec<u8>> {
            self.supply_boot
                .then(|| self.files.get(&split.data_file).map(|b| b.to_vec()))
                .flatten()
        }
    }

    #[test]
    fn inlined_boot_avoids_split_open_fetches() {
        // 30 docs all matching "abc", small cap -> several tiered splits (no Bloom here).
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 2048,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        for i in 0..30u32 {
            b.add_text(&format!("abc tok{i:04}")).unwrap();
        }
        let built = b.finish().unwrap();
        let ss = open_built(&built);
        let files: HashMap<String, std::rc::Rc<Vec<u8>>> = built
            .splits
            .iter()
            .map(|(n, by)| (n.clone(), std::rc::Rc::new(by.clone())))
            .collect();

        // Same top-3 query, once opening splits by fetch and once from inlined boots.
        let run = |supply_boot: bool| -> (Vec<u32>, usize) {
            let reads = std::rc::Rc::new(std::cell::Cell::new(0));
            let r = BootResolver {
                files: files.clone(),
                reads: std::rc::Rc::clone(&reads),
                supply_boot,
            };
            let hits = block_on(ss.search(&r, "abc", 3)).unwrap();
            (hits, reads.get())
        };
        let (fetched_hits, fetched_reads) = run(false);
        let (boot_hits, boot_reads) = run(true);

        assert_eq!(fetched_hits, vec![0, 1, 2]);
        assert_eq!(
            boot_hits, fetched_hits,
            "inlined boot must not change results"
        );
        // Inlined boots skip the header+sparse read per opened split, so strictly fewer reads.
        assert!(
            boot_reads < fetched_reads,
            "inlined boot ({boot_reads}) should do fewer reads than fetched ({fetched_reads})"
        );
    }

    #[test]
    fn facet_filtered_search_prunes_and_filters() {
        // 4 docs all matching "abc"; a small cap puts the two "en" docs in split 0 and the two
        // "fr" docs in split 1, so a facet filter prunes the split that lacks the category.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 250,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 8,
        });
        let f = |lang: &str| vec![("lang".to_string(), lang.to_string())];
        b.add_faceted("abc en0", &f("en")).unwrap();
        b.add_faceted("abc en1", &f("en")).unwrap();
        b.add_faceted("abc fr0", &f("fr")).unwrap();
        b.add_faceted("abc fr1", &f("fr")).unwrap();
        let built = b.finish().unwrap();
        assert_eq!(built.splits.len(), 2, "cap should split en|fr");
        assert_eq!(built.facets.len(), 2, "one RRSF per split");

        let ss = open_built(&built);
        assert_eq!(ss.flags() & FLAG_FACET, FLAG_FACET);

        // Resolver serving both the .rrs splits and their .rrf facet sidecars, counting opens.
        let files: HashMap<String, Vec<u8>> = built
            .splits
            .iter()
            .chain(built.facets.iter())
            .cloned()
            .collect();
        let resolver = |opens: usize| CountingResolver {
            files: files.clone(),
            opens: std::cell::Cell::new(opens),
        };

        // lang=en → only the two en docs; the fr split is pruned by facet presence.
        let r = resolver(0);
        assert_eq!(
            block_on(ss.search_filtered(&r, "abc", &f("en"), 10)).unwrap(),
            vec![0, 1]
        );
        assert_eq!(r.opens.get(), 2, "only split 0 opened (its .rrs + .rrf)");

        // lang=fr → the other two; the en split is pruned.
        assert_eq!(
            block_on(ss.search_filtered(&resolver(0), "abc", &f("fr"), 10)).unwrap(),
            vec![2, 3]
        );

        // An absent category prunes every split — zero opens, empty result.
        let r = resolver(0);
        assert!(block_on(ss.search_filtered(&r, "abc", &f("de"), 10))
            .unwrap()
            .is_empty());
        assert_eq!(r.opens.get(), 0, "an absent category opens no splits");

        // An empty filter is exactly the unfiltered search.
        assert_eq!(
            block_on(ss.search_filtered(&resolver(0), "abc", &[], 10)).unwrap(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn facet_filtered_search_survives_stripped_summaries() {
        // The splitset_strip_summaries flow: the same faceted fixture, but the manifest is
        // re-emitted with every per-split summary dropped and FLAG_FACET cleared. Without the
        // summaries no split may be facet-pruned — a missing facet TLV must read as "no
        // information", not "no facets" — and the filter must resolve via each split's .rrf.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 250,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 8,
        });
        let f = |lang: &str| vec![("lang".to_string(), lang.to_string())];
        b.add_faceted("abc en0", &f("en")).unwrap();
        b.add_faceted("abc en1", &f("en")).unwrap();
        b.add_faceted("abc fr0", &f("fr")).unwrap();
        b.add_faceted("abc fr1", &f("fr")).unwrap();
        let built = b.finish().unwrap();
        let full = open_built(&built);

        let stripped_specs: Vec<SplitSpec> = full
            .splits()
            .iter()
            .map(|s| SplitSpec {
                data_file: s.data_file.clone(),
                tier: s.tier,
                doc_count: s.doc_count,
                doc_id_lo: s.doc_id_lo,
                doc_id_hi: s.doc_id_hi,
                epoch: s.epoch,
                byte_size: s.byte_size,
                flags: 0,
                summary: Vec::new(),
            })
            .collect();
        let config = SplitSetConfig {
            policy: full.policy(),
            tier_count: full.tier_count(),
            base_count: full.base_count(),
            byte_cap: full.byte_cap(),
            gram_size: full.gram_size(),
            body_kind: full.body_kind(),
            sortcol: None,
            flags: 0,
        };
        let mut manifest = Vec::new();
        write_splitset(&mut manifest, &stripped_specs, &config).unwrap();
        let ss = block_on(SplitSet::open(MemoryFetch::new(manifest))).unwrap();
        assert_eq!(ss.flags() & FLAG_FACET, 0);

        let files: HashMap<String, Vec<u8>> = built
            .splits
            .iter()
            .chain(built.facets.iter())
            .cloned()
            .collect();
        let resolver = || CountingResolver {
            files: files.clone(),
            opens: std::cell::Cell::new(0),
        };

        // The filtered results must match the summarized manifest's exactly.
        assert_eq!(
            block_on(ss.search_filtered(&resolver(), "abc", &f("en"), 10)).unwrap(),
            vec![0, 1]
        );
        assert_eq!(
            block_on(ss.search_filtered(&resolver(), "abc", &f("fr"), 10)).unwrap(),
            vec![2, 3]
        );

        // An absent category now costs .rrf reads (no pruning possible) but stays empty.
        let r = resolver();
        assert!(block_on(ss.search_filtered(&r, "abc", &f("de"), 10))
            .unwrap()
            .is_empty());
        assert!(
            r.opens.get() > 0,
            "without summaries every split is consulted"
        );
    }

    #[test]
    fn facet_counts_aggregate_across_splits_by_name() {
        // Same fixture: two "en" docs in split 0, two "fr" docs in split 1, each with its own
        // .rrf. facet_counts must sum each split's per-category counts into one name-keyed result.
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 250,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 8,
        });
        let f = |lang: &str| vec![("lang".to_string(), lang.to_string())];
        b.add_faceted("abc en0", &f("en")).unwrap();
        b.add_faceted("abc en1", &f("en")).unwrap();
        b.add_faceted("abc fr0", &f("fr")).unwrap();
        b.add_faceted("abc fr1", &f("fr")).unwrap();
        let built = b.finish().unwrap();
        assert_eq!(built.splits.len(), 2);

        let files: HashMap<String, Vec<u8>> = built
            .splits
            .iter()
            .chain(built.facets.iter())
            .cloned()
            .collect();
        let resolver = MapResolver(files);
        let ss = open_built(&built);

        let lang_of = |counts: &[FieldCounts]| -> Vec<(String, u64)> {
            counts
                .iter()
                .find(|fc| fc.field == "lang")
                .map(|fc| fc.categories.clone())
                .unwrap_or_default()
        };

        // Full result set spans both splits: en=2 (split 0) + fr=2 (split 1).
        let counts = block_on(ss.facet_counts(&resolver, &[0, 1, 2, 3])).unwrap();
        let m: HashMap<String, u64> = lang_of(&counts).into_iter().collect();
        assert_eq!(m.get("en"), Some(&2));
        assert_eq!(m.get("fr"), Some(&2));

        // Subset in split 0 only: fr is omitted (zero counts dropped).
        let counts = block_on(ss.facet_counts(&resolver, &[0, 1])).unwrap();
        assert_eq!(lang_of(&counts), vec![("en".to_string(), 2)]);

        // A single fr id -> fr=1.
        let counts = block_on(ss.facet_counts(&resolver, &[2])).unwrap();
        assert_eq!(lang_of(&counts), vec![("fr".to_string(), 1)]);

        // No ids -> no fields.
        assert!(block_on(ss.facet_counts(&resolver, &[]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn streaming_drain_equals_whole_build() {
        // Streaming a large build (drain after each add → write+free) must produce byte-for-byte
        // the same splits, facets, and manifest as accumulating the whole set in RAM.
        let docs: Vec<String> = (0..40)
            .map(|i| format!("abc topic{} tok{i:03}", i % 5))
            .collect();
        let cfg = || SplitBuildConfig {
            byte_cap_max: 0,
            policy: Policy::Tiered,
            byte_cap: 2048,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 10,
        };

        let mut whole = SplitSetBuilder::new(cfg());
        for (i, d) in docs.iter().enumerate() {
            whole
                .add_faceted(d, &[("g".to_string(), format!("{}", i % 3))])
                .unwrap();
        }
        let whole = whole.finish().unwrap();
        assert!(whole.splits.len() > 1);

        let mut stream = SplitSetBuilder::new(cfg());
        let mut splits = Vec::new();
        let mut facets = Vec::new();
        for (i, d) in docs.iter().enumerate() {
            stream
                .add_faceted(d, &[("g".to_string(), format!("{}", i % 3))])
                .unwrap();
            let (s, f) = stream.drain_sealed();
            splits.extend(s);
            facets.extend(f);
        }
        let tail = stream.finish().unwrap();
        splits.extend(tail.splits);
        facets.extend(tail.facets);

        assert_eq!(tail.manifest, whole.manifest, "manifests differ");
        assert_eq!(splits, whole.splits, "streamed splits differ");
        assert_eq!(facets, whole.facets, "streamed facet sidecars differ");
    }
}
