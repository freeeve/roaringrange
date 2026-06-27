//! The `RRTI` range-fetchable term-level inverted index reader.
//!
//! A new, additive member of the roaringrange format family (next to the trigram
//! `RRS`, vector `RRVI`, facet `RRSF`, record `RRSR`, and lookup `RRIL`), sharing
//! the same doc-ID space so it composes with all of them. Where `RRS` keys a
//! sorted-`u64` dictionary by trigram, `RRTI` keys an **FST term dictionary** by
//! whole word — one posting per query term instead of ~(L−2) per word.
//!
//! Layout (`TERMS.md`): `[header][FST dictionary][postings region]`. The postings
//! region reuses the `RRS` `[head][tail]` roaring split (the head holds the
//! top-ranked docs, so top-K is free). Boot range-fetches the small FST blob once
//! and holds it in memory, so a term resolves to its posting location with **zero**
//! further reads; the head posting is one ranged read, the tail lazy.
//!
//! The FST output `u64` packs `(head_off << 24) | head_size` — the byte offset of
//! the term's posting block within the postings region (40 bits → 1 TB) and the
//! head posting's length (24 bits → 16 MB, ample for one rank-head). Each block is
//! `[tail_size: u32 LE][head bytes][tail bytes]`, so fetching the head also yields
//! the tail's length for the lazy second wave.

use crate::fetch::RangeFetch;
use crate::index::{deserialize, read_u16, read_u32, read_u64, IndexError};
use crate::terms_dict::{iter_block, scan_block, unpack_loc};
use fst::{IntoStreamer, Map, Streamer};
use futures::future::join_all;
use roaring::RoaringBitmap;
use rust_stemmers::{Algorithm, Stemmer};

/// `RRTI` magic.
const MAGIC: &[u8; 4] = b"RRTI";
/// Header size in bytes: magic[4] + version[2] + flags[2] + termCount[4] +
/// headBoundary[4] + routerLen[8] + dictLen[8] + blockCap[4] + reserved[4]
/// (stemmer language at offset 36). Kept in sync with the builder.
const HEADER_SIZE: usize = 40;
/// Format version written into / accepted from the header (v2 = blocked dictionary
/// with a router FST; the original monolithic-FST v1 is no longer read).
const VERSION: u16 = 2;

/// Splits `text` into lowercased terms, mirroring Tantivy's `SimpleTokenizer`
/// (a token is a maximal run of `char::is_alphanumeric`) followed by its
/// `LowerCaser` (`char::to_lowercase`). The builder and the reader call this same
/// function, so a query tokenizes identically to the indexed text — the one
/// correctness invariant of a term index. Stop-word and stemming filters slot in
/// after this base step in a later phase. Equivalent to [`tokenize_with`] with case
/// folding on — the default — so the public signature is unchanged.
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_with(text, true)
}

/// The base SimpleTokenizer with explicit case folding: a token is a maximal run of
/// `char::is_alphanumeric`, lowercased via `char::to_lowercase` only when `case_fold`
/// is true. With `case_fold` false the token text is kept verbatim (a case-sensitive
/// index), so the builder and reader must agree on the flag; it is recorded in the
/// header (`FLAG_CASE_SENSITIVE`) and rebuilt by [`Tokenizer::from_header`].
pub fn tokenize_with(text: &str, case_fold: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            if case_fold {
                cur.extend(c.to_lowercase());
            } else {
                cur.push(c);
            }
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Header `flags` bit: the index's terms were Snowball-stemmed, so queries must stem
/// identically (the stemmer language is the header's language byte).
pub(crate) const FLAG_STEMMED: u16 = 1;
/// Header `flags` bit: stop words were removed from the index, so queries drop them too.
pub(crate) const FLAG_STOPWORDS: u16 = 2;
/// Header `flags` bit: the index is **case-sensitive** — its terms were not lowercased,
/// so queries must skip lowercasing too. Unset (the default) means case-folded, which
/// keeps every default-built index byte-identical to before this flag existed.
pub(crate) const FLAG_CASE_SENSITIVE: u16 = 4;

/// One source-of-truth table of every Snowball stemmer `rust-stemmers` provides —
/// `Variant = on-disk byte, "snowball name", "iso-639-1", StemmerAlgorithm` — which
/// the macro expands into the [`Language`] enum and all of its conversions. Adding a
/// language is one row here: pick the next free byte. **The byte is the stable header
/// encoding and must never change or be reused**, so append new languages rather than
/// renumbering (the listed order is otherwise arbitrary).
macro_rules! languages {
    ($($variant:ident = $byte:literal, $name:literal, $iso:literal, $algo:ident;)+) => {
        /// A stemmer language, recorded in the header so the reader stems a query
        /// exactly as the builder stemmed the corpus. Covers the full Snowball set;
        /// the on-disk byte ([`from_u8`](Self::from_u8)) is stable across versions.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Language {
            $(
                #[doc = concat!("Snowball \"", $name, "\" (ISO-639-1 \"", $iso, "\").")]
                $variant,
            )+
        }

        impl Language {
            /// The on-disk language byte. Thin shim over the canonical
            /// [`From<Language> for u8`](Language#impl-From<Language>-for-u8).
            pub fn to_u8(self) -> u8 {
                u8::from(self)
            }

            /// Maps an on-disk language byte back to a [`Language`], or `None` when
            /// the byte names no known stemmer (an older/newer index, or an
            /// unstemmed one). Intentionally lenient — unknown ⇒ no stemmer — so it
            /// stays an `Option` rather than a `TryFrom`.
            pub fn from_u8(b: u8) -> Option<Language> {
                match b {
                    $($byte => Some(Language::$variant),)+
                    _ => None,
                }
            }

            /// Parses a human/CLI language code into a [`Language`], or `None` if
            /// unrecognized. Accepts the canonical Snowball name or the ISO-639-1
            /// code, case-insensitively (e.g. `"english"`/`"en"`, `"spanish"`/
            /// `"es"`). The single source for string→language that every builder
            /// front-end (CLI, Python, examples) routes through, so a new language
            /// is wired in exactly one place.
            pub fn from_code(code: &str) -> Option<Language> {
                match code.trim().to_ascii_lowercase().as_str() {
                    $($name | $iso => Some(Language::$variant),)+
                    _ => None,
                }
            }

            /// The canonical Snowball name for this language — the long-form inverse
            /// of [`from_code`](Self::from_code).
            pub fn as_code(self) -> &'static str {
                match self {
                    $(Language::$variant => $name,)+
                }
            }

            fn algorithm(self) -> Algorithm {
                match self {
                    $(Language::$variant => Algorithm::$algo,)+
                }
            }
        }

        impl From<Language> for u8 {
            /// The stable on-disk language byte (the header encoding the reader
            /// reads back).
            fn from(l: Language) -> u8 {
                match l {
                    $(Language::$variant => $byte,)+
                }
            }
        }
    };
}

