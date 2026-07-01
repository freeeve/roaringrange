//! Native writer for the `RRSS` split-set manifest — the build-side mirror of
//! [`crate::splitset`], emitting the byte layout in `SPLITSET.md`. Excluded from the wasm
//! reader build.
//!
//! The writer is **metadata-only**: the split objects are separate, already-built `RRS`
//! files, so the caller passes each split's name, rank tier, doc-id range, byte size, and
//! supersession epoch (plus, generically, any summary bytes — the reserved TLV region, so
//! the enrichment step needs no writer change). The byte-capped *assignment* of docs to
//! splits (the greedy seal) is a separate builder; this only serializes the manifest those
//! splits produce.

use crate::build::{
    serialize_posting, split_posting, write_facets_with, write_index_with, FacetCategory,
    FacetField, DEFAULT_HEAD_BOUNDARY, DEFAULT_STRIDE,
};
use crate::facet::facet_key;
use crate::ngram::ngram_keys_with;
use crate::splitset::{
    bloom_build, tlv_record, BodyKind, Policy, FLAG_BLOOM, FLAG_CASE_SENSITIVE, FLAG_FACET,
    SORTCOL_FLAG_DESCENDING, SUMMARY_TAG_BLOOM, SUMMARY_TAG_FACET,
};
#[cfg(feature = "terms")]
use crate::terms::{Language, Tokenizer};
#[cfg(feature = "terms")]
use crate::terms_build::write_term_index_from_postings;
use roaring::RoaringBitmap;
use std::collections::BTreeMap;
use std::io::{self, Write};

/// `RRSS` magic.
const MAGIC: &[u8; 4] = b"RRSS";
/// Format version written into the header.
const VERSION: u16 = 1;

/// The stable-key rank source to record in the manifest header: an `RRSC` store name, the
/// rank column within it, and its direction. Pass `None` for the tiered policy (rank is the
/// doc-id range there) or when stable-key rank is supplied out of band.
#[derive(Clone)]
pub struct SortColSpec {
    /// The `RRSC` data-file name holding the rank column.
    pub name: String,
    /// Column index within that `RRSC`.
    pub column: u16,
    /// Whether a higher value ranks better (descending sort).
    pub descending: bool,
}

/// One split to record in the manifest. Carries the split's identity and pruning metadata;
/// the split's `RRS` bytes live in its own `data_file` and are not passed here.
pub struct SplitSpec {
    /// The split's `RRS` data-file name (or URL).
    pub data_file: String,
    /// Rank tier (tiered policy; `0` for stable-key / delta).
    pub tier: u16,
    /// Number of docs in the split.
    pub doc_count: u32,
    /// Minimum doc id present (inclusive).
    pub doc_id_lo: u32,
    /// Maximum doc id present (inclusive).
    pub doc_id_hi: u32,
    /// Flush/build epoch — supersession ordering (`0` for an additions-only base).
    pub epoch: u64,
    /// The split `.rrs` file size in bytes.
    pub byte_size: u64,
    /// Per-split flags (`SPLIT_FLAG_HAS_TOMBSTONE` | …); `0` in the additions-only case.
    pub flags: u16,
    /// Opaque summary bytes for this split — the reserved TLV region (term Bloom / facet /
    /// time / tombstone). Empty in v1.
    pub summary: Vec<u8>,
}

/// Manifest-level configuration: the base policy, tier count, base/delta boundary, the byte
/// cap the splits were sealed at, and the optional stable-key rank descriptor.
pub struct SplitSetConfig {
    /// How the base splits were assembled.
    pub policy: Policy,
    /// Number of rank tiers (tiered policy); `0` for stable-key.
    pub tier_count: u16,
    /// Number of base splits — entries `[0, base_count)` are base, the rest delta.
    pub base_count: u32,
    /// The per-split byte cap the builder sealed at (informational).
    pub byte_cap: u64,
    /// The n-gram window the splits were built with — recorded so the reader can derive a
    /// query's keys for Bloom pruning without opening a split. Must match the splits. `0` for a
    /// term-bodied set (no n-grams).
    pub gram_size: u16,
    /// How each split's data file is encoded ([`BodyKind`]). Written to header
    /// byte 9; trigram (`0`) keeps older manifests byte-identical.
    pub body_kind: BodyKind,
    /// The stable-key rank source, if any.
    pub sortcol: Option<SortColSpec>,
    /// Header summary-presence flags (`FLAG_BLOOM` | `FLAG_FACET` | …).
    pub flags: u16,
}

