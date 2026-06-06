//! Native ingestion writer for the `RRSS` split set — the pure builder API (`SPLITSET.md`
//! §writer, task steps 4 & 7). Excluded from the wasm reader build.
//!
//! [`SplitSetWriter`] is **pure**: bytes in, bytes out, no I/O, threads, queue, or scheduler.
//! The library owns the *mechanism* (memtable accumulation, immutable-split serialization,
//! doc-id allocation, manifest cutover, supersession, compaction); the client owns the
//! *policy* (transport, durability, when to flush/compact, where the bytes land, and
//! single-writer discipline). A client loop is tiny:
//!
//! ```text
//! let mut w = SplitSetWriter::resume(&prev_manifest, gram, hb, stride, prefix);
//! for msg in source.poll() { w.add_text(msg.text); }
//! if w.memtable_bytes() > CAP || timer.elapsed() > INTERVAL {
//!     let f = w.flush()?.unwrap();
//!     put(&f.split_name, &f.split_bytes);  // client I/O
//!     put(MANIFEST_KEY, &f.manifest);      // client I/O — atomic cutover
//!     source.ack();                        // client durability
//! }
//! ```
//!
//! **Doc IDs.** A flush seals the memtable into one immutable **L0 `RRS` delta** with local
//! 0-based ids based at a fresh high global id (`docIdLo`); fresh docs have no rank, so a
//! delta is always stable-key / ingest-ordered. Rank-tiering happens only when the base is
//! rebuilt. **Supersession.** [`delete`](SplitSetWriter::delete) records a tombstone (a
//! superseded global id) carried in the flushed split's summary; the reader masks those base
//! docs at query time. **Compaction.** [`compact`](SplitSetWriter::compact) merges L0 delta
//! splits into one **absolute-id** split (ids stay stable — no renumber), dropping tombstoned
//! docs and bounding read fan-out; re-tiering the base is a full rebuild via
//! [`crate::splitset_build::SplitSetBuilder`].

use crate::build::{split_posting, write_index, DEFAULT_HEAD_BOUNDARY, DEFAULT_STRIDE};
use crate::index::{deserialize, read_u32, read_u64, IndexError};
use crate::ngram::ngram_keys;
use crate::splitset::{
    bloom_build, tlv_record, Policy, SplitSet, FLAG_BLOOM, FLAG_TOMBSTONES,
    SPLIT_FLAG_ABSOLUTE_IDS, SPLIT_FLAG_HAS_TOMBSTONE, SUMMARY_TAG_BLOOM, SUMMARY_TAG_TOMBSTONE,
};
use crate::splitset_build::{write_splitset, SortColSpec, SplitSetConfig, SplitSpec};
use roaring::RoaringBitmap;
use std::collections::{BTreeMap, HashSet};
use std::io;

/// Configuration for a fresh [`SplitSetWriter`] (no prior manifest).
#[derive(Clone)]
pub struct WriterConfig {
    /// N-gram window the delta `RRS` splits are built with (must match the base).
    pub gram_size: u16,
    /// Doc-ID head/tail split for delta splits (`0` → [`DEFAULT_HEAD_BOUNDARY`]).
    pub head_boundary: u32,
    /// Sparse-index stride for delta splits (`0` → [`DEFAULT_STRIDE`]).
    pub stride: u32,
    /// The per-split byte cap recorded in the manifest (informational).
    pub byte_cap: u64,
    /// Filename prefix for emitted splits — `‹prefix›-d00000.rrs` (flush) /
    /// `‹prefix›-c00000.rrs` (compaction).
    pub name_prefix: String,
    /// The base policy recorded in the manifest header.
    pub policy: Policy,
    /// The base tier count recorded in the manifest header (`0` for stable-key).
    pub tier_count: u16,
    /// The stable-key rank source recorded in the manifest header, if any.
    pub sortcol: Option<SortColSpec>,
    /// Bits per key for the per-split term Bloom filter on flushed/compacted splits (`0`
    /// disables; `~10` ≈ 1% false positives), matching the batch builder's option.
    pub bloom_bits_per_key: u32,
}

/// The result of [`SplitSetWriter::flush`]: one immutable delta split and the new manifest.
/// The client PUTs the split, then the manifest (the atomic cutover), then acks its source.
pub struct FlushOutput {
    /// The delta split's filename.
    pub split_name: String,
    /// The delta split's `RRS` bytes.
    pub split_bytes: Vec<u8>,
    /// The new `RRSS` manifest bytes (points at the base + every delta incl. this one).
    pub manifest: Vec<u8>,
}

