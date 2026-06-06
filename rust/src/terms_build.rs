//! Native writer for the `RRTI` term-level inverted index — the build-side mirror
//! of [`crate::terms`], emitting the byte layout in `TERMS.md`.
//!
//! Postings are portable RoaringBitmaps split into head/tail by the same
//! [`crate::build::split_posting`] the trigram index uses, so the term index
//! inherits the rank-ordered head and the whole reader posting path unchanged —
//! only the dictionary (an FST keyed by whole term) is new.

use crate::build::split_posting;
use crate::terms::{Language, Tokenizer, FLAG_STEMMED, FLAG_STOPWORDS};
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
    block_cap: usize,
}

impl TermIndexBuilder {
    /// Creates an empty builder with the given config.
    pub fn new(config: &TermIndexConfig) -> Self {
        TermIndexBuilder {
            postings: BTreeMap::new(),
            tokenizer: Tokenizer::new(config.language, config.stopwords),
            head_boundary: config.head_boundary,
            language: config.language,
            stopwords: config.stopwords,
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
/// IDs; `head_boundary` is the head/tail split; the tokenizer settings (`language`/`stopwords`)
/// are recorded in the header so the reader tokenizes queries identically; `block_cap` is the
/// dict block byte cap (`0` selects [`DEFAULT_DICT_BLOCK_CAP`]). Shared by
/// [`TermIndexBuilder::finish`] and the split-set term builder.
pub(crate) fn write_term_index_from_postings<W: Write>(
    mut w: W,
    postings: BTreeMap<String, RoaringBitmap>,
    head_boundary: u32,
    language: Option<Language>,
    stopwords: bool,
    block_cap: usize,
) -> io::Result<()> {
    let term_count = postings.len() as u32;

    // Lay out the (unchanged) postings region and front-code the dictionary into
    // blocks in one sorted, draining pass. Each posting block is
    // `[tail_size: u32 LE][head bytes][tail bytes]`; the dict records each term's
    // `(head_off within the postings region, head_size)`.
    let mut region: Vec<u8> = Vec::new();
    let mut blocks = BlockWriter::new(block_cap);
    for (term, bm) in postings {
        let (head, tail) = split_posting(&bm, head_boundary);
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
        blocks.push(term.as_bytes(), head_off, head_size as u64);
    }
    let blocks = blocks.finish();

    // Router FST: each block's last term -> `(block_off << 24) | block_len`, where
    // `block_off` is relative to the dict region. Keys are already sorted (blocks
    // are in term order) and distinct, as `MapBuilder` requires.
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

    // Header (40 B): `flags` records the tokenizer (stemmed / stop-words); the
    // first reserved byte (offset 36) carries the stemmer language so the reader
    // rebuilds it. `routerLen`/`dictLen` locate the dict region and postings.
    let flags = (if language.is_some() { FLAG_STEMMED } else { 0 })
        | (if stopwords { FLAG_STOPWORDS } else { 0 });
    let block_cap_used = if block_cap == 0 {
        DEFAULT_DICT_BLOCK_CAP
    } else {
        block_cap
    } as u32;
    let mut reserved = [0u8; 4];
    reserved[0] = language.map_or(0, |l| l.to_u8());
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&flags.to_le_bytes())?;
    w.write_all(&term_count.to_le_bytes())?;
    w.write_all(&head_boundary.to_le_bytes())?;
    w.write_all(&(router_bytes.len() as u64).to_le_bytes())?;
    w.write_all(&dict_len.to_le_bytes())?;
    w.write_all(&block_cap_used.to_le_bytes())?;
    w.write_all(&reserved)?; // reserved[0] (offset 36) = stemmer language; pads to 40 B
    w.write_all(&router_bytes)?;
    for b in &blocks {
        w.write_all(&b.bytes)?;
    }
    w.write_all(&region)?;
    Ok(())
}
