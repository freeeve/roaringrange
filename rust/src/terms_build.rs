//! Native writer for the `RRTI` term-level inverted index — the build-side mirror
//! of [`crate::terms`], emitting the byte layout in `TERMS.md`.
//!
//! Postings are portable RoaringBitmaps split into head/tail by the same
//! [`crate::build::split_posting`] the trigram index uses, so the term index
//! inherits the rank-ordered head and the whole reader posting path unchanged —
//! only the dictionary (an FST keyed by whole term) is new.

use crate::build::split_posting;
use crate::terms::{Language, Tokenizer, FLAG_CASE_SENSITIVE, FLAG_STEMMED, FLAG_STOPWORDS};
use crate::terms_dict::{pack_loc, BlockWriter, DEFAULT_DICT_BLOCK_CAP, SIZE_BITS};
use fst::MapBuilder;
use roaring::RoaringBitmap;
use std::collections::BTreeMap;
use std::io::{self, Write};

/// `RRTI` magic.
const MAGIC: &[u8; 4] = b"RRTI";
/// Format version written into the header: v2 is the blocked, front-coded
/// dictionary with a small resident FST routing over block boundaries.
const VERSION: u16 = 2;

/// Build-time configuration for an `RRTI` index. The tokenizer settings (`language`
/// for Snowball stemming, `stopwords`) are recorded in the header so the reader
/// tokenizes queries identically — the term-index correctness invariant.
#[derive(Debug, Clone, Copy)]
pub struct TermIndexConfig {
    /// Doc-ID head/tail split (a multiple of 65536, e.g. [`crate::build::DEFAULT_HEAD_BOUNDARY`]).
    pub head_boundary: u32,
    /// Optional Snowball stemmer language; `None` builds an unstemmed index.
    pub language: Option<Language>,
    /// Remove common stop words from the index (and, symmetrically, from queries).
    pub stopwords: bool,
    /// Build a **case-sensitive** index: terms are not lowercased at index or query time
    /// (recorded in the header so the reader skips query-side folding too). The default
    /// (`false`) case-folds, reproducing the historical behavior and keeping the output
    /// byte-identical.
    pub case_sensitive: bool,
    /// Dictionary block byte cap — the dictionary is partitioned into front-coded
    /// blocks sized for one cheap ranged GET. `0` selects the default
    /// (~[`DEFAULT_DICT_BLOCK_CAP`] bytes / a few hundred terms per block).
    pub block_cap: usize,
}

/// Builds an unstemmed `RRTI` term index over `(doc_id, text)` documents and writes it
/// to `w`. Doc IDs should be the shared rank-order IDs (so facets/records/vector
/// compose); `head_boundary` is the doc-ID head/tail split (a multiple of 65536, e.g.
/// [`crate::build::DEFAULT_HEAD_BOUNDARY`]). For Snowball stemming or stop-word removal,
/// use [`write_term_index_with`].
pub fn write_term_index<W: Write>(
    w: W,
    docs: &[(u32, &str)],
    head_boundary: u32,
) -> io::Result<()> {
    write_term_index_with(
        w,
        docs,
        &TermIndexConfig {
            head_boundary,
            language: None,
            stopwords: false,
            case_sensitive: false,
            block_cap: 0,
        },
    )
}

/// Builds an `RRTI` term index with an explicit [`TermIndexConfig`] over an in-memory
/// `docs` slice and writes it to `w`. Convenience over [`TermIndexBuilder`]; for a
/// large corpus, stream documents into a [`TermIndexBuilder`] instead so the text is
/// never all held at once.
pub fn write_term_index_with<W: Write>(
    w: W,
    docs: &[(u32, &str)],
    config: &TermIndexConfig,
) -> io::Result<()> {
    let mut builder = TermIndexBuilder::new(config);
    for &(doc, text) in docs {
        builder.add(doc, text);
    }
    builder.finish(w)
}

/// A streaming `RRTI` builder. Feed documents incrementally with [`add`](Self::add)
/// (or [`add_text`](Self::add) repeatedly): each is tokenized through the configured
/// [`Tokenizer`] and its text discarded, so only the `term -> doc-id bitmap` postings
/// grow — never all the corpus text at once. [`finish`](Self::finish) writes the index.
/// The tokenizer config (head boundary + stemming/stop words) is fixed at construction
/// and recorded in the header for build/query symmetry.
pub struct TermIndexBuilder {
    postings: BTreeMap<String, RoaringBitmap>,
    tokenizer: Tokenizer,
    head_boundary: u32,
    language: Option<Language>,
    stopwords: bool,
    case_normalization: bool,
    block_cap: usize,
}