/// The result of [`SplitSetWriter::compact`]: the merged split, the new manifest, and the
/// input split names it supersedes (the client may delete those objects after the cutover).
pub struct CompactOutput {
    /// The merged (absolute-id) split's filename.
    pub split_name: String,
    /// The merged split's `RRS` bytes.
    pub split_bytes: Vec<u8>,
    /// The new `RRSS` manifest bytes (inputs replaced by the merged split).
    pub manifest: Vec<u8>,
    /// The input split filenames now superseded — safe for the client to delete post-cutover.
    pub removed: Vec<String>,
}

/// A pure, resumable split-set ingestion writer. See the module docs for the lifecycle.
pub struct SplitSetWriter {
    gram_size: u16,
    head_boundary: u32,
    stride: u32,
    byte_cap: u64,
    name_prefix: String,
    policy: Policy,
    tier_count: u16,
    sortcol: Option<SortColSpec>,
    bloom_bits_per_key: u32,
    /// Carried split metadata: base splits `[0, base_count)` then deltas, re-emitted each cutover.
    base_count: usize,
    specs: Vec<SplitSpec>,
    next_global_id: u32,
    next_epoch: u64,
    flush_seq: u32,
    compact_seq: u32,
    /// The open memtable: n-gram → bitmap of memtable-local 0-based doc ids.
    memtable: BTreeMap<u64, RoaringBitmap>,
    /// Global id of the memtable's first doc (its eventual split `docIdLo`).
    memtable_base: u32,
    /// Docs accumulated in the open memtable.
    memtable_count: u32,
    /// Global ids deleted since the last flush — tombstoned on the next flush.
    pending_deletes: RoaringBitmap,
}

impl SplitSetWriter {
    /// Creates a fresh writer with no prior splits (the base is empty; the first flush writes
    /// the first delta). `config.head_boundary`/`stride` of `0` take the `RRS` defaults.
    pub fn new(config: WriterConfig) -> Self {
        SplitSetWriter {
            gram_size: config.gram_size,
            head_boundary: nonzero_or(config.head_boundary, DEFAULT_HEAD_BOUNDARY),
            stride: nonzero_or(config.stride, DEFAULT_STRIDE),
            byte_cap: config.byte_cap,
            name_prefix: config.name_prefix,
            policy: config.policy,
            tier_count: config.tier_count,
            sortcol: config.sortcol,
            bloom_bits_per_key: config.bloom_bits_per_key,
            base_count: 0,
            specs: Vec::new(),
            next_global_id: 0,
            next_epoch: 1,
            flush_seq: 0,
            compact_seq: 0,
            memtable: BTreeMap::new(),
            memtable_base: 0,
            memtable_count: 0,
            pending_deletes: RoaringBitmap::new(),
        }
    }