/// Writes the `RRSS` manifest for `splits` to `w`, in the order given (base splits first,
/// then delta splits — `config.base_count` marks the boundary). Emits
/// `[header][split entries][string blob][summary blob]` per `SPLITSET.md`; all integers
/// little-endian. The string blob holds the split data-file names in split order followed by
/// the optional sort-column name; the summary blob concatenates the non-empty per-split
/// summary regions.
pub fn write_splitset<W: Write>(
    mut w: W,
    splits: &[SplitSpec],
    config: &SplitSetConfig,
) -> io::Result<()> {
    let split_count: u32 = splits
        .len()
        .try_into()
        .map_err(|_| io::Error::other("RRSS split count exceeds the 32-bit limit"))?;
    if config.base_count > split_count {
        return Err(io::Error::other(format!(
            "RRSS base_count {} exceeds split count {}",
            config.base_count, split_count
        )));
    }

    // String blob: split data-file names in order, then the optional sort-column name.
    let mut string_blob: Vec<u8> = Vec::new();
    let mut name_spans: Vec<(u32, u16)> = Vec::with_capacity(splits.len());
    for s in splits {
        name_spans.push(push_name(&mut string_blob, &s.data_file)?);
    }
    let (sortcol_name_off, sortcol_name_len, sortcol_column, sortcol_flags) = match &config.sortcol
    {
        Some(sc) => {
            let (off, len) = push_name(&mut string_blob, &sc.name)?;
            let flags = if sc.descending {
                SORTCOL_FLAG_DESCENDING
            } else {
                0
            };
            (off, len, sc.column, flags)
        }
        None => (0, 0, 0, 0),
    };

    // Summary blob: each non-empty split summary appended; the rest record `(0, 0)`.
    let mut summary_blob: Vec<u8> = Vec::new();
    let mut summary_spans: Vec<(u64, u32)> = Vec::with_capacity(splits.len());
    for s in splits {
        if s.summary.is_empty() {
            summary_spans.push((0, 0));
        } else {
            let off = summary_blob.len() as u64;
            let len: u32 = s
                .summary
                .len()
                .try_into()
                .map_err(|_| io::Error::other("RRSS split summary exceeds the 32-bit limit"))?;
            summary_blob.extend_from_slice(&s.summary);
            summary_spans.push((off, len));
        }
    }

    let str_bytes: u32 = string_blob
        .len()
        .try_into()
        .map_err(|_| io::Error::other("RRSS string blob exceeds the 32-bit limit"))?;
    let summary_bytes = summary_blob.len() as u64;

    // Header (64 B).
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&config.flags.to_le_bytes())?;
    w.write_all(&[config.policy.to_u8()])?;
    w.write_all(&[u8::from(config.body_kind)])?; // bodyKind @9 (0 = trigram RRS, 1 = term RRTI)
    w.write_all(&config.tier_count.to_le_bytes())?;
    w.write_all(&split_count.to_le_bytes())?;
    w.write_all(&config.base_count.to_le_bytes())?;
    w.write_all(&str_bytes.to_le_bytes())?;
    w.write_all(&summary_bytes.to_le_bytes())?;
    w.write_all(&config.byte_cap.to_le_bytes())?;
    w.write_all(&sortcol_name_off.to_le_bytes())?;
    w.write_all(&sortcol_name_len.to_le_bytes())?;
    w.write_all(&sortcol_column.to_le_bytes())?;
    w.write_all(&[sortcol_flags])?;
    w.write_all(&config.gram_size.to_le_bytes())?; // gramSize @49
    w.write_all(&[0u8; 5])?; // pad1
    w.write_all(&[0u8; 8])?; // reserved

    // Split entries (56 B each), in split order.
    for (i, s) in splits.iter().enumerate() {
        let (name_off, name_len) = name_spans[i];
        let (summary_off, summary_len) = summary_spans[i];
        w.write_all(&name_off.to_le_bytes())?;
        w.write_all(&name_len.to_le_bytes())?;
        w.write_all(&s.tier.to_le_bytes())?;
        w.write_all(&s.doc_count.to_le_bytes())?;
        w.write_all(&s.doc_id_lo.to_le_bytes())?;
        w.write_all(&s.doc_id_hi.to_le_bytes())?;
        w.write_all(&s.flags.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // pad
        w.write_all(&s.byte_size.to_le_bytes())?;
        w.write_all(&s.epoch.to_le_bytes())?;
        w.write_all(&summary_off.to_le_bytes())?;
        w.write_all(&summary_len.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // reserved
    }

    w.write_all(&string_blob)?;
    w.write_all(&summary_blob)?;
    Ok(())
}

/// Builds a fixed, field-exhaustive `RRSS` manifest used as the cross-language conformance
/// golden (the Go writer in `go/splitset.go` must reproduce these exact bytes). Covers the
/// header (stable-key policy, tier count, base/delta boundary, byte cap, sort-column
/// descriptor, flags), three split entries with varied fields, per-split summaries (so the
/// summary blob is exercised), and the string blob. See `go/conformance/`.
#[cfg(test)]
pub(crate) fn conformance_golden() -> Vec<u8> {
    use crate::splitset::{FLAG_TOMBSTONES, SPLIT_FLAG_HAS_TOMBSTONE};
    let splits = vec![
        SplitSpec {
            data_file: "base-s00000.rrs".to_string(),
            tier: 0,
            doc_count: 100,
            doc_id_lo: 0,
            doc_id_hi: 99,
            epoch: 0,
            byte_size: 4096,
            flags: 0,
            summary: Vec::new(),
        },
        SplitSpec {
            data_file: "base-s00001.rrs".to_string(),
            tier: 1,
            doc_count: 50,
            doc_id_lo: 100,
            doc_id_hi: 149,
            epoch: 0,
            byte_size: 2048,
            flags: 0,
            summary: vec![0x01, 0x02, 0x03],
        },
        SplitSpec {
            data_file: "delta-d00000.rrs".to_string(),
            tier: 0,
            doc_count: 5,
            doc_id_lo: 1000,
            doc_id_hi: 1004,
            epoch: 7,
            byte_size: 512,
            flags: SPLIT_FLAG_HAS_TOMBSTONE,
            summary: vec![0x04, 0x01, 0x00, 0x00, 0x00, 0xff],
        },
    ];
    let config = SplitSetConfig {
        policy: Policy::StableKey,
        tier_count: 2,
        base_count: 2,
        byte_cap: 32 << 20,
        gram_size: 3,
        body_kind: BodyKind::Trigram,
        sortcol: Some(SortColSpec {
            name: "corpus.rrsc".to_string(),
            column: 3,
            descending: true,
        }),
        flags: FLAG_TOMBSTONES,
    };
    let mut out = Vec::new();
    write_splitset(&mut out, &splits, &config).unwrap();
    out
}

/// Builds a fixed, deterministic split set used as the cross-language **split-assignment**
/// conformance fixture: the Go `SplitSetBuilder` must reproduce the manifest *and* every split
/// `RRS` byte-for-byte from the same docs + config (`go/conformance` / `go/splitsetbuild_test.go`).
/// Tiered, a small cap (forcing several splits), and term Blooms (so the Bloom bytes are
/// conformance-checked too). Doc order is rank order.
#[cfg(test)]
pub(crate) fn conformance_build() -> BuiltSplitSet {
    // (text, facets) per doc — also exercises the per-split `RRSF` facet sidecars and the
    // facet-presence summary (tag 2) for cross-language conformance.
    let docs: [(&str, &[(&str, &str)]); 5] = [
        ("alpha beta", &[("year", "2020"), ("kind", "a")]),
        ("beta gamma", &[("year", "2021"), ("kind", "b")]),
        ("gamma delta", &[("year", "2020"), ("kind", "a")]),
        ("delta alpha", &[("year", "2021"), ("kind", "b")]),
        ("alpha gamma", &[("year", "2022"), ("kind", "a")]),
    ];
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap: 600,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "corpus".to_string(),
        sortcol: None,
        bloom_bits_per_key: 8,
        case_sensitive: false,
    });
    for (text, facets) in docs {
        let pairs: Vec<(String, String)> = facets
            .iter()
            .map(|(f, c)| (f.to_string(), c.to_string()))
            .collect();
        b.add_faceted(text, &pairs).unwrap();
    }
    b.finish().unwrap()
}

/// Geometric-tiering conformance fixture: the same corpus repeated enough that the
/// doubling caps (`byte_cap 300 → byte_cap_max 1200`) place several seal boundaries —
/// each tier visibly larger than the last. Shared with Go via
/// `go/testdata/rrss_geo_build_golden.txt`, pinning the per-tier cap arithmetic
/// (`cap_for` ⇄ Go `capFor`) cross-language: a one-off divergence in the boundary
/// placement changes every byte after it.
#[cfg(test)]
pub(crate) fn geo_conformance_build() -> BuiltSplitSet {
    let words = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    ];
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap: 300,
        byte_cap_max: 1200,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "geo".to_string(),
        sortcol: None,
        bloom_bits_per_key: 8,
        case_sensitive: false,
    });
    for i in 0..24 {
        let text = format!(
            "{} {}",
            words[i % words.len()],
            words[(i + 3) % words.len()]
        );
        let year = (2018 + (i % 5)).to_string();
        b.add_faceted(&text, &[("year".to_string(), year)]).unwrap();
    }
    b.finish().unwrap()
}

/// Builds the facet-presence summary payload: `[count u32 LE][key u64 LE]*`, the sorted,
/// deduplicated `facet_key`s of the categories present in `facets`.
fn facet_presence(
    facets: &BTreeMap<String, BTreeMap<String, RoaringBitmap>>,
    case_fold: bool,
) -> Vec<u8> {
    let mut keys: Vec<u64> = Vec::new();
    for (field, cats) in facets {
        for cat in cats.keys() {
            keys.push(facet_key(field, cat, case_fold));
        }
    }
    keys.sort_unstable();
    keys.dedup();
    let mut out = Vec::with_capacity(4 + keys.len() * 8);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        out.extend_from_slice(&k.to_le_bytes());
    }
    out
}

/// Converts the open facet postings into [`FacetField`]s with head/tail split postings, for
/// [`write_facets`]. Field order is the `BTreeMap`'s sorted order (deterministic).
fn facet_fields(
    facets: BTreeMap<String, BTreeMap<String, RoaringBitmap>>,
    head_boundary: u32,
) -> Vec<FacetField> {
    facets
        .into_iter()
        .map(|(name, cats)| FacetField {
            name,
            cats: cats
                .into_iter()
                .map(|(cname, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm, head_boundary);
                    FacetCategory {
                        name: cname,
                        card,
                        head,
                        tail,
                    }
                })
                .collect(),
        })
        .collect()
}