impl TermIndexBuilder {
    /// Creates an empty builder with the given config.
    pub fn new(config: &TermIndexConfig) -> Self {
        TermIndexBuilder {
            postings: BTreeMap::new(),
            tokenizer: Tokenizer::new(config.language, config.stopwords, !config.case_sensitive),
            head_boundary: config.head_boundary,
            language: config.language,
            stopwords: config.stopwords,
            case_normalization: !config.case_sensitive,
            block_cap: config.block_cap,
        }
    }

    /// Tokenizes `text` and records `doc` under each resulting term. The text is not
    /// retained — only the postings grow. `doc` should be the shared rank-order ID.
    pub fn add(&mut self, doc: u32, text: &str) {
        for term in self.tokenizer.tokenize(text) {
            self.postings.entry(term).or_default().insert(doc);
        }
    }

    /// Number of distinct terms accumulated so far.
    pub fn len(&self) -> usize {
        self.postings.len()
    }

    /// Whether no terms have been added.
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    /// Writes the accumulated `RRTI` index to `w`, consuming the builder. Delegates to
    /// [`write_term_index_from_postings`] so the on-disk layout has a single source of truth
    /// shared with the split-set term builder.
    pub fn finish<W: Write>(self, w: W) -> io::Result<()> {
        write_term_index_from_postings(
            w,
            self.postings,
            self.head_boundary,
            self.language,
            self.stopwords,
            self.case_normalization,
            self.block_cap,
        )
    }
}

/// Writes an `RRTI` v2 term index body to `w` from already-accumulated `term -> doc-id bitmap`
/// `postings`. Terms are drained in byte-lexicographic order (a `BTreeMap`, exactly the
/// blocked dictionary's required order), and each posting bitmap is freed right after it is
/// serialized, so peak memory is the postings region rather than double it.
///
/// The dictionary is partitioned into byte-capped, front-coded blocks
/// ([`crate::terms_dict`]) that the reader range-fetches one at a time; a small **router FST**
/// maps each block's last term to its byte range, so only O(#blocks) — not O(vocab) — is
/// resident. The postings region is byte-identical to v1. Doc IDs must be the shared rank-order
/// IDs; `head_boundary` is the head/tail split; the tokenizer settings (`language`/`stopwords`/
/// `case_normalization`) are recorded in the header so the reader tokenizes queries identically;
/// `block_cap` is the dict block byte cap (`0` selects [`DEFAULT_DICT_BLOCK_CAP`]). Shared by
/// [`TermIndexBuilder::finish`] and the split-set term builder.
pub(crate) fn write_term_index_from_postings<W: Write>(
    w: W,
    postings: BTreeMap<String, RoaringBitmap>,
    head_boundary: u32,
    language: Option<Language>,
    stopwords: bool,
    case_normalization: bool,
    block_cap: usize,
) -> io::Result<()> {
    let mut region: Vec<u8> = Vec::new();
    let mut sw = TermIndexStreamWriter::new(
        &mut region,
        head_boundary,
        language,
        stopwords,
        case_normalization,
        block_cap,
    );
    for (term, bm) in postings {
        let (head, tail) = split_posting(&bm, head_boundary);
        sw.push(&term, &head, &tail)?;
    }
    let (header, router_bytes, blocks) = sw.finish_meta()?;
    let mut w = w;
    w.write_all(&header)?;
    w.write_all(&router_bytes)?;
    for b in &blocks {
        w.write_all(b)?;
    }
    w.write_all(&region)?;
    Ok(())
}

/// Streaming `RRTI` writer: terms arrive in sorted byte order with their already
/// split-and-serialized head/tail postings; the postings region streams to the
/// caller's sink (typically a temp file — the multi-GB half of a big index), while
/// the dictionary blocks accumulate in memory (term-proportional, small).
/// [`finish_into`](Self::finish_into) then assembles
/// `[header][router FST][dict blocks][region]`. The batch
/// `write_term_index_from_postings` is a thin wrapper, so the two cannot drift.
pub struct TermIndexStreamWriter<W: Write> {
    region: W,
    region_len: u64,
    blocks: BlockWriter,
    term_count: u64,
    head_boundary: u32,
    language: Option<Language>,
    stopwords: bool,
    case_normalization: bool,
    block_cap: usize,
}

impl<W: Write> TermIndexStreamWriter<W> {
    /// Creates a writer streaming the postings region to `region_sink`.
    /// `block_cap` of `0` selects the default dictionary block cap.
    pub fn new(
        region_sink: W,
        head_boundary: u32,
        language: Option<Language>,
        stopwords: bool,
        case_normalization: bool,
        block_cap: usize,
    ) -> Self {
        TermIndexStreamWriter {
            region: region_sink,
            region_len: 0,
            blocks: BlockWriter::new(block_cap),
            term_count: 0,
            head_boundary,
            language,
            stopwords,
            case_normalization,
            block_cap,
        }
    }