    /// Resumes over an existing split set: carries forward every split's metadata (so the next
    /// cutover re-emits a complete manifest), continues the global id space past the highest
    /// id, and advances the epoch past the latest. The base policy, tier count, byte cap, and
    /// sort-column come from `prev`; `gram_size`/`head_boundary`/`stride`/`bloom_bits_per_key`
    /// (which the manifest does not fully record per split) are supplied to match the base.
    pub fn resume(
        prev: &SplitSet,
        gram_size: u16,
        head_boundary: u32,
        stride: u32,
        name_prefix: String,
        bloom_bits_per_key: u32,
    ) -> Self {
        let specs: Vec<SplitSpec> = prev
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
                flags: s.flags,
                summary: prev.summary(s).map(<[u8]>::to_vec).unwrap_or_default(),
            })
            .collect();
        let next_global_id = prev
            .splits()
            .iter()
            .map(|s| s.doc_id_hi.saturating_add(1))
            .max()
            .unwrap_or(0);
        let next_epoch = prev.splits().iter().map(|s| s.epoch).max().unwrap_or(0) + 1;
        SplitSetWriter {
            gram_size,
            head_boundary: nonzero_or(head_boundary, DEFAULT_HEAD_BOUNDARY),
            stride: nonzero_or(stride, DEFAULT_STRIDE),
            byte_cap: prev.byte_cap(),
            name_prefix,
            policy: prev.policy(),
            tier_count: prev.tier_count(),
            sortcol: prev.sortcol().map(|d| SortColSpec {
                name: d.name.clone(),
                column: d.column,
                descending: d.descending,
            }),
            bloom_bits_per_key,
            base_count: prev.base_count() as usize,
            specs,
            next_global_id,
            next_epoch,
            flush_seq: 0,
            compact_seq: 0,
            memtable: BTreeMap::new(),
            memtable_base: next_global_id,
            memtable_count: 0,
            pending_deletes: RoaringBitmap::new(),
        }
    }

    /// Tokenizes `text` into n-gram keys and appends it to the memtable — the convenience over
    /// [`add_keys`](Self::add_keys). Returns the doc's global id.
    pub fn add_text(&mut self, text: &str) -> u32 {
        let keys = ngram_keys(text, self.gram_size as usize);
        self.add_keys(&keys)
    }

    /// Appends one document by its (deduplicated) n-gram `keys` to the open memtable, returning
    /// its monotonic global doc id. A keyword-less document still consumes an id so the id space
    /// stays dense.
    pub fn add_keys(&mut self, keys: &[u64]) -> u32 {
        if self.memtable_count == 0 {
            self.memtable_base = self.next_global_id;
        }
        let local = self.memtable_count;
        for &k in keys {
            self.memtable.entry(k).or_default().insert(local);
        }
        self.memtable_count += 1;
        let id = self.next_global_id;
        self.next_global_id += 1;
        id
    }

    /// Records a tombstone for a previously-indexed global doc id (in the base or an earlier
    /// delta). The next [`flush`](Self::flush) carries it; the reader masks the doc thereafter.
    pub fn delete(&mut self, global_id: u32) {
        self.pending_deletes.insert(global_id);
    }

    /// Total documents ever added (the next global id to be handed out).
    pub fn doc_count(&self) -> u32 {
        self.next_global_id
    }

    /// Documents currently buffered in the open memtable (not yet flushed).
    pub fn memtable_doc_count(&self) -> u32 {
        self.memtable_count
    }

    /// An estimate of the open memtable's serialized `RRS` size — the size trigger a client
    /// polls to decide when to flush. Counts the header, per-key dictionary, sparse index, and
    /// each posting's serialized size.
    pub fn memtable_bytes(&self) -> u64 {
        let nkeys = self.memtable.len() as u64;
        let postings: u64 = self
            .memtable
            .values()
            .map(|b| b.serialized_size() as u64 + 8)
            .sum();
        20 + nkeys * 24 + nkeys.div_ceil(self.stride.max(1) as u64) * 8 + postings
    }

    /// Seals the open memtable (and any pending deletes) into one immutable L0 delta `RRS`
    /// split plus a new manifest, returned as bytes. Returns `Ok(None)` when there is nothing
    /// to flush (no adds, no deletes). Resets the memtable and pending deletes and advances the
    /// epoch. A deletes-only flush emits a zero-document split that carries just the tombstone.
    pub fn flush(&mut self) -> io::Result<Option<FlushOutput>> {
        if self.memtable_count == 0 && self.pending_deletes.is_empty() {
            return Ok(None);
        }
        let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = self
            .memtable
            .iter()
            .map(|(k, bm)| {
                let (head, tail) = split_posting(bm, self.head_boundary);
                (*k, head, tail)
            })
            .collect();
        let mut bytes = Vec::new();
        write_index(
            &mut bytes,
            self.gram_size,
            self.stride,
            self.head_boundary,
            entries,
        )?;

        let name = format!("{}-d{:05}.rrs", self.name_prefix, self.flush_seq);
        self.flush_seq += 1;
        let mut flags = 0u16;
        let mut summary = Vec::new();
        if self.bloom_bits_per_key > 0 && self.memtable_count > 0 {
            let keys: Vec<u64> = self.memtable.keys().copied().collect();
            summary.extend_from_slice(&tlv_record(
                SUMMARY_TAG_BLOOM,
                &bloom_build(&keys, self.bloom_bits_per_key),
            ));
        }
        if !self.pending_deletes.is_empty() {
            flags |= SPLIT_FLAG_HAS_TOMBSTONE;
            summary.extend_from_slice(&tombstone_tlv(&self.pending_deletes));
        }
        let (lo, hi) = if self.memtable_count > 0 {
            (
                self.memtable_base,
                self.memtable_base + self.memtable_count - 1,
            )
        } else {
            (self.next_global_id, self.next_global_id) // 0-doc split: nominal range
        };
        self.specs.push(SplitSpec {
            data_file: name.clone(),
            tier: 0,
            doc_count: self.memtable_count,
            doc_id_lo: lo,
            doc_id_hi: hi,
            epoch: self.next_epoch,
            byte_size: bytes.len() as u64,
            flags,
            summary,
        });
        self.next_epoch += 1;
        self.memtable.clear();
        self.memtable_count = 0;
        self.pending_deletes = RoaringBitmap::new();

        let manifest = self.emit_manifest()?;
        Ok(Some(FlushOutput {
            split_name: name,
            split_bytes: bytes,
            manifest,
        }))
    }

    /// Minor compaction: merges the named delta `inputs` (their `RRS` bytes supplied by the
    /// client) into **one** absolute-id split, dropping tombstoned docs, and rewrites the
    /// manifest with the inputs replaced by the merged split. Doc ids are preserved (no
    /// renumber), so records/facets/vectors keyed by the same ids stay valid. Errors if an
    /// input is not a current **delta** split — re-tiering the base is a full rebuild via
    /// [`crate::splitset_build::SplitSetBuilder`], not minor compaction.
    pub fn compact(&mut self, inputs: &[(String, Vec<u8>)]) -> io::Result<CompactOutput> {
        let mut merged: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
        let mut dead = RoaringBitmap::new();
        for (name, bytes) in inputs {
            let idx = self
                .specs
                .iter()
                .position(|s| &s.data_file == name)
                .ok_or_else(|| io::Error::other(format!("compact: {name:?} not in manifest")))?;
            if idx < self.base_count {
                return Err(io::Error::other(
                    "compact: base splits need a full rebuild, not minor compaction",
                ));
            }
            let spec = &self.specs[idx];
            let absolute = spec.flags & SPLIT_FLAG_ABSOLUTE_IDS != 0;
            let base = spec.doc_id_lo;
            if let Some(tb) = parse_tombstone(&spec.summary)? {
                dead |= tb;
            }
            for (key, bm) in read_rrs_entries(bytes)? {
                let remapped: RoaringBitmap = if absolute {
                    bm
                } else {
                    bm.iter().map(|l| base.saturating_add(l)).collect()
                };
                *merged.entry(key).or_default() |= remapped;
            }
        }
        // Physically drop superseded docs, then prune now-empty keys.
        for bm in merged.values_mut() {
            *bm -= &dead;
        }
        merged.retain(|_, bm| !bm.is_empty());

        let mut present = RoaringBitmap::new();
        for bm in merged.values() {
            present |= bm;
        }
        let lo = present.min().unwrap_or(0);
        let hi = present.max().unwrap_or(0);

        let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = merged
            .iter()
            .map(|(k, bm)| {
                let (head, tail) = split_posting(bm, self.head_boundary);
                (*k, head, tail)
            })
            .collect();
        let mut bytes = Vec::new();
        write_index(
            &mut bytes,
            self.gram_size,
            self.stride,
            self.head_boundary,
            entries,
        )?;

        let name = format!("{}-c{:05}.rrs", self.name_prefix, self.compact_seq);
        self.compact_seq += 1;
        let mut flags = SPLIT_FLAG_ABSOLUTE_IDS;
        let mut summary = Vec::new();
        if self.bloom_bits_per_key > 0 && !merged.is_empty() {
            let keys: Vec<u64> = merged.keys().copied().collect();
            summary.extend_from_slice(&tlv_record(
                SUMMARY_TAG_BLOOM,
                &bloom_build(&keys, self.bloom_bits_per_key),
            ));
        }
        if !dead.is_empty() {
            flags |= SPLIT_FLAG_HAS_TOMBSTONE;
            summary.extend_from_slice(&tombstone_tlv(&dead));
        }
        let epoch = inputs
            .iter()
            .filter_map(|(n, _)| self.specs.iter().find(|s| &s.data_file == n))
            .map(|s| s.epoch)
            .max()
            .unwrap_or(self.next_epoch);

        let removed: Vec<String> = inputs.iter().map(|(n, _)| n.clone()).collect();
        let removed_set: HashSet<&String> = inputs.iter().map(|(n, _)| n).collect();
        self.specs.retain(|s| !removed_set.contains(&s.data_file));
        self.specs.push(SplitSpec {
            data_file: name.clone(),
            tier: 0,
            doc_count: present.len() as u32,
            doc_id_lo: lo,
            doc_id_hi: hi,
            epoch,
            byte_size: bytes.len() as u64,
            flags,
            summary,
        });

        let manifest = self.emit_manifest()?;
        Ok(CompactOutput {
            split_name: name,
            split_bytes: bytes,
            manifest,
            removed,
        })
    }

    /// Serializes the current split set into a fresh `RRSS` manifest (the atomic cutover bytes).
    fn emit_manifest(&self) -> io::Result<Vec<u8>> {
        let mut flags = 0u16;
        if self
            .specs
            .iter()
            .any(|s| s.flags & SPLIT_FLAG_HAS_TOMBSTONE != 0)
        {
            flags |= FLAG_TOMBSTONES;
        }
        if self.bloom_bits_per_key > 0 {
            flags |= FLAG_BLOOM;
        }
        let config = SplitSetConfig {
            policy: self.policy,
            tier_count: self.tier_count,
            base_count: self.base_count as u32,
            byte_cap: self.byte_cap,
            gram_size: self.gram_size,
            body_kind: crate::splitset::BODY_KIND_TRIGRAM,
            sortcol: self.sortcol.clone(),
            flags,
        };
        let mut out = Vec::new();
        write_splitset(&mut out, &self.specs, &config)?;
        Ok(out)
    }
}