/// Appends a UTF-8 name to `blob` and returns its `(offset, length)` span, erroring if the
/// offset or length overflows the manifest's 32-/16-bit fields.
fn push_name(blob: &mut Vec<u8>, name: &str) -> io::Result<(u32, u16)> {
    let off: u32 = blob
        .len()
        .try_into()
        .map_err(|_| io::Error::other("RRSS string blob exceeds the 32-bit limit"))?;
    let len: u16 = name
        .len()
        .try_into()
        .map_err(|_| io::Error::other("RRSS name exceeds the 16-bit length limit"))?;
    blob.extend_from_slice(name.as_bytes());
    Ok((off, len))
}

/// Build-time configuration for a [`SplitSetBuilder`].
#[derive(Clone)]
pub struct SplitBuildConfig {
    /// How the base splits are assembled (`Tiered` feeds docs in rank order; `StableKey`
    /// feeds them in ingest order and records the rank `sortcol`).
    pub policy: Policy,
    /// The serialized byte cap each split is sealed at. Splits are kept at or under this via
    /// an upper-bound size estimate (see [`SplitSetBuilder::add_keys`]). **Must be > 0** — a `0`
    /// cap seals every document into its own split and [`SplitSetBuilder::finish`] then errors.
    /// With a non-zero [`byte_cap_max`](Self::byte_cap_max) this is the FIRST tier's cap.
    pub byte_cap: u64,
    /// Geometric tiering: when non-zero, tier `i`'s cap is `min(byte_cap << i, byte_cap_max)` —
    /// small splits at the top of the rank order (fine pruning where queries concentrate),
    /// doubling down the tail so the whole set stays at a handful of splits and a full
    /// worst-case descent pays per-split round-trip overhead only ~log-many times. `0` keeps
    /// the flat single-cap behavior (and every existing manifest byte-identical).
    pub byte_cap_max: u64,
    /// N-gram window the split `RRS` files are built with (e.g. `3`).
    pub gram_size: u16,
    /// Doc-ID head/tail split each split `RRS` uses (a multiple of 65536); pass `0` for
    /// [`DEFAULT_HEAD_BOUNDARY`]. Local to each split — a split is structurally a monolith.
    pub head_boundary: u32,
    /// Sparse-index stride each split `RRS` uses; pass `0` for [`DEFAULT_STRIDE`].
    pub stride: u32,
    /// Filename prefix for the emitted splits — `‹prefix›-s00000.rrs`, `…-s00001.rrs`, ….
    pub name_prefix: String,
    /// The stable-key rank source recorded in the manifest (the `RRSC` the caller builds
    /// separately over the same global doc-id space). `None` for the tiered policy.
    pub sortcol: Option<SortColSpec>,
    /// Bits per key for the per-split term Bloom filter (the biggest fan-out reducer: skip a
    /// split whose vocabulary can't contain a query n-gram). `0` disables it; `~10` gives a
    /// ~1% false-positive rate. The filters live in the manifest's summary blob, so larger
    /// values grow the manifest.
    pub bloom_bits_per_key: u32,
    /// Build a **case-sensitive** split set: n-gram and facet keys are not lowercased at index
    /// or query time (v4 `RRS` splits, case-sensitive `RRSF` keys, and the manifest's
    /// case-sensitive flag). The default (`false`) case-folds, reproducing the historical
    /// byte-identical behavior.
    pub case_sensitive: bool,
}

/// A list of emitted files as `(filename, bytes)` — split `RRS` blobs or facet `RRSF` sidecars.
pub type NamedFiles = Vec<(String, Vec<u8>)>;

/// The output of [`SplitSetBuilder::finish`]: the `.rrss` manifest bytes and each split's
/// `(filename, RRS bytes)`. The caller writes the manifest to `‹prefix›.rrss` and every
/// split to its filename; nothing is written by the library.
pub struct BuiltSplitSet {
    /// The serialized `RRSS` manifest.
    pub manifest: Vec<u8>,
    /// Each emitted split as `(filename, RRS bytes)`, in seal (rank/ingest) order.
    pub splits: Vec<(String, Vec<u8>)>,
    /// Each split's facet sidecar as `(filename, RRSF bytes)` — `‹split›.rrf` — when documents
    /// carried facets (empty otherwise). Parallel to `splits` by seal order.
    pub facets: Vec<(String, Vec<u8>)>,
}

/// A byte-capped split builder (the **greedy seal**). Documents are fed in policy order —
/// rank order for [`Policy::Tiered`] (top-cited first), ingest order for
/// [`Policy::StableKey`] — and accumulated into the open split; when an upper-bound estimate
/// of the open split's serialized size would cross [`SplitBuildConfig::byte_cap`], the open
/// split is sealed into an immutable `RRS` and a fresh one starts.
///
/// Splits store **local 0-based doc IDs**; the manifest's `docIdLo` carries each split's
/// global base, so `global_id = docIdLo + local_id` (and for the tiered policy `global_id`
/// is the rank). This keeps every split structurally identical to the monolith — same
/// head/tail layout — so one split is exactly today's `RRS`. Nothing is dropped: the corpus
/// grows the split *count*, and pruning (not truncation) keeps per-query bytes down.
pub struct SplitSetBuilder {
    policy: Policy,
    byte_cap: u64,
    byte_cap_max: u64,
    gram_size: u16,
    head_boundary: u32,
    stride: u32,
    name_prefix: String,
    sortcol: Option<SortColSpec>,
    bloom_bits_per_key: u32,
    case_normalization: bool,
    /// The open split's postings, keyed by n-gram, holding local 0-based doc IDs.
    open: BTreeMap<u64, RoaringBitmap>,
    /// Number of docs (incl. keyword-less ones) in the open split — its local-id count.
    open_count: u32,
    /// Global doc id of the open split's first doc (its manifest `docIdLo`).
    global_base: u32,
    /// Next global doc id to hand out.
    next_global_id: u32,
    /// Running upper bound on the open split's serialized posting bytes (see `add_keys`).
    postings_upper: u64,
    /// The open split's facet postings: field → category → bitmap of local 0-based doc ids.
    open_facets: BTreeMap<String, BTreeMap<String, RoaringBitmap>>,
    /// Whether any document has carried a facet (drives the per-split `RRSF` + header flag).
    has_facets: bool,
    /// Sealed split metadata, in seal order (all base — the batch builder writes no delta).
    specs: Vec<SplitSpec>,
    /// Sealed split blobs `(filename, bytes)`, parallel to `specs`.
    blobs: Vec<(String, Vec<u8>)>,
    /// Sealed split facet sidecars `(filename, RRSF bytes)`, parallel to `specs`.
    facet_blobs: Vec<(String, Vec<u8>)>,
}

/// Bytes charged per new key: the 24-byte dictionary entry plus the serialized base of the
/// key's head **and** (empty) tail roaring bitmaps. Measured at ~48 B for the common case
/// where a split's docs sit in a single head container; `64` adds margin so the running
/// estimate stays an upper bound and a sealed split stays at or under the cap. (A split with
/// more than 65536 docs spreads a key across several head/tail containers, adding ~8 B per
/// extra container — a modest overshoot the cap tolerates as a soft target.)
const PER_NEW_KEY_BYTES: u64 = 64;
/// Bytes charged per (key, doc) element — a roaring array container stores a `u16` (2 B) per
/// element; once a container turns bitmap this over-counts, keeping the estimate an upper bound.
const PER_ELEMENT_BYTES: u64 = 2;