languages! {
    English = 1, "english", "en", English;
    Spanish = 2, "spanish", "es", Spanish;
    Arabic = 3, "arabic", "ar", Arabic;
    Danish = 4, "danish", "da", Danish;
    Dutch = 5, "dutch", "nl", Dutch;
    Finnish = 6, "finnish", "fi", Finnish;
    French = 7, "french", "fr", French;
    German = 8, "german", "de", German;
    Greek = 9, "greek", "el", Greek;
    Hungarian = 10, "hungarian", "hu", Hungarian;
    Italian = 11, "italian", "it", Italian;
    Norwegian = 12, "norwegian", "no", Norwegian;
    Portuguese = 13, "portuguese", "pt", Portuguese;
    Romanian = 14, "romanian", "ro", Romanian;
    Russian = 15, "russian", "ru", Russian;
    Swedish = 16, "swedish", "sv", Swedish;
    Tamil = 17, "tamil", "ta", Tamil;
    Turkish = 18, "turkish", "tr", Turkish;
}

/// Common English stop words, sorted for binary search. Removed from the index (and
/// from queries) only when the index was built with stop-word removal.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "had", "has", "have",
    "he", "in", "is", "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were",
    "which", "will", "with",
];

/// Whether the lowercased token `t` is a stop word.
fn is_stop_word(t: &str) -> bool {
    STOP_WORDS.binary_search(&t).is_ok()
}

/// The configured token-filter chain applied after the base [`tokenize`]
/// (SimpleTokenizer + LowerCaser): optional stop-word removal, then optional Snowball
/// stemming. The builder (from its config) and the reader (from the header flags) build
/// this identically, guaranteeing a query tokenizes exactly as the corpus did — the one
/// correctness invariant of a term index.
pub struct Tokenizer {
    stemmer: Option<Stemmer>,
    stopwords: bool,
    language: Option<Language>,
    case_fold: bool,
}

impl Tokenizer {
    /// The base SimpleTokenizer + LowerCaser with no stemming or stop-word removal
    /// (an unstemmed index, or a pre-stemming v1 file whose flags are zero).
    pub fn plain() -> Self {
        Tokenizer::new(None, false, true)
    }

    /// A tokenizer with the given optional stemmer language, stop-word removal, and
    /// case folding (`case_fold == false` builds a case-sensitive index, keeping token
    /// text verbatim).
    pub fn new(language: Option<Language>, stopwords: bool, case_fold: bool) -> Self {
        Tokenizer {
            stemmer: language.map(|l| Stemmer::create(l.algorithm())),
            stopwords,
            language,
            case_fold,
        }
    }

    /// The `(language, stopwords, case_fold)` triple this tokenizer was built with —
    /// lets build tooling construct identical tokenizers (e.g. one per worker thread).
    pub fn spec(&self) -> (Option<Language>, bool, bool) {
        (self.language, self.stopwords, self.case_fold)
    }

    /// Whether this tokenizer case-folds (lowercases) tokens. The reader's prefix
    /// paths consult it so a case-sensitive index matches prefixes verbatim.
    pub fn case_fold(&self) -> bool {
        self.case_fold
    }

    /// Builds the tokenizer the header describes from its `flags` + `language` byte.
    fn from_header(flags: u16, language: u8) -> Self {
        let lang = if flags & FLAG_STEMMED != 0 {
            Language::from_u8(language)
        } else {
            None
        };
        Tokenizer::new(
            lang,
            flags & FLAG_STOPWORDS != 0,
            flags & FLAG_CASE_SENSITIVE == 0,
        )
    }

    /// Tokenizes `text`: base tokens (lowercased iff this tokenizer case-folds), then
    /// drop stop words (if enabled), then stem each surviving token (if a stemmer is
    /// configured).
    pub fn tokenize(&self, text: &str) -> Vec<String> {
        tokenize_with(text, self.case_fold)
            .into_iter()
            .filter(|t| !(self.stopwords && is_stop_word(t)))
            .map(|t| match &self.stemmer {
                Some(s) => s.stem(&t).into_owned(),
                None => t,
            })
            .collect()
    }
}

/// Walks one front-coded dictionary block (fetched via the ranges
/// [`TermIndex::dict_block_locs`] reports), yielding each
/// `(term, head_off, head_size)` in term order. Build-tooling surface for
/// streaming the full vocabulary without materializing it (see
/// [`TermIndex::dict_terms`]); `head_size` lets a sequential consumer pace a
/// lockstep read of the postings region.
pub fn parse_dict_block(block: &[u8]) -> impl Iterator<Item = (Vec<u8>, u64, usize)> + '_ {
    iter_block(block)
}

/// A term's head posting plus the location of its (lazily fetched) tail.
/// Crate-visible so the BM25 path can rerank head candidates without tails.
pub(crate) struct HeadBlock {
    pub(crate) head: RoaringBitmap,
    pub(crate) tail_off: u64,
    pub(crate) tail_size: usize,
}