/// Returns `v` unless it is `0`, in which case `default`.
fn nonzero_or(v: u32, default: u32) -> u32 {
    if v == 0 {
        default
    } else {
        v
    }
}

/// Frames a tombstone posting (superseded global ids) as a `[tag][len u32 LE][bytes]` summary
/// TLV record (`SPLITSET.md` §summary blob, tag `4`).
fn tombstone_tlv(dead: &RoaringBitmap) -> Vec<u8> {
    let mut posting = Vec::with_capacity(dead.serialized_size());
    dead.serialize_into(&mut posting)
        .expect("serialize tombstone");
    let mut tlv = Vec::with_capacity(5 + posting.len());
    tlv.push(SUMMARY_TAG_TOMBSTONE);
    tlv.extend_from_slice(&(posting.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&posting);
    tlv
}

/// Parses a tombstone posting out of a summary TLV byte region, or `None` if it has none.
fn parse_tombstone(summary: &[u8]) -> io::Result<Option<RoaringBitmap>> {
    let mut off = 0usize;
    while off + 5 <= summary.len() {
        let tag = summary[off];
        let len = read_u32(summary, off + 1) as usize;
        let start = off + 5;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= summary.len())
            .ok_or_else(|| io::Error::other("compact: bad summary TLV length"))?;
        if tag == SUMMARY_TAG_TOMBSTONE {
            return deserialize(&summary[start..end]).map(Some).map_err(to_io);
        }
        off = end;
    }
    Ok(None)
}