/// The byte cap for the split at seal index `tier`: the flat `byte_cap` when
/// `byte_cap_max` is `0`, else doubling per tier and clamped to `byte_cap_max`
/// (geometric tiering — see [`SplitBuildConfig::byte_cap_max`]). Shared by the
/// trigram and term builders so the two stay seal-identical (and mirrored by
/// Go's `capFor` — the conformance goldens pin the boundary placement).
fn cap_for(byte_cap: u64, byte_cap_max: u64, tier: usize) -> u64 {
    if byte_cap_max == 0 {
        return byte_cap;
    }
    let shifted = byte_cap
        .checked_shl(tier.min(63) as u32)
        .unwrap_or(u64::MAX);
    shifted.min(byte_cap_max.max(byte_cap))
}

impl SplitSetBuilder {
    /// Creates an empty builder. `config.head_boundary`/`stride` of `0` take the `RRS`
    /// defaults.
    pub fn new(config: SplitBuildConfig) -> Self {
        SplitSetBuilder {
            policy: config.policy,
            byte_cap: config.byte_cap,
            byte_cap_max: config.byte_cap_max,
            gram_size: config.gram_size,
            head_boundary: if config.head_boundary == 0 {
                DEFAULT_HEAD_BOUNDARY
            } else {
                config.head_boundary
            },
            stride: if config.stride == 0 {
                DEFAULT_STRIDE
            } else {
                config.stride
            },
            name_prefix: config.name_prefix,
            sortcol: config.sortcol,
            bloom_bits_per_key: config.bloom_bits_per_key,
            case_normalization: !config.case_sensitive,
            open: BTreeMap::new(),
            open_count: 0,
            global_base: 0,
            next_global_id: 0,
            postings_upper: 0,
            open_facets: BTreeMap::new(),
            has_facets: false,
            specs: Vec::new(),
            blobs: Vec::new(),
            facet_blobs: Vec::new(),
        }
    }

    /// Tokenizes `text` into n-gram keys and appends it as one document — the convenience
    /// over [`add_keys`](Self::add_keys). Returns the doc's global id.
    pub fn add_text(&mut self, text: &str) -> io::Result<u32> {
        let keys = ngram_keys_with(text, self.gram_size as usize, self.case_normalization);
        self.add_inner(&keys, &[])
    }

    /// Like [`add_text`](Self::add_text) but also records the document's facet memberships
    /// (`(field, category)` pairs). Each split gets its own `RRSF` facet sidecar over its docs,
    /// and the manifest carries a per-split facet-presence summary so a facet-filtered query
    /// can skip a split that holds none of a selected field's categories. Returns the global id.
    pub fn add_faceted(&mut self, text: &str, facets: &[(String, String)]) -> io::Result<u32> {
        let keys = ngram_keys_with(text, self.gram_size as usize, self.case_normalization);
        self.add_inner(&keys, facets)
    }

    /// Appends one document by its (deduplicated) n-gram `keys`, returning its global doc id.
    /// A keyword-less document (empty `keys`) still consumes an id so the doc-id space stays
    /// dense and aligned with records/facets/vectors.
    ///
    /// Before inserting, the doc's marginal byte cost is added to the running upper-bound
    /// estimate; if that would exceed the cap and the open split already holds a document,
    /// the open split is sealed first so this doc starts the next one — guaranteeing each
    /// sealed split is ≤ cap. A single document whose postings alone exceed the cap is a
    /// degenerate corpus and is reported by [`finish`](Self::finish).
    pub fn add_keys(&mut self, keys: &[u64]) -> io::Result<u32> {
        self.add_inner(keys, &[])
    }

    /// The shared add path: makes the seal decision (text estimate only — facets live in a
    /// separate `RRSF`), assigns the local id, then records the n-gram `keys` and the document's
    /// `facets` under it.
    fn add_inner(&mut self, keys: &[u64], facets: &[(String, String)]) -> io::Result<u32> {
        // Marginal cost of this doc against the open split: new keys each cost a dict entry +
        // posting base, and every key occurrence costs one element. Seal first if adding the
        // doc would push the open split over the cap (and it already holds a document).
        let new_keys = keys.iter().filter(|k| !self.open.contains_key(k)).count() as u64;
        let marginal = new_keys * PER_NEW_KEY_BYTES + keys.len() as u64 * PER_ELEMENT_BYTES;
        if self.open_count > 0
            && self.estimate() + marginal
                > cap_for(self.byte_cap, self.byte_cap_max, self.specs.len())
        {
            self.seal()?;
        }

        let local_id = self.open_count;
        for &k in keys {
            if self.open.entry(k).or_default().insert(local_id) {
                self.postings_upper += PER_ELEMENT_BYTES;
            }
        }
        for (field, cat) in facets {
            self.open_facets
                .entry(field.clone())
                .or_default()
                .entry(cat.clone())
                .or_default()
                .insert(local_id);
            self.has_facets = true;
        }
        self.open_count += 1;
        let global = self.next_global_id;
        self.next_global_id += 1;
        Ok(global)
    }

    /// The current upper-bound estimate of the open split's serialized `RRS` size: the header,
    /// the per-key dictionary + posting base ([`PER_NEW_KEY_BYTES`] each), the sparse index,
    /// and the per-element bytes accumulated in `postings_upper`.
    fn estimate(&self) -> u64 {
        let nkeys = self.open.len() as u64;
        let sparse = nkeys.div_ceil(self.stride.max(1) as u64) * 8;
        20 + nkeys * PER_NEW_KEY_BYTES + sparse + self.postings_upper
    }

    /// Seals the open split into an immutable `RRS` blob + a manifest entry, then resets the
    /// open state with `global_base` advanced to the next id. A no-op when the open split is
    /// empty.
    fn seal(&mut self) -> io::Result<()> {
        if self.open_count == 0 {
            return Ok(());
        }
        let open = std::mem::take(&mut self.open);
        let entries: Vec<(u64, Vec<u8>)> = open
            .iter()
            .map(|(k, bm)| (*k, serialize_posting(bm)))
            .collect();
        let mut bytes = Vec::new();
        write_index_with(
            &mut bytes,
            self.gram_size,
            self.stride,
            entries,
            self.case_normalization,
        )?;

        let idx = self.specs.len();
        let name = format!("{}-s{idx:05}.rrs", self.name_prefix);
        let tier = match self.policy {
            Policy::Tiered => idx.min(u16::MAX as usize) as u16,
            Policy::StableKey => 0,
        };
        // Summary = term Bloom (tag 1) then facet-presence (tag 2), both optional. The Bloom
        // lets a query skip a split lacking its n-grams; the facet-presence list lets a
        // facet-filtered query skip a split lacking a selected category — both without a fetch.
        let mut summary = Vec::new();
        if self.bloom_bits_per_key > 0 {
            let keys: Vec<u64> = open.keys().copied().collect();
            summary.extend_from_slice(&tlv_record(
                SUMMARY_TAG_BLOOM,
                &bloom_build(&keys, self.bloom_bits_per_key),
            ));
        }
        // Seal the split's facet sidecar (its categories over local ids) and the presence list.
        let open_facets = std::mem::take(&mut self.open_facets);
        if !open_facets.is_empty() {
            summary.extend_from_slice(&tlv_record(
                SUMMARY_TAG_FACET,
                &facet_presence(&open_facets, self.case_normalization),
            ));
            let facet_name = format!("{}-s{idx:05}.rrf", self.name_prefix);
            let mut facet_bytes = Vec::new();
            write_facets_with(
                &mut facet_bytes,
                facet_fields(open_facets, self.head_boundary),
                self.case_normalization,
            )?;
            self.facet_blobs.push((facet_name, facet_bytes));
        }
        self.specs.push(SplitSpec {
            data_file: name.clone(),
            tier,
            doc_count: self.open_count,
            doc_id_lo: self.global_base,
            doc_id_hi: self.global_base + self.open_count - 1,
            epoch: 0,
            byte_size: bytes.len() as u64,
            flags: 0,
            summary,
        });
        self.blobs.push((name, bytes));

        self.open_count = 0;
        self.global_base = self.next_global_id;
        self.postings_upper = 0;
        Ok(())
    }