/// A range-fetchable `RRTI` term index. Boot holds only the small **router FST**
/// (mapping each dict block's last term to its byte range — O(#blocks), not
/// O(vocab)); each query range-fetches the front-coded dict blocks and the posting
/// blocks it needs.
pub struct TermIndex<F: RangeFetch> {
    fetch: F,
    /// Resident router: each dict block's last term -> `(block_off, block_len)`
    /// (relative to `dict_start`).
    router: Map<Vec<u8>>,
    /// Absolute byte offset of the front-coded dict-blocks region.
    dict_start: u64,
    /// Byte offset of the postings region.
    postings_offset: u64,
    /// The head/tail doc-ID split baked into the postings (metadata).
    head_boundary: u32,
    /// Number of distinct terms in the dictionary.
    term_count: u32,
    /// The tokenizer the index was built with (from the header flags); queries are
    /// tokenized through it so they match the indexed terms.
    tokenizer: Tokenizer,
}

impl<F: RangeFetch> TermIndex<F> {
    /// Boots the index: reads the header, then the small resident **router FST** in
    /// one ranged read (the front-coded dict blocks stay on the wire). Subsequent
    /// queries fetch only the dict blocks and posting blocks they need.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        if header.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRTI header"));
        }
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let flags = read_u16(&header, 6);
        let term_count = read_u32(&header, 8);
        let head_boundary = read_u32(&header, 12);
        let router_len = read_u64(&header, 16);
        let dict_len = read_u64(&header, 24);
        // blockCap (offset 32) is informational; the language byte is at offset 36.
        let tokenizer = Tokenizer::from_header(flags, header[36]);
        let router_bytes = fetch.read(HEADER_SIZE as u64, router_len as usize).await?;
        let router =
            Map::new(router_bytes).map_err(|_| IndexError::Malformed("RRTI invalid router FST"))?;
        // `Map::new` validates the header/footer but not every interior node, so a
        // corrupted node would `assert!`-panic deep in the fst crate when a query
        // streams it (and wasm aborts on panic — uncatchable). Verify the FST's
        // CRC32 up front: the router is small and resident, so this one-time scan is
        // cheap, and it rejects any byte-level corruption before a query reaches it.
        router
            .as_fst()
            .verify()
            .map_err(|_| IndexError::Malformed("RRTI router FST failed checksum"))?;
        // `router_len`/`dict_len` are untrusted header fields; saturate the offset
        // sums so a crafted header can't overflow here — a saturated offset simply
        // fails the later range fetch with an out-of-range error.
        let dict_start = (HEADER_SIZE as u64).saturating_add(router_len);
        Ok(Self {
            fetch,
            router,
            dict_start,
            postings_offset: dict_start.saturating_add(dict_len),
            head_boundary,
            term_count,
            tokenizer,
        })
    }

    /// Number of distinct terms in the dictionary. `u32` like every other
    /// roaringrange entity count (the doc-ID space is `u32`, so counts can't
    /// exceed it).
    pub fn len(&self) -> u32 {
        self.term_count
    }

    /// Reports whether the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.term_count == 0
    }

    /// The doc-ID head/tail boundary baked into the postings.
    pub fn head_boundary(&self) -> u32 {
        self.head_boundary
    }

    /// Bytes held resident after boot — the header plus the block router FST
    /// (`[0, dictStart)`). This is O(#blocks), not O(vocabulary): the front-coded
    /// dict blocks stay on the wire and are range-fetched per query. The headline
    /// scaling win over the old whole-term FST, which was resident in full.
    pub fn resident_len(&self) -> u64 {
        self.dict_start
    }

    /// Byte range of the dict block that could contain `term` — the first block
    /// whose last term `>= term` (a resident router-FST lookup, no fetch). `None`
    /// when `term` sorts past every block (so it is absent).
    fn block_range_for(&self, term: &str) -> Option<(u64, usize)> {
        let mut stream = self.router.range().ge(term.as_bytes()).into_stream();
        let (_, packed) = stream.next()?;
        let (block_off, block_len) = unpack_loc(packed);
        Some((self.dict_start + block_off, block_len))
    }

    /// Resolves query `terms` to their posting locations `(head_off, head_size)`.
    /// In strict mode (`lenient == false`) any absent term abandons the query with
    /// `Ok(None)` — the strict-AND result is then empty. In lenient mode the absent
    /// terms are dropped and the present ones returned (possibly an empty `Vec`, but
    /// always `Some`), which is what the min-should-match path counts toward M.
    /// Issues one concurrent wave of (deduped) dict-block reads, then scans each
    /// front-coded block.
    async fn resolve_locs(
        &self,
        terms: &[String],
        lenient: bool,
    ) -> Result<Option<Vec<(u64, usize)>>, IndexError> {
        let mut present: Vec<(&String, (u64, usize))> = Vec::with_capacity(terms.len());
        for t in terms {
            match self.block_range_for(t) {
                Some(r) => present.push((t, r)),
                None if lenient => continue,
                None => return Ok(None),
            }
        }
        // Fetch each distinct block once (a query's terms often share one).
        let mut unique: Vec<(u64, usize)> = Vec::new();
        for &(_, r) in &present {
            if !unique.contains(&r) {
                unique.push(r);
            }
        }
        let fetched = join_all(unique.iter().map(|&(off, len)| self.fetch.read(off, len))).await;
        let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(fetched.len());
        for r in fetched {
            blocks.push(r?);
        }
        let mut locs = Vec::with_capacity(present.len());
        for &(t, r) in &present {
            let idx = unique.iter().position(|u| *u == r).unwrap();
            match scan_block(&blocks[idx], t.as_bytes()) {
                Some(loc) => locs.push(loc),
                None if lenient => continue,
                None => return Ok(None),
            }
        }
        Ok(Some(locs))
    }

    /// Fetches one term's head posting and learns its tail's location.
    async fn head_block(&self, head_off: u64, head_size: usize) -> Result<HeadBlock, IndexError> {
        // `head_off`/`head_size` are parsed from the (untrusted) dictionary; guard
        // every offset sum so a crafted entry can't overflow into a panic. A bad
        // value resolves to either a malformed error or an out-of-range fetch.
        let base = self
            .postings_offset
            .checked_add(head_off)
            .ok_or(IndexError::Malformed("RRTI head posting offset overflow"))?;
        let want = head_size
            .checked_add(4)
            .ok_or(IndexError::Malformed("RRTI head posting size overflow"))?;
        let block = self.fetch.read(base, want).await?;
        if block.len() < want {
            return Err(IndexError::Malformed("RRTI short term posting block"));
        }
        let tail_size = read_u32(&block, 0) as usize;
        let head = deserialize(&block[4..want])?;
        let tail_off = base
            .checked_add(want as u64)
            .ok_or(IndexError::Malformed("RRTI tail offset overflow"))?;
        Ok(HeadBlock {
            head,
            tail_off,
            tail_size,
        })
    }

    /// Fetches one term's tail posting.
    async fn tail(&self, tail_off: u64, tail_size: usize) -> Result<RoaringBitmap, IndexError> {
        if tail_size == 0 {
            return Ok(RoaringBitmap::new());
        }
        let bytes = self.fetch.read(tail_off, tail_size).await?;
        deserialize(&bytes)
    }

    /// Returns up to `limit` doc IDs matching every query term (strict AND), most
    /// popular first (ascending doc ID == descending rank). A query term absent
    /// from the dictionary yields no results. The rank-ordered head alone usually
    /// fills `limit` in one wave; the tail is fetched only when it does not.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<u32>, IndexError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut terms = self.tokenizer.tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        match self.resolve_locs(&terms, false).await? {
            None => Ok(Vec::new()),
            Some(locs) => self.and_locs(locs, limit).await,
        }
    }

    /// Intersects the postings at `locs` (one per query term) and returns the first
    /// `limit` doc IDs ascending (== descending rank). Wave 1 fetches every head
    /// concurrently and ANDs smallest-first; the rank-ordered head usually fills
    /// `limit`, so wave 2 (the tails) runs only when it underflows and a tail exists.
    async fn and_locs(
        &self,
        locs: Vec<(u64, usize)>,
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        if locs.is_empty() {
            return Ok(Vec::new());
        }
        let heads = join_all(locs.iter().map(|&(off, size)| self.head_block(off, size))).await;
        let mut blocks: Vec<HeadBlock> = Vec::with_capacity(heads.len());
        for h in heads {
            blocks.push(h?);
        }

        blocks.sort_by_key(|b| b.head.len());
        let mut acc = blocks[0].head.clone();
        for b in &blocks[1..] {
            acc &= &b.head;
            if acc.is_empty() {
                break;
            }
        }
        let has_tail = blocks.iter().any(|b| b.tail_size > 0);
        if acc.len() as usize >= limit || !has_tail {
            return Ok(acc.iter().take(limit).collect());
        }

        let tails = join_all(blocks.iter().map(|b| self.tail(b.tail_off, b.tail_size))).await;
        let mut fulls: Vec<RoaringBitmap> = Vec::with_capacity(blocks.len());
        for (b, t) in blocks.iter().zip(tails) {
            let mut full = b.head.clone();
            full |= &t?;
            fulls.push(full);
        }
        fulls.sort_by_key(|b| b.len());
        let mut acc = fulls[0].clone();
        for b in &fulls[1..] {
            acc &= b;
            if acc.is_empty() {
                break;
            }
        }
        Ok(acc.iter().take(limit).collect())
    }

    /// Resolves every query term to its **full posting** (head ∪ tail) keyed by the
    /// term's posting-region `head_off` — the stable per-term address the `.rrb`
    /// BM25 impact sidecar is keyed by. Terms are tokenized/sorted/deduped exactly
    /// like [`search`]; `Ok(None)` when any term is absent (the strict-AND result
    /// is empty). Tails are fetched eagerly — the reranker needs full-posting
    /// ranks for candidates past the head boundary, and full df for IDF.
    pub async fn query_postings(
        &self,
        query: &str,
    ) -> Result<Option<Vec<(u64, RoaringBitmap)>>, IndexError> {
        let heads = match self.query_head_postings(query).await? {
            Some(h) => h,
            None => return Ok(None),
        };
        let tails = join_all(
            heads
                .iter()
                .map(|(_, b)| self.tail(b.tail_off, b.tail_size)),
        )
        .await;
        let mut out = Vec::with_capacity(heads.len());
        for ((off, block), tail) in heads.into_iter().zip(tails) {
            let mut full = block.head;
            full |= &tail?;
            out.push((off, full));
        }
        Ok(Some(out))
    }

    /// The head-wave half of [`query_postings`]: each query term's head posting
    /// (docs below the head boundary) plus its tail location, fetched in one
    /// concurrent wave — the same bytes [`search`]'s first wave moves. Head docs
    /// are a PREFIX of full posting order, so a head bitmap's `rank()` addresses
    /// the `.rrb` impact bytes for head candidates without touching any tail.
    pub(crate) async fn query_head_postings(
        &self,
        query: &str,
    ) -> Result<Option<Vec<(u64, HeadBlock)>>, IndexError> {
        let mut terms = self.tokenizer.tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(None);
        }
        let locs = match self.resolve_locs(&terms, false).await? {
            Some(l) => l,
            None => return Ok(None),
        };
        let heads = join_all(locs.iter().map(|&(off, size)| self.head_block(off, size))).await;
        let mut out = Vec::with_capacity(locs.len());
        for (h, &(off, _)) in heads.into_iter().zip(&locs) {
            out.push((off, h?));
        }
        Ok(Some(out))
    }

    /// The lenient sibling of [`query_head_postings`]: resolves each query term's
    /// head posting and tail location but **drops** terms absent from the dictionary
    /// instead of abandoning the whole query. The min-should-match path counts only
    /// the terms that resolve, so it needs the present subset, not all-or-nothing.
    /// Returns an empty `Vec` when no term resolves (M == 0).
    pub(crate) async fn query_head_postings_present(
        &self,
        query: &str,
    ) -> Result<Vec<(u64, HeadBlock)>, IndexError> {
        let mut terms = self.tokenizer.tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let locs = self.resolve_locs(&terms, true).await?.unwrap_or_default();
        let heads = join_all(locs.iter().map(|&(off, size)| self.head_block(off, size))).await;
        let mut out = Vec::with_capacity(locs.len());
        for (h, &(off, _)) in heads.into_iter().zip(&locs) {
            out.push((off, h?));
        }
        Ok(out)
    }

    /// Fetches one tail posting by location — the lazy second wave companion to
    /// [`query_head_postings`] (crate-internal: the BM25 path upgrades to full
    /// postings only when the head intersection can't fill the candidate window).
    pub(crate) async fn fetch_tail(
        &self,
        tail_off: u64,
        tail_size: usize,
    ) -> Result<RoaringBitmap, IndexError> {
        self.tail(tail_off, tail_size).await
    }

    /// Streams the whole dictionary: every `(term, head_off)` in sorted term order
    /// (which is also ascending `head_off` — postings are laid out in dictionary
    /// order). One fetch per dict block. O(vocabulary), so this is build-tooling
    /// surface (joining build-side term stats with the on-disk layout for the
    /// `.rrb` impact sidecar), not a query-path API.
    pub async fn dict_terms(&self) -> Result<Vec<(String, u64)>, IndexError> {
        // `term_count` is a header field; an inflated value must not pre-allocate
        // gigabytes. Cap the capacity hint — the vec still grows to the true count
        // (bounded by the actual dictionary blocks iterated below).
        let mut out = Vec::with_capacity((self.term_count as usize).min(1 << 20));
        for (off, len) in self.dict_block_locs() {
            let block = self.fetch.read(off, len).await?;
            for (term, head_off, _) in parse_dict_block(&block) {
                let term = String::from_utf8(term)
                    .map_err(|_| IndexError::Malformed("RRTI non-UTF-8 dictionary term"))?;
                out.push((term, head_off));
            }
        }
        Ok(out)
    }

    /// Every dictionary block's absolute `(offset, len)`, in term order (== file
    /// order), from the resident router — no fetches. O(#blocks) memory: the
    /// streaming counterpart to [`dict_terms`](Self::dict_terms) for vocabularies
    /// too large to hold as strings (the 484M corpus has ~187M terms) — fetch each
    /// block and walk it with [`parse_dict_block`].
    pub fn dict_block_locs(&self) -> Vec<(u64, usize)> {
        let mut out = Vec::new();
        let mut stream = self.router.stream();
        while let Some((_, packed)) = stream.next() {
            let (block_off, block_len) = unpack_loc(packed);
            out.push((self.dict_start + block_off, block_len));
        }
        out
    }

    /// The tokenizer the index was built with (from its header flags) — build
    /// tooling uses it to tokenize companion-artifact passes (e.g. the `.rrb`
    /// impact build) identically to the indexed corpus.
    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    /// Returns up to `limit` doc IDs matching **any** dictionary term that starts
    /// with `prefix` (the OR / union of every prefix-matching term's posting), most
    /// popular first (ascending doc ID == descending rank). The prefix is lowercased
    /// the same way [`tokenize`] lowercases — unless the index is case-sensitive, when
    /// it is matched verbatim. An empty match set yields no results.
    pub async fn search_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<u32>, IndexError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let p = self.fold_prefix(prefix);
        let locs = self.prefix_locs(&p).await?;
        self.union_locs(locs, limit).await
    }

    /// Autocompletes `prefix`: up to `max_terms` dictionary terms that start with it,
    /// in lexicographic order. The prefix is lowercased the same way [`tokenize`]
    /// lowercases (unless the index is case-sensitive). Range-fetches only the dict
    /// blocks spanning the prefix.
    pub async fn complete(
        &self,
        prefix: &str,
        max_terms: usize,
    ) -> Result<Vec<String>, IndexError> {
        let p = self.fold_prefix(prefix);
        Ok(self.scan_prefix(&p, max_terms, true).await?.1)
    }

    /// Normalizes a query prefix the way the index's tokenizer normalized its terms:
    /// lowercased for a case-folding index, verbatim for a case-sensitive one. The
    /// prefix path skips stemming/stop-words by design (a prefix is already fuzzy).
    fn fold_prefix(&self, prefix: &str) -> String {
        if self.tokenizer.case_fold() {
            prefix.chars().flat_map(|c| c.to_lowercase()).collect()
        } else {
            prefix.to_string()
        }
    }

    /// Posting locations of every dictionary term starting with `p` (scans the dict
    /// blocks spanning the prefix).
    async fn prefix_locs(&self, p: &str) -> Result<Vec<(u64, usize)>, IndexError> {
        Ok(self.scan_prefix(p, usize::MAX, false).await?.0)
    }

    /// Scans the dictionary forward from the first block that could contain `p`,
    /// fetching blocks on demand and gathering matches until a term sorts past the
    /// prefix or `max` are found. Returns `(locs, terms)` — `terms` is filled only
    /// when `want_terms`. Stops fetching as soon as the prefix range ends.
    async fn scan_prefix(
        &self,
        p: &str,
        max: usize,
        want_terms: bool,
    ) -> Result<(Vec<(u64, usize)>, Vec<String>), IndexError> {
        let pb = p.as_bytes();
        let mut locs = Vec::new();
        let mut terms = Vec::new();
        if max == 0 {
            return Ok((locs, terms));
        }
        // Candidate blocks: every block whose last term >= the prefix, in order.
        let mut ranges: Vec<(u64, usize)> = Vec::new();
        {
            let mut stream = self.router.range().ge(pb).into_stream();
            while let Some((_, packed)) = stream.next() {
                let (off, len) = unpack_loc(packed);
                ranges.push((self.dict_start + off, len));
            }
        }
        for (off, len) in ranges {
            let bytes = self.fetch.read(off, len).await?;
            let mut passed = false;
            for (t, head_off, head_size) in iter_block(&bytes) {
                if t.as_slice() < pb {
                    continue; // before the prefix (only possible in the first block)
                }
                if !t.starts_with(pb) {
                    passed = true; // sorted: the prefix range has ended
                    break;
                }
                locs.push((head_off, head_size));
                if want_terms {
                    terms.push(String::from_utf8_lossy(&t).into_owned());
                }
                if locs.len() >= max {
                    return Ok((locs, terms));
                }
            }
            if passed {
                break;
            }
        }
        Ok((locs, terms))
    }

    /// Unions (ORs) the postings at `locs` and returns the first `limit` doc IDs
    /// ascending (== descending rank). Wave 1 ORs the heads; wave 2 (the tails) runs
    /// only when the heads underflow `limit` and a tail exists. Shared by the prefix
    /// and (v1) fuzzy paths.
    async fn union_locs(
        &self,
        locs: Vec<(u64, usize)>,
        limit: usize,
    ) -> Result<Vec<u32>, IndexError> {
        if locs.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let heads = join_all(locs.iter().map(|&(off, size)| self.head_block(off, size))).await;
        let mut blocks: Vec<HeadBlock> = Vec::with_capacity(heads.len());
        for h in heads {
            blocks.push(h?);
        }
        let mut acc = RoaringBitmap::new();
        for b in &blocks {
            acc |= &b.head;
        }
        let has_tail = blocks.iter().any(|b| b.tail_size > 0);
        if acc.len() as usize >= limit || !has_tail {
            return Ok(acc.iter().take(limit).collect());
        }
        let tails = join_all(blocks.iter().map(|b| self.tail(b.tail_off, b.tail_size))).await;
        let mut acc = RoaringBitmap::new();
        for (b, t) in blocks.iter().zip(tails) {
            acc |= &b.head;
            acc |= &t?;
        }
        Ok(acc.iter().take(limit).collect())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::terms_build::{write_term_index, write_term_index_with, TermIndexConfig};
    use futures::executor::block_on;

    #[test]
    fn language_code_and_byte_roundtrip() {
        use Language::*;
        const ALL: [Language; 18] = [
            English, Spanish, Arabic, Danish, Dutch, Finnish, French, German, Greek, Hungarian,
            Italian, Norwegian, Portuguese, Romanian, Russian, Swedish, Tamil, Turkish,
        ];
        let mut seen_bytes = std::collections::BTreeSet::new();
        for lang in ALL {
            // Byte round-trips through the canonical `From`/`from_u8` pair.
            assert_eq!(Language::from_u8(u8::from(lang)), Some(lang));
            assert_eq!(lang.to_u8(), u8::from(lang));
            // Name and uppercased name both resolve back to the same language.
            assert_eq!(Language::from_code(lang.as_code()), Some(lang));
            assert_eq!(
                Language::from_code(&lang.as_code().to_uppercase()),
                Some(lang)
            );
            assert!(seen_bytes.insert(u8::from(lang)), "duplicate on-disk byte");
        }
        // Bytes are dense 1..=18 — every language present, none reused.
        assert_eq!(seen_bytes, (1u8..=18).collect());
        // Stable bytes — must not drift across versions.
        assert_eq!(u8::from(English), 1);
        assert_eq!(u8::from(Spanish), 2);
        // ISO-639-1 aliases and whitespace/case handling.
        assert_eq!(Language::from_code("es"), Some(Spanish));
        assert_eq!(Language::from_code("de"), Some(German));
        assert_eq!(Language::from_code(" Tr "), Some(Turkish));
        assert_eq!(Language::from_code("klingon"), None);
        assert_eq!(Language::from_u8(0), None);
        assert_eq!(Language::from_u8(99), None);
    }

    /// Builds an in-memory v2 `RRTI` over the docs at the default head boundary.
    fn build(docs: &[(u32, &str)], head_boundary: u32) -> TermIndex<MemoryFetch> {
        let mut buf = Vec::new();
        write_term_index(&mut buf, docs, head_boundary).unwrap();
        block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap()
    }

    /// Builds an in-memory v2 `RRTI` with an explicit dict block cap — a tiny cap
    /// forces many blocks, exercising the router and the cross-block scan paths.
    fn build_capped(
        docs: &[(u32, &str)],
        head_boundary: u32,
        block_cap: usize,
    ) -> TermIndex<MemoryFetch> {
        let mut buf = Vec::new();
        write_term_index_with(
            &mut buf,
            docs,
            &TermIndexConfig {
                head_boundary,
                language: None,
                stopwords: false,
                case_sensitive: false,
                block_cap,
            },
        )
        .unwrap();
        block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap()
    }

    #[test]
    fn tokenize_mirrors_simple_lowercaser() {
        assert_eq!(
            tokenize("Machine-Learning, FAST!"),
            vec!["machine", "learning", "fast"]
        );
        assert_eq!(tokenize("posthuman became"), vec!["posthuman", "became"]);
        assert_eq!(tokenize("GPT-4 and BERT"), vec!["gpt", "4", "and", "bert"]);
        assert!(tokenize("  --  ").is_empty());
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn tokenize_with_case_fold_off_keeps_case() {
        // case_fold on (the default) lowercases; off keeps the token verbatim.
        assert_eq!(
            tokenize_with("Machine LEARNING", true),
            vec!["machine", "learning"]
        );
        assert_eq!(
            tokenize_with("Machine LEARNING", false),
            vec!["Machine", "LEARNING"]
        );
        // Boundary rules (alnum runs) are identical regardless of folding.
        assert_eq!(tokenize_with("GPT-4 BERT", false), vec!["GPT", "4", "BERT"]);
    }

    #[test]
    fn case_sensitive_index_distinguishes_case() {
        // A case-sensitive index keeps "Rust" and "rust" as distinct terms; queries
        // are matched verbatim (the header's FLAG_CASE_SENSITIVE bit round-trips
        // through Tokenizer::from_header so build and query agree).
        let docs = [
            (0u32, "Rust systems programming"),
            (1, "rust never sleeps"),
            (2, "RUST belt revival"),
        ];
        let mut buf = Vec::new();
        write_term_index_with(
            &mut buf,
            &docs,
            &TermIndexConfig {
                head_boundary: 65_536,
                language: None,
                stopwords: false,
                case_sensitive: true,
                block_cap: 0,
            },
        )
        .unwrap();
        let ti = block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap();
        // The three casings are three distinct dictionary terms.
        assert_eq!(block_on(ti.search("Rust", 10)).unwrap(), vec![0]);
        assert_eq!(block_on(ti.search("rust", 10)).unwrap(), vec![1]);
        assert_eq!(block_on(ti.search("RUST", 10)).unwrap(), vec![2]);
        // Prefix matching is also verbatim: "Ru" hits only doc 0's "Rust".
        assert_eq!(block_on(ti.search_prefix("Ru", 10)).unwrap(), vec![0]);

        // The default (case-folding) index merges all three onto "rust".
        let ci = build(&docs, 65_536);
        assert_eq!(block_on(ci.search("Rust", 10)).unwrap(), vec![0, 1, 2]);
        assert_eq!(block_on(ci.search("RUST", 10)).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn whole_word_and_with_rank_order() {
        let docs = [
            (0u32, "deep learning for vision"),
            (1, "deep reinforcement learning"),
            (2, "statistical learning theory"),
            (3, "deep sea creatures"),
        ];
        let ti = build(&docs, 65_536);
        // distinct terms: deep, learning, for, vision, reinforcement, statistical,
        // theory, sea, creatures.
        assert_eq!(ti.len(), 9);

        // "deep learning" -> docs with BOTH (0, 1), ascending.
        assert_eq!(
            block_on(ti.search("deep learning", 10)).unwrap(),
            vec![0, 1]
        );
        // "learning" alone -> 0, 1, 2.
        assert_eq!(block_on(ti.search("learning", 10)).unwrap(), vec![0, 1, 2]);
        // "deep" -> 0, 1, 3; top-1 by rank is the lowest doc ID.
        assert_eq!(block_on(ti.search("deep", 1)).unwrap(), vec![0]);
        // case/punctuation-insensitive, same as the indexed tokens.
        assert_eq!(
            block_on(ti.search("DEEP, Learning", 10)).unwrap(),
            vec![0, 1]
        );
        // a term not in the dictionary -> no results.
        assert!(block_on(ti.search("quantum", 10)).unwrap().is_empty());
        // empty / punctuation-only query -> no results.
        assert!(block_on(ti.search("  ---  ", 10)).unwrap().is_empty());
    }

    #[test]
    fn head_tail_split_and_lazy_tail() {
        // A tiny head boundary forces a head/tail split: head = docs [0, 2).
        let docs = [
            (0u32, "alpha"),
            (1, "alpha"),
            (2, "alpha"),
            (3, "alpha"),
            (4, "alpha beta"),
            (5, "beta"),
        ];
        let ti = build(&docs, 2);

        // "alpha" spans head {0,1} and tail {2,3,4}; limit 10 pulls the tail.
        assert_eq!(
            block_on(ti.search("alpha", 10)).unwrap(),
            vec![0, 1, 2, 3, 4]
        );
        // limit 2 is satisfied by the rank head alone — no tail fetch needed.
        assert_eq!(block_on(ti.search("alpha", 2)).unwrap(), vec![0, 1]);
        // AND across head and tail: alpha {0..4} & beta {4,5} = {4} (a tail doc).
        assert_eq!(block_on(ti.search("alpha beta", 10)).unwrap(), vec![4]);
    }

    #[test]
    fn empty_corpus_and_bad_magic() {
        let ti = build(&[], 65_536);
        assert!(ti.is_empty());
        assert!(block_on(ti.search("anything", 10)).unwrap().is_empty());

        // A buffer that is not an RRTI file is rejected.
        let bogus = MemoryFetch::new(vec![0u8; HEADER_SIZE]);
        assert!(matches!(
            block_on(TermIndex::open(bogus)),
            Err(IndexError::BadMagic(_))
        ));
    }

    #[test]
    fn complete_returns_capped_prefix_terms() {
        let docs = [
            (0u32, "learn learner learning learned"),
            (1, "learns lethal"),
        ];
        let ti = build(&docs, 65_536);

        // "learn" prefixes five terms, returned in lexicographic order.
        assert_eq!(
            block_on(ti.complete("learn", 10)).unwrap(),
            vec!["learn", "learned", "learner", "learning", "learns"]
        );
        // `max_terms` caps the result; the first two in order.
        assert_eq!(
            block_on(ti.complete("learn", 2)).unwrap(),
            vec!["learn", "learned"]
        );
        // The prefix is lowercased the same way the tokens are.
        assert_eq!(
            block_on(ti.complete("LEARN", 10)).unwrap(),
            vec!["learn", "learned", "learner", "learning", "learns"]
        );
        // "le" widens to include "lethal".
        assert_eq!(
            block_on(ti.complete("le", 10)).unwrap(),
            vec!["learn", "learned", "learner", "learning", "learns", "lethal"]
        );
        // A prefix that matches nothing yields nothing.
        assert!(block_on(ti.complete("zzz", 10)).unwrap().is_empty());
        // An empty prefix matches everything (capped).
        assert_eq!(block_on(ti.complete("", 1)).unwrap(), vec!["learn"]);
    }

    #[test]
    fn blocked_dict_many_blocks_round_trips() {
        // A tiny block cap forces many front-coded blocks, so exact lookup, the
        // dict-block wave, and the cross-block prefix scan all span block boundaries.
        let docs: Vec<(u32, String)> = (0..200u32)
            .map(|i| (i, format!("term{i:04} shared{}", i % 5)))
            .collect();
        let refs: Vec<(u32, &str)> = docs.iter().map(|(d, t)| (*d, t.as_str())).collect();
        let ti = build_capped(&refs, 65_536, 24);

        // Exact lookups resolve across many blocks.
        assert_eq!(block_on(ti.search("term0000", 10)).unwrap(), vec![0]);
        assert_eq!(block_on(ti.search("term0137", 10)).unwrap(), vec![137]);
        assert_eq!(block_on(ti.search("term0199", 10)).unwrap(), vec![199]);
        // Absent term -> no results (fetches its candidate block, scans, misses).
        assert!(block_on(ti.search("term9999", 10)).unwrap().is_empty());
        // A two-term AND whose terms live in different blocks.
        assert_eq!(
            block_on(ti.search("term0000 shared0", 10)).unwrap(),
            vec![0]
        );
        // "shared0" appears on docs 0,5,10,...; prefix scan crosses blocks and unions.
        assert_eq!(
            block_on(ti.search_prefix("shared0", 5)).unwrap(),
            vec![0, 5, 10, 15, 20]
        );
        // Completion crosses block boundaries and is capped.
        assert_eq!(
            block_on(ti.complete("term001", 3)).unwrap(),
            vec!["term0010", "term0011", "term0012"]
        );
    }

    #[test]
    fn search_prefix_unions_matching_terms() {
        let docs = [
            (0u32, "learn"),
            (1, "learning"),
            (2, "lethal"),
            (3, "unrelated"),
        ];
        let ti = build(&docs, 65_536);

        // "learn" matches both "learn" (doc 0) and "learning" (doc 1) — their OR.
        assert_eq!(block_on(ti.search_prefix("learn", 10)).unwrap(), vec![0, 1]);
        // "le" widens to also include "lethal" (doc 2); ascending == rank order.
        assert_eq!(block_on(ti.search_prefix("le", 10)).unwrap(), vec![0, 1, 2]);
        // `limit` keeps the most-popular (lowest doc-ID) prefix hits.
        assert_eq!(block_on(ti.search_prefix("le", 2)).unwrap(), vec![0, 1]);
        // A prefix matching no term yields no results.
        assert!(block_on(ti.search_prefix("zzz", 10)).unwrap().is_empty());
    }

    #[test]
    fn search_prefix_spans_head_and_tail() {
        // A tiny head boundary forces a split; "alpha" spans head {0,1} and tail
        // {2,3,4}, "alpine" adds doc 5, so the prefix "alp" unions all of them.
        let docs = [
            (0u32, "alpha"),
            (1, "alpha"),
            (2, "alpha"),
            (3, "alpha"),
            (4, "alpha"),
            (5, "alpine"),
        ];
        let ti = build(&docs, 2);
        assert_eq!(
            block_on(ti.search_prefix("alp", 10)).unwrap(),
            vec![0, 1, 2, 3, 4, 5]
        );
        // limit 2 is satisfied by the rank heads alone — no tail fetch needed.
        assert_eq!(block_on(ti.search_prefix("alp", 2)).unwrap(), vec![0, 1]);
    }

    #[test]
    fn resident_router_is_small_vs_dictionary() {
        // The scaling property: with the default block cap, what stays resident
        // (header + router FST) is a small fraction of the full dictionary bytes —
        // O(#blocks), not O(vocabulary). The old v1 FST held the whole dictionary.
        let docs: Vec<(u32, String)> = (0..8_000u32)
            .map(|i| (i, format!("scholarlyterm{i:05}")))
            .collect();
        let refs: Vec<(u32, &str)> = docs.iter().map(|(d, t)| (*d, t.as_str())).collect();
        let mut buf = Vec::new();
        write_term_index(&mut buf, &refs, 65_536).unwrap();
        let dict_len = read_u64(&buf, 24); // header field: dict-blocks region length
        let ti = block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap();
        assert_eq!(ti.len(), 8_000);
        // The resident boot (header + router) is well under a third of the dict it
        // routes — and shrinks relative to the dict as the vocabulary grows.
        assert!(
            ti.resident_len() * 3 < dict_len,
            "resident {} should be << dict {dict_len}",
            ti.resident_len()
        );
        // And it still resolves a term across the many blocks.
        assert_eq!(
            block_on(ti.search("scholarlyterm04242", 10)).unwrap(),
            vec![4242]
        );
    }

    #[test]
    fn stemming_makes_inflections_match() {
        // An English-stemmed index reduces learning/learned/learns to the stem "learn",
        // so a query for any inflection matches docs indexed with any other. The reader
        // stems the query identically (from the header flags) — the symmetry invariant.
        let docs = [
            (0u32, "deep learning"),
            (1, "she learned quickly"),
            (2, "it learns fast"),
            (3, "ocean depths"),
        ];
        let mut buf = Vec::new();
        write_term_index_with(
            &mut buf,
            &docs,
            &TermIndexConfig {
                head_boundary: 65_536,
                language: Some(Language::English),
                stopwords: false,
                case_sensitive: false,
                block_cap: 0,
            },
        )
        .unwrap();
        let ti = block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap();
        // "learning" -> stem "learn" -> docs 0, 1, 2 (learning / learned / learns).
        assert_eq!(block_on(ti.search("learning", 10)).unwrap(), vec![0, 1, 2]);
        // a different inflection of the same stem returns the same docs (and lowercases).
        assert_eq!(block_on(ti.search("LEARNED", 10)).unwrap(), vec![0, 1, 2]);
        // an unrelated word does not.
        assert_eq!(block_on(ti.search("ocean", 10)).unwrap(), vec![3]);
    }

    #[test]
    fn stopwords_dropped_from_index_and_query() {
        let docs = [(0u32, "the cat"), (1, "a dog and a cat")];
        let mut buf = Vec::new();
        write_term_index_with(
            &mut buf,
            &docs,
            &TermIndexConfig {
                head_boundary: 65_536,
                language: None,
                stopwords: true,
                case_sensitive: false,
                block_cap: 0,
            },
        )
        .unwrap();
        let ti = block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap();
        // Stop words ("the", "a", "and") were dropped from the index; "cat" remains in both.
        assert_eq!(block_on(ti.search("cat", 10)).unwrap(), vec![0, 1]);
        // A query of only stop words drops to nothing.
        assert!(block_on(ti.search("the and a", 10)).unwrap().is_empty());
        // Stop words in a query are dropped too, leaving "cat" -> docs 0, 1.
        assert_eq!(block_on(ti.search("the cat", 10)).unwrap(), vec![0, 1]);
    }
}