/// Parses every `(key, full posting)` out of an `RRS` split blob — the all-entries enumerate
/// the query reader does not expose, needed to merge splits during compaction. The blob is the
/// writer's own immutable split, so bounds are validated defensively but not exhaustively.
fn read_rrs_entries(bytes: &[u8]) -> io::Result<Vec<(u64, RoaringBitmap)>> {
    if bytes.len() < 20 || &bytes[0..4] != b"RRSI" {
        return Err(io::Error::other("compact: input is not an RRS split"));
    }
    let ngrams = read_u32(bytes, 8) as usize;
    let stride = read_u32(bytes, 12) as usize;
    let sparse_count = if ngrams == 0 || stride == 0 {
        0
    } else {
        ngrams.div_ceil(stride)
    };
    let dict_start = 20 + sparse_count * 8;
    let read = |off: usize, len: usize| -> io::Result<RoaringBitmap> {
        let end = off
            .checked_add(len)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| io::Error::other("compact: posting out of range"))?;
        deserialize(&bytes[off..end]).map_err(to_io)
    };
    let mut out = Vec::with_capacity(ngrams);
    for i in 0..ngrams {
        let base = dict_start + i * 24;
        if base + 24 > bytes.len() {
            return Err(io::Error::other("compact: truncated dictionary"));
        }
        let key = read_u64(bytes, base);
        let head_off = read_u64(bytes, base + 8) as usize;
        let head_size = read_u32(bytes, base + 16) as usize;
        let tail_size = read_u32(bytes, base + 20) as usize;
        let mut bm = read(head_off, head_size)?;
        if tail_size > 0 {
            bm |= read(head_off + head_size, tail_size)?;
        }
        out.push((key, bm));
    }
    Ok(out)
}