    /// Number of documents added so far (across sealed and open splits).
    pub fn doc_count(&self) -> u32 {
        self.next_global_id
    }

    /// Streams out the splits sealed since the last call, as `(split RRS files, facet RRSF
    /// files)`, **clearing** the builder's internal buffers. Call it periodically during a
    /// large build (e.g. after each batch of `add`s) to write the bytes to disk and free them,
    /// so peak RAM is one open split rather than the whole split set — what makes a full-corpus
    /// split set buildable in bounded memory. The small per-split metadata (and Bloom
    /// summaries) stay in the builder for the manifest; [`finish`](Self::finish) seals the final
    /// open split and returns it plus the manifest, so a streaming caller writes `finish`'s
    /// remaining splits/facets last.
    pub fn drain_sealed(&mut self) -> (NamedFiles, NamedFiles) {
        (
            std::mem::take(&mut self.blobs),
            std::mem::take(&mut self.facet_blobs),
        )
    }

    /// Seals the final open split and serializes the manifest, returning the manifest bytes
    /// and every split's `(filename, RRS bytes)`. Errors if any single document's postings
    /// alone exceed the byte cap (a degenerate corpus — the split cannot be made to fit).
    pub fn finish(mut self) -> io::Result<BuiltSplitSet> {
        self.seal()?;
        for (i, spec) in self.specs.iter().enumerate() {
            let cap = cap_for(self.byte_cap, self.byte_cap_max, i);
            if spec.doc_count == 1 && spec.byte_size > cap {
                return Err(io::Error::other(format!(
                    "RRSS split {:?}: a single document's postings ({} B) exceed the byte cap ({} B)",
                    spec.data_file, spec.byte_size, cap
                )));
            }
        }
        let tier_count = match self.policy {
            Policy::Tiered => self.specs.len().min(u16::MAX as usize) as u16,
            Policy::StableKey => 0,
        };
        let mut flags = 0u16;
        if self.bloom_bits_per_key > 0 {
            flags |= FLAG_BLOOM;
        }
        if self.has_facets {
            flags |= FLAG_FACET;
        }
        if !self.case_normalization {
            flags |= FLAG_CASE_SENSITIVE;
        }
        let config = SplitSetConfig {
            policy: self.policy,
            tier_count,
            base_count: self.specs.len() as u32,
            byte_cap: self.byte_cap,
            gram_size: self.gram_size,
            body_kind: BodyKind::Trigram,
            sortcol: self.sortcol.take(),
            flags,
        };
        let mut manifest = Vec::new();
        write_splitset(&mut manifest, &self.specs, &config)?;
        Ok(BuiltSplitSet {
            manifest,
            splits: self.blobs,
            facets: self.facet_blobs,
        })
    }
}

/// Build-time configuration for a **term-bodied** ([`BodyKind::Term`]) split set: like
/// [`SplitBuildConfig`] but each sealed split is an `RRTI` term index rather than a trigram
/// `RRS`. The n-gram window is replaced by the tokenizer settings (recorded per split for
/// query/index symmetry), and there is no term Bloom (term-level Bloom pruning is deferred).
#[cfg(feature = "terms")]
#[derive(Clone)]
pub struct TermSplitBuildConfig {
    /// Base policy (tiered or stable-key) — identical cross-split semantics to the trigram builder.
    pub policy: Policy,
    /// The per-split byte cap; the open split seals before a document would cross it.
    /// **Must be nonzero** (a `0` cap seals every document into its own split and `finish`
    /// then errors). With a nonzero [`byte_cap_max`](Self::byte_cap_max) this is the FIRST
    /// tier's cap.
    pub byte_cap: u64,
    /// Geometric tiering: when non-zero, tier `i`'s cap is `min(byte_cap << i, byte_cap_max)`.
    /// `0` keeps the flat single-cap behavior. See [`SplitBuildConfig::byte_cap_max`].
    pub byte_cap_max: u64,
    /// Doc-ID head/tail split (a multiple of 65536); `0` takes the `RRS` default.
    pub head_boundary: u32,
    /// Split data-file name prefix — sealed splits are `‹prefix›-s00000.rrt`, ….
    pub name_prefix: String,
    /// Optional stable-key rank source (the `RRSC` the manifest names).
    pub sortcol: Option<SortColSpec>,
    /// The index language, shared by the `stem` and `stopwords` filters and recorded in each
    /// split's header. Must be `Some` whenever either filter is on; `None` only when both are
    /// off (a filter on with no language is rejected when a split seals).
    pub language: Option<Language>,
    /// Apply Snowball stemming in `language`. Independent of `stopwords`.
    pub stem: bool,
    /// Remove the language's stop words from the index (and, symmetrically, from queries).
    /// Requires `language`.
    pub stopwords: bool,
    /// Build a **case-sensitive** split set: terms are not lowercased at index or query time
    /// (each split's `RRTI`/`RRSF` records the choice). The default (`false`) case-folds,
    /// reproducing the historical byte-identical behavior.
    pub case_sensitive: bool,
}

/// Bytes charged per new term: the posting block base (`[tail_size u32][head roaring base]`) plus
/// the front-coded dict-entry framing (shared/suffix-len/head-off/head-size varints) and the block
/// router's amortized per-term cost. The term's own byte length is added on top — an upper bound,
/// since front-coding stores only each term's suffix. Keeps the estimate at or above the real
/// `RRTI` v2 size.
#[cfg(feature = "terms")]
const PER_NEW_TERM_BYTES: u64 = 24;
/// Bytes charged per `(term, doc)` occurrence — a roaring array element (`u16`); over-counts once
/// a container turns bitmap, keeping the estimate an upper bound.
#[cfg(feature = "terms")]
const PER_TERM_ELEMENT_BYTES: u64 = 2;
/// Fixed `RRTI` v2 header (40 B) + block-router FST base allowance, added once to each open
/// split's estimate.
#[cfg(feature = "terms")]
const TERM_INDEX_HEADER_EST: u64 = 128;