    /// Appends one term's posting block (`[tail_size u32 LE][head][tail]`,
    /// the halves from [`split_posting`]). Terms MUST arrive in ascending byte
    /// order — the dictionary's required order.
    pub fn push(&mut self, term: &str, head: &[u8], tail: &[u8]) -> io::Result<()> {
        let head_off = self.region_len;
        if head.len() >= (1usize << SIZE_BITS) {
            return Err(io::Error::other(format!(
                "term {term:?}: head posting {} B exceeds the 24-bit size limit",
                head.len()
            )));
        }
        if head_off >= (1u64 << (64 - SIZE_BITS)) {
            return Err(io::Error::other(
                "postings region exceeds the 40-bit offset limit",
            ));
        }
        self.region.write_all(&(tail.len() as u32).to_le_bytes())?;
        self.region.write_all(head)?;
        self.region.write_all(tail)?;
        self.region_len += 4 + head.len() as u64 + tail.len() as u64;
        self.blocks
            .push(term.as_bytes(), head_off, head.len() as u64);
        self.term_count += 1;
        Ok(())
    }

    /// Number of terms pushed so far.
    pub fn term_count(&self) -> u64 {
        self.term_count
    }

    /// Bytes streamed into the postings region so far.
    pub fn region_len(&self) -> u64 {
        self.region_len
    }

    /// Finishes the dictionary + router and writes the COMPLETE index to `w`:
    /// `[header][router][dict blocks]` followed by `region` — the caller hands
    /// back the postings-region bytes it sank (e.g. by copying its temp file in;
    /// pass the sink's contents here, or use this with an in-memory region).
    pub fn finish_into<O: Write>(self, mut w: O, region: &[u8]) -> io::Result<()> {
        debug_assert_eq!(region.len() as u64, self.region_len);
        let (header, router_bytes, blocks) = self.finish_meta()?;
        w.write_all(&header)?;
        w.write_all(&router_bytes)?;
        for b in &blocks {
            w.write_all(b)?;
        }
        w.write_all(region)?;
        Ok(())
    }

    /// Finishes the dictionary + router, returning `(header, router, dict block
    /// bytes)` for callers that assemble the file themselves (header + router +
    /// blocks, then their streamed region — e.g. an `io::copy` from the temp file).
    #[allow(clippy::type_complexity)]
    pub fn finish_meta(self) -> io::Result<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>)> {
        let blocks = self.blocks.finish();
        let mut router = MapBuilder::memory();
        let mut dict_len: u64 = 0;
        for b in &blocks {
            let block_len = b.bytes.len() as u64;
            if block_len >= (1u64 << SIZE_BITS) {
                return Err(io::Error::other(
                    "dict block exceeds the 24-bit block-length limit",
                ));
            }
            if b.off >= (1u64 << (64 - SIZE_BITS)) {
                return Err(io::Error::other(
                    "dict region exceeds the 40-bit block-offset limit",
                ));
            }
            router
                .insert(&b.last_term, pack_loc(b.off, block_len))
                .map_err(|e| io::Error::other(format!("router fst insert: {e}")))?;
            dict_len += block_len;
        }
        let router_bytes = router
            .into_inner()
            .map_err(|e| io::Error::other(format!("router fst finish: {e}")))?;

        // Header (40 B): `flags` records the tokenizer (stemmed / stop-words /
        // case-sensitive); the first reserved byte (offset 36) carries the stemmer
        // language so the reader rebuilds it. `routerLen`/`dictLen` locate the dict
        // region and postings.
        let flags = (if self.language.is_some() {
            FLAG_STEMMED
        } else {
            0
        }) | (if self.stopwords { FLAG_STOPWORDS } else { 0 })
            | (if self.case_normalization {
                0
            } else {
                FLAG_CASE_SENSITIVE
            });
        let block_cap_used = if self.block_cap == 0 {
            DEFAULT_DICT_BLOCK_CAP
        } else {
            self.block_cap
        } as u32;
        let term_count: u32 = self
            .term_count
            .try_into()
            .map_err(|_| io::Error::other("RRTI term count exceeds the 32-bit limit"))?;
        let mut reserved = [0u8; 4];
        reserved[0] = self.language.map_or(0, |l| l.to_u8());
        let mut header = Vec::with_capacity(40);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&flags.to_le_bytes());
        header.extend_from_slice(&term_count.to_le_bytes());
        header.extend_from_slice(&self.head_boundary.to_le_bytes());
        header.extend_from_slice(&(router_bytes.len() as u64).to_le_bytes());
        header.extend_from_slice(&dict_len.to_le_bytes());
        header.extend_from_slice(&block_cap_used.to_le_bytes());
        header.extend_from_slice(&reserved); // reserved[0] (offset 36) = stemmer language; pads to 40 B
        Ok((
            header,
            router_bytes,
            blocks.into_iter().map(|b| b.bytes).collect(),
        ))
    }
}