/// Maps an [`IndexError`] to an [`io::Error`] for the build-side `io::Result` surface.
fn to_io(e: IndexError) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::SplitSetWriter;
    use crate::fetch::MemoryFetch;
    use crate::splitset::{Policy, SplitFetcher, SplitSet};
    use crate::splitset_build::{SplitBuildConfig, SplitSetBuilder};
    use futures::executor::block_on;
    use std::collections::HashMap;

    /// A [`SplitFetcher`] over an in-memory name→bytes map (base splits, deltas, compacted).
    struct MapResolver(HashMap<String, Vec<u8>>);

    impl SplitFetcher for MapResolver {
        type Fetch = MemoryFetch;
        fn fetch_named(&self, name: &str) -> MemoryFetch {
            MemoryFetch::new(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn open(manifest: &[u8]) -> SplitSet {
        block_on(SplitSet::open(MemoryFetch::new(manifest.to_vec()))).unwrap()
    }

    #[test]
    fn flush_supersession_and_compact_lifecycle() {
        // ---- base: 6 docs all matching "abc", built tiered ----
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            policy: Policy::Tiered,
            byte_cap: 400,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 10, // exercise Blooms across base + delta + compacted splits
        });
        for i in 0..6 {
            b.add_text(&format!("abc base{i}")).unwrap();
        }
        let built = b.finish().unwrap();
        let mut files: HashMap<String, Vec<u8>> = built.splits.iter().cloned().collect();
        let base_ss = open(&built.manifest);

        // ---- resume + add two fresh docs + flush (atomic cutover bytes) ----
        let mut w = SplitSetWriter::resume(&base_ss, 3, 0, 0, "corpus".to_string(), 10);
        assert_eq!(w.add_text("abc new0"), 6); // ids continue past the base
        assert_eq!(w.add_text("abc new1"), 7);
        assert_eq!(w.memtable_doc_count(), 2);
        let f = w.flush().unwrap().expect("a flush happened");
        files.insert(f.split_name.clone(), f.split_bytes.clone());

        let ss1 = open(&f.manifest);
        assert_eq!(ss1.delta_splits().len(), 1);
        let res1 = block_on(ss1.search(&MapResolver(files.clone()), "abc", 100)).unwrap();
        // Base in rank order, then the two fresh docs appended (findable after the base).
        assert_eq!(res1, vec![0, 1, 2, 3, 4, 5, 6, 7]);

        // ---- delete a base doc + flush (deletes-only -> a tombstone-carrying split) ----
        w.delete(2);
        let f2 = w.flush().unwrap().expect("deletes-only flush");
        files.insert(f2.split_name.clone(), f2.split_bytes.clone());
        let ss2 = open(&f2.manifest);
        assert_eq!(ss2.delta_splits().len(), 2);
        let res2 = block_on(ss2.search(&MapResolver(files.clone()), "abc", 100)).unwrap();
        assert_eq!(res2, vec![0, 1, 3, 4, 5, 6, 7]); // doc 2 masked by the tombstone

        // ---- compact the two deltas into one absolute-id split ----
        let inputs: Vec<(String, Vec<u8>)> = ss2
            .delta_splits()
            .iter()
            .map(|s| (s.data_file.clone(), files[&s.data_file].clone()))
            .collect();
        let c = w.compact(&inputs).unwrap();
        assert_eq!(c.removed.len(), 2);
        files.insert(c.split_name.clone(), c.split_bytes.clone());
        let ss3 = open(&c.manifest);
        assert_eq!(ss3.delta_splits().len(), 1, "two deltas merged into one");
        assert!(ss3.delta_splits()[0].absolute_ids());
        let res3 = block_on(ss3.search(&MapResolver(files.clone()), "abc", 100)).unwrap();
        assert_eq!(
            res3, res2,
            "compaction preserves results (and supersession)"
        );
    }

    #[test]
    fn fresh_writer_flushes_first_delta() {
        let mut w = SplitSetWriter::new(super::WriterConfig {
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            byte_cap: 1 << 20,
            name_prefix: "fresh".to_string(),
            policy: Policy::StableKey,
            tier_count: 0,
            sortcol: None,
            bloom_bits_per_key: 0,
        });
        assert!(w.flush().unwrap().is_none(), "nothing to flush yet");
        w.add_text("abc hello");
        let f = w.flush().unwrap().unwrap();
        let ss = open(&f.manifest);
        assert_eq!(ss.base_count(), 0);
        assert_eq!(ss.delta_splits().len(), 1);
        let files: HashMap<String, Vec<u8>> = [(f.split_name, f.split_bytes)].into_iter().collect();
        assert_eq!(
            block_on(ss.search(&MapResolver(files), "abc", 10)).unwrap(),
            vec![0]
        );
    }
}