/// The term-bodied analogue of [`SplitSetBuilder`]: greedily packs documents into byte-capped
/// splits, but each sealed split is an `RRTI` term index (a blocked, front-coded term dictionary
/// keyed by whole token) instead of a trigram `RRS`. Everything cross-split — the policy, tiering,
/// doc-ID ranges,
/// facet-presence summaries, per-split `RRSF` sidecars, and the streaming
/// [`drain_sealed`](Self::drain_sealed) — is identical to the trigram builder; only the open
/// accumulator (keyed by term string, not n-gram `u64`) and [`seal`](Self::seal)'s body encoder
/// differ. The manifest records [`BodyKind::Term`] so the reader opens each split as a
/// [`crate::terms::TermIndex`]. Term Bloom summaries are deferred (no summary tag 1).
#[cfg(feature = "terms")]
pub struct TermSplitSetBuilder {
    policy: Policy,
    byte_cap: u64,
    byte_cap_max: u64,
    head_boundary: u32,
    name_prefix: String,
    sortcol: Option<SortColSpec>,
    language: Option<Language>,
    stem: bool,
    stopwords: bool,
    case_normalization: bool,
    /// The resident tokenizer (fixed at construction); query-side tokenization must match it.
    tokenizer: Tokenizer,
    /// The open split's postings, keyed by term, holding local 0-based doc IDs.
    open: BTreeMap<String, RoaringBitmap>,
    /// Number of docs (incl. token-less ones) in the open split — its local-id count.
    open_count: u32,
    /// Global doc id of the open split's first doc (its manifest `docIdLo`).
    global_base: u32,
    /// Next global doc id to hand out.
    next_global_id: u32,
    /// Running upper bound on the open split's serialized `RRTI` bytes.
    bytes_upper: u64,
    /// The open split's facet postings: field → category → bitmap of local 0-based doc ids.
    open_facets: BTreeMap<String, BTreeMap<String, RoaringBitmap>>,
    /// Whether any document has carried a facet (drives the per-split `RRSF` + header flag).
    has_facets: bool,
    /// Sealed split metadata, in seal order (all base — the batch builder writes no delta).
    specs: Vec<SplitSpec>,
    /// Sealed split blobs `(filename, RRTI bytes)`, parallel to `specs`.
    blobs: Vec<(String, Vec<u8>)>,
    /// Sealed split facet sidecars `(filename, RRSF bytes)`, parallel to `specs`.
    facet_blobs: Vec<(String, Vec<u8>)>,
}

#[cfg(feature = "terms")]
impl TermSplitSetBuilder {
    /// Creates an empty term-split builder. `config.head_boundary` of `0` takes the `RRS` default.
    pub fn new(config: TermSplitBuildConfig) -> Self {
        TermSplitSetBuilder {
            policy: config.policy,
            byte_cap: config.byte_cap,
            byte_cap_max: config.byte_cap_max,
            head_boundary: if config.head_boundary == 0 {
                DEFAULT_HEAD_BOUNDARY
            } else {
                config.head_boundary
            },
            name_prefix: config.name_prefix,
            sortcol: config.sortcol,
            language: config.language,
            stem: config.stem,
            stopwords: config.stopwords,
            case_normalization: !config.case_sensitive,
            tokenizer: Tokenizer::with(
                config.language,
                config.stem,
                config.stopwords,
                !config.case_sensitive,
            ),
            open: BTreeMap::new(),
            open_count: 0,
            global_base: 0,
            next_global_id: 0,
            bytes_upper: 0,
            open_facets: BTreeMap::new(),
            has_facets: false,
            specs: Vec::new(),
            blobs: Vec::new(),
            facet_blobs: Vec::new(),
        }
    }

    /// Tokenizes `text` and appends it as one document, returning its global doc id. A token-less
    /// document still consumes an id so the doc-id space stays dense and aligned with
    /// records/facets/vectors.
    pub fn add_text(&mut self, text: &str) -> io::Result<u32> {
        self.add_faceted(text, &[])
    }

    /// Like [`add_text`](Self::add_text) but also records the document's facet memberships
    /// (`(field, category)` pairs) into the open split's `RRSF` sidecar and presence summary.
    pub fn add_faceted(&mut self, text: &str, facets: &[(String, String)]) -> io::Result<u32> {
        let mut terms = self.tokenizer.tokenize(text);
        terms.sort();
        terms.dedup();

        // Marginal cost of this doc against the open split: new terms each cost an FST key +
        // posting base (plus the term's bytes), every occurrence costs a roaring element. Seal
        // first if adding the doc would push the open split over the cap (and it already holds one).
        let mut marginal = 0u64;
        for t in &terms {
            if !self.open.contains_key(t.as_str()) {
                marginal += PER_NEW_TERM_BYTES + t.len() as u64;
            }
            marginal += PER_TERM_ELEMENT_BYTES;
        }
        if self.open_count > 0
            && self.estimate() + marginal
                > cap_for(self.byte_cap, self.byte_cap_max, self.specs.len())
        {
            self.seal()?;
        }

        let local_id = self.open_count;
        for t in terms {
            let is_new = !self.open.contains_key(t.as_str());
            let len = t.len() as u64;
            if self.open.entry(t).or_default().insert(local_id) {
                self.bytes_upper += PER_TERM_ELEMENT_BYTES;
                if is_new {
                    self.bytes_upper += PER_NEW_TERM_BYTES + len;
                }
            }
        }
        for (field, cat) in facets {
            self.open_facets
                .entry(field.clone())
                .or_default()
                .entry(cat.clone())
                .or_default()
                .insert(local_id);
            self.has_facets = true;
        }
        self.open_count += 1;
        let global = self.next_global_id;
        self.next_global_id += 1;
        Ok(global)
    }

    /// The current upper-bound estimate of the open split's serialized `RRTI` size.
    fn estimate(&self) -> u64 {
        TERM_INDEX_HEADER_EST + self.bytes_upper
    }

    /// Seals the open split into an immutable `RRTI` blob + a manifest entry, then resets the open
    /// state with `global_base` advanced. A no-op when the open split is empty.
    fn seal(&mut self) -> io::Result<()> {
        if self.open_count == 0 {
            return Ok(());
        }
        let open = std::mem::take(&mut self.open);
        let mut bytes = Vec::new();
        write_term_index_from_postings(
            &mut bytes,
            open,
            self.head_boundary,
            self.language,
            self.stem,
            self.stopwords,
            self.case_normalization,
            0, // default dictionary block cap
        )?;

        let idx = self.specs.len();
        let name = format!("{}-s{idx:05}.rrt", self.name_prefix);
        let tier = match self.policy {
            Policy::Tiered => idx.min(u16::MAX as usize) as u16,
            Policy::StableKey => 0,
        };
        // Summary = facet-presence (tag 2) only; the term Bloom (tag 1) is deferred for term bodies.
        let mut summary = Vec::new();
        let open_facets = std::mem::take(&mut self.open_facets);
        if !open_facets.is_empty() {
            summary.extend_from_slice(&tlv_record(
                SUMMARY_TAG_FACET,
                &facet_presence(&open_facets, self.case_normalization),
            ));
            let facet_name = format!("{}-s{idx:05}.rrf", self.name_prefix);
            let mut facet_bytes = Vec::new();
            write_facets_with(
                &mut facet_bytes,
                facet_fields(open_facets, self.head_boundary),
                self.case_normalization,
            )?;
            self.facet_blobs.push((facet_name, facet_bytes));
        }
        self.specs.push(SplitSpec {
            data_file: name.clone(),
            tier,
            doc_count: self.open_count,
            doc_id_lo: self.global_base,
            doc_id_hi: self.global_base + self.open_count - 1,
            epoch: 0,
            byte_size: bytes.len() as u64,
            flags: 0,
            summary,
        });
        self.blobs.push((name, bytes));

        self.open_count = 0;
        self.global_base = self.next_global_id;
        self.bytes_upper = 0;
        Ok(())
    }

    /// Number of documents added so far (across sealed and open splits).
    pub fn doc_count(&self) -> u32 {
        self.next_global_id
    }

    /// Streams out the splits sealed since the last call, as `(split RRTI files, facet RRSF
    /// files)`, **clearing** the builder's buffers — the term-builder counterpart of
    /// [`SplitSetBuilder::drain_sealed`], for bounded-memory full-corpus builds.
    pub fn drain_sealed(&mut self) -> (NamedFiles, NamedFiles) {
        (
            std::mem::take(&mut self.blobs),
            std::mem::take(&mut self.facet_blobs),
        )
    }

    /// Seals the final open split and serializes the manifest (`body_kind = BodyKind::Term`,
    /// `gram_size = 0`), returning the manifest bytes and every split's `(filename, RRTI bytes)`.
    /// Errors if any single document's postings alone exceed the byte cap.
    pub fn finish(mut self) -> io::Result<BuiltSplitSet> {
        self.seal()?;
        for (i, spec) in self.specs.iter().enumerate() {
            let cap = cap_for(self.byte_cap, self.byte_cap_max, i);
            if spec.doc_count == 1 && spec.byte_size > cap {
                return Err(io::Error::other(format!(
                    "RRSS term split {:?}: a single document's postings ({} B) exceed the byte cap ({} B)",
                    spec.data_file, spec.byte_size, cap
                )));
            }
        }
        let tier_count = match self.policy {
            Policy::Tiered => self.specs.len().min(u16::MAX as usize) as u16,
            Policy::StableKey => 0,
        };
        let mut flags = 0u16;
        if self.has_facets {
            flags |= FLAG_FACET;
        }
        if !self.case_normalization {
            flags |= FLAG_CASE_SENSITIVE;
        }
        let config = SplitSetConfig {
            policy: self.policy,
            tier_count,
            base_count: self.specs.len() as u32,
            byte_cap: self.byte_cap,
            gram_size: 0, // term-bodied: no n-grams
            body_kind: BodyKind::Term,
            sortcol: self.sortcol.take(),
            flags,
        };
        let mut manifest = Vec::new();
        write_splitset(&mut manifest, &self.specs, &config)?;
        Ok(BuiltSplitSet {
            manifest,
            splits: self.blobs,
            facets: self.facet_blobs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::conformance_golden;

    /// The cross-language conformance golden (also asserted byte-for-byte by the Go writer in
    /// `go/splitset_test.go`). Locking it on the Rust side guards against an accidental layout
    /// change that would silently diverge from Go.
    const GOLDEN_HEX: &str = "525253530100080001000200030000000200000039000000090000000000000000000002000000002e0000000b00030001030000000000000000000000000000000000000f0000006400000000000000630000000000000000100000000000000000000000000000000000000000000000000000000000000f0000000f0001003200000064000000950000000000000000080000000000000000000000000000000000000000000003000000000000001e0000001000000005000000e8030000ec030000010000000002000000000000070000000000000003000000000000000600000000000000626173652d7330303030302e727273626173652d7330303030312e72727364656c74612d6430303030302e727273636f727075732e727273630102030401000000ff";

    #[test]
    fn manifest_matches_conformance_golden() {
        let bytes = conformance_golden();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, GOLDEN_HEX,
            "RRSS manifest layout drifted from the golden"
        );
    }
}

/// Cross-language conformance fixture for the TERM-bodied builder: a fixed corpus
/// through `TermSplitSetBuilder` with English stemming + stop words + facets. The
/// texts deliberately stress the tokenizer's Unicode contract — Turkish `İ`
/// (U+0130, the one unconditional multi-char lowercase mapping), a circled digit
/// (category `No`: numeric but not a digit), Greek capitals, and combining
/// accents — exactly where an inexact Go port would diverge. Shared with
/// `go/termsplitsetbuild_test.go` via `go/testdata/rrti_term_split_golden.txt`.
#[cfg(all(test, feature = "terms"))]
pub(crate) fn term_conformance_build() -> BuiltSplitSet {
    let docs: [(&str, &[(&str, &str)]); 6] = [
        (
            "The running runner runs quickly",
            &[("year", "2020"), ("kind", "a")],
        ),
        (
            "İstanbul naïve cafés résumé",
            &[("year", "2021"), ("kind", "b")],
        ),
        (
            "status ① bitmap roaring bitmaps",
            &[("year", "2020"), ("kind", "a")],
        ),
        (
            "ΣΟΦΌΣ σοφός wisdom connection",
            &[("year", "2022"), ("kind", "b")],
        ),
        (
            "connected connecting connections",
            &[("year", "2021"), ("kind", "a")],
        ),
        ("the a an and are", &[("year", "2022"), ("kind", "b")]), // all stop words → token-less doc
    ];
    let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap: 400, // small enough that the corpus seals into several tiers
        head_boundary: 0,
        name_prefix: "tcorpus".to_string(),
        sortcol: None,
        language: Some(Language::English),
        stem: true,
        stopwords: true,
        case_sensitive: false,
    });
    for (text, facets) in docs {
        let pairs: Vec<(String, String)> = facets
            .iter()
            .map(|(f, c)| (f.to_string(), c.to_string()))
            .collect();
        b.add_faceted(text, &pairs).unwrap();
    }
    b.finish().unwrap()
}

/// The **case-sensitive** term-split fixture (task 054): a mixed-case corpus with mixed-case
/// facet category values, built with `case_sensitive: true` (and no stemming/stop-words,
/// to isolate case). "Roaring"/"roaring", "Bitmap"/"bitmap", and the "A"/"a", "B"/"b" categories
/// stay distinct, so each split's `RRTI` carries the case-sensitive flag, the `RRSF` keys are
/// case-sensitive, and the manifest sets its case-sensitive flag. Shared with Go's
/// `termConformanceCaseSensitiveBuild` via `go/testdata/rrti_term_split_cs_golden.txt`.
#[cfg(all(test, feature = "terms"))]
pub(crate) fn term_conformance_cs_build() -> BuiltSplitSet {
    let docs: [(&str, &[(&str, &str)]); 4] = [
        ("Roaring Range Index", &[("Kind", "A")]),
        ("roaring range query", &[("Kind", "a")]),
        ("Bitmap BITMAP Index", &[("Kind", "B")]),
        ("bitmap index lookup", &[("Kind", "b")]),
    ];
    let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap: 400, // small enough to seal several tiers, large enough for a single doc
        head_boundary: 0,
        name_prefix: "cscorpus".to_string(),
        sortcol: None,
        language: None,
        stem: false,
        stopwords: false,
        case_sensitive: true,
    });
    for (text, facets) in docs {
        let pairs: Vec<(String, String)> = facets
            .iter()
            .map(|(f, c)| (f.to_string(), c.to_string()))
            .collect();
        b.add_faceted(text, &pairs).unwrap();
    }
    b.finish().unwrap()
}

#[cfg(test)]
mod conformance_full_build {
    use super::conformance_build;
    use std::collections::HashMap;

    /// Parses the shared cross-language golden (`name <hex>` per line) into `name -> bytes`.
    /// The same file is asserted by the Go builder (`go/splitsetbuild_test.go`), so it is the
    /// single source of truth for full split-set byte-for-byte conformance.
    fn golden() -> HashMap<String, Vec<u8>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrss_build_golden.txt"
        );
        let text = std::fs::read_to_string(path).expect("read shared golden");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let (name, hex) = l.split_once(' ').expect("`name <hex>` line");
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect::<Vec<u8>>();
                (name.to_string(), bytes)
            })
            .collect()
    }

    #[test]
    fn full_build_matches_shared_golden() {
        let g = golden();
        let built = conformance_build();
        assert_eq!(
            &built.manifest,
            g.get("manifest").expect("manifest golden present"),
            "RRSS manifest drifted from the shared golden"
        );
        // Every split RRS and every facet RRSF sidecar must match byte-for-byte.
        for (name, bytes) in built.splits.iter().chain(built.facets.iter()) {
            assert_eq!(
                bytes,
                g.get(name)
                    .unwrap_or_else(|| panic!("no golden for {name}")),
                "{name} bytes drifted from the shared golden"
            );
        }
        assert_eq!(
            g.len(),
            1 + built.splits.len() + built.facets.len(),
            "golden entry count drifted (manifest + splits + facet sidecars)"
        );
    }

    /// Regenerates the shared golden from the current builder output. Ignored by default; run with
    /// `cargo test --features splits regen_shared_golden -- --ignored` after an intended change.
    #[test]
    #[ignore]
    fn regen_shared_golden() {
        let built = conformance_build();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrss_build_golden.txt"
        );
        std::fs::write(path, super::golden_text(&built)).expect("write shared golden");
    }
}

/// Serializes a built split set in the shared golden format (`name <hex>` per line).
#[cfg(test)]
pub(crate) fn golden_text(built: &BuiltSplitSet) -> String {
    let mut out = String::new();
    let mut line = |name: &str, bytes: &[u8]| {
        out.push_str(name);
        out.push(' ');
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out.push('\n');
    };
    line("manifest", &built.manifest);
    for (name, bytes) in &built.splits {
        line(name, bytes);
    }
    for (name, bytes) in &built.facets {
        line(name, bytes);
    }
    out
}

/// Term-bodied cross-language conformance: the same shared-golden discipline as the trigram
/// builder, over `go/testdata/rrti_term_split_golden.txt`. Asserted byte-for-byte by Go's
/// `termsplitsetbuild_test.go` — the guard for the Go RRTI writer (front-coded dict blocks,
/// router FST, postings) AND its tokenizer (stemmer/stop-words/Unicode lowercasing).
#[cfg(all(test, feature = "terms"))]
mod term_conformance {
    use super::term_conformance_build;
    use std::collections::HashMap;

    fn golden() -> HashMap<String, Vec<u8>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrti_term_split_golden.txt"
        );
        let text = std::fs::read_to_string(path).expect("read shared term golden");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let (name, hex) = l.split_once(' ').expect("`name <hex>` line");
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect::<Vec<u8>>();
                (name.to_string(), bytes)
            })
            .collect()
    }

    #[test]
    fn term_split_build_matches_shared_golden() {
        let g = golden();
        let built = term_conformance_build();
        assert_eq!(
            &built.manifest,
            g.get("manifest").expect("manifest golden present"),
            "term RRSS manifest drifted from the shared golden"
        );
        for (name, bytes) in built.splits.iter().chain(built.facets.iter()) {
            assert_eq!(
                bytes,
                g.get(name)
                    .unwrap_or_else(|| panic!("no golden for {name}")),
                "{name} bytes drifted from the shared golden"
            );
        }
        assert_eq!(
            g.len(),
            1 + built.splits.len() + built.facets.len(),
            "term golden entry count drifted"
        );
    }

    /// Regenerate with `cargo test --features "splits terms" regen_term_split_golden -- --ignored`.
    #[test]
    #[ignore]
    fn regen_term_split_golden() {
        let built = term_conformance_build();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrti_term_split_golden.txt"
        );
        std::fs::write(path, super::golden_text(&built)).expect("write shared term golden");
    }

    use super::term_conformance_cs_build;

    fn cs_golden() -> HashMap<String, Vec<u8>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrti_term_split_cs_golden.txt"
        );
        let text = std::fs::read_to_string(path).expect("read shared term cs golden");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let (name, hex) = l.split_once(' ').expect("`name <hex>` line");
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect::<Vec<u8>>();
                (name.to_string(), bytes)
            })
            .collect()
    }

    #[test]
    fn term_split_cs_build_matches_shared_golden() {
        let g = cs_golden();
        let built = term_conformance_cs_build();
        assert_eq!(
            &built.manifest,
            g.get("manifest").expect("cs manifest golden present"),
            "case-sensitive term RRSS manifest drifted from the shared golden"
        );
        for (name, bytes) in built.splits.iter().chain(built.facets.iter()) {
            assert_eq!(
                bytes,
                g.get(name)
                    .unwrap_or_else(|| panic!("no cs golden for {name}")),
                "{name} bytes drifted from the shared cs golden"
            );
        }
        assert_eq!(
            g.len(),
            1 + built.splits.len() + built.facets.len(),
            "cs term golden entry count drifted"
        );
    }

    /// Regenerate with `cargo test --features "splits terms" regen_term_split_cs_golden -- --ignored`.
    #[test]
    #[ignore]
    fn regen_term_split_cs_golden() {
        let built = term_conformance_cs_build();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrti_term_split_cs_golden.txt"
        );
        std::fs::write(path, super::golden_text(&built)).expect("write shared term cs golden");
    }
}

/// Geometric-cap conformance: the doubling seal boundaries shared with Go.
#[cfg(test)]
mod geo_conformance {
    use super::geo_conformance_build;

    fn golden_path() -> &'static str {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../testdata/rrss_geo_build_golden.txt"
        )
    }

    #[test]
    fn geometric_build_matches_shared_golden() {
        let built = geo_conformance_build();
        assert!(built.splits.len() >= 3, "fixture should seal several tiers");
        let text = std::fs::read_to_string(golden_path()).expect("read geo golden");
        assert_eq!(
            text,
            super::golden_text(&built),
            "geometric split build drifted from the shared golden"
        );
    }

    /// The behavioral half: doubling caps must seal FEWER splits than the same
    /// corpus under the flat base cap — the whole point of geometric tiering.
    #[test]
    fn geometric_seals_fewer_splits_than_flat() {
        use super::{Policy, SplitBuildConfig, SplitSetBuilder};
        let build = |byte_cap_max: u64| {
            let words = [
                "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota",
                "kappa",
            ];
            // Base 400: comfortably above the largest single document (the flat
            // arm must not trip the degenerate single-doc-over-cap guard).
            let mut b = SplitSetBuilder::new(SplitBuildConfig {
                policy: Policy::Tiered,
                byte_cap: 400,
                byte_cap_max,
                gram_size: 3,
                head_boundary: 0,
                stride: 0,
                name_prefix: "geo".to_string(),
                sortcol: None,
                bloom_bits_per_key: 8,
                case_sensitive: false,
            });
            for i in 0..24 {
                let text = format!(
                    "{} {}",
                    words[i % words.len()],
                    words[(i + 3) % words.len()]
                );
                b.add_faceted(&text, &[("year".to_string(), (2018 + (i % 5)).to_string())])
                    .unwrap();
            }
            b.finish().unwrap().splits.len()
        };
        assert!(
            build(1600) < build(0),
            "doubling caps should reduce the split count"
        );
    }

    /// Regenerate with `cargo test --features splits regen_geo_golden -- --ignored`.
    #[test]
    #[ignore]
    fn regen_geo_golden() {
        let built = geo_conformance_build();
        std::fs::write(golden_path(), super::golden_text(&built)).expect("write geo golden");
    }
}
