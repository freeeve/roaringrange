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
use fst::Map;
use futures::future::join_all;
use roaring::RoaringBitmap;

/// `RRTI` magic.
const MAGIC: &[u8; 4] = b"RRTI";
/// Header size in bytes: magic[4] + version[2] + flags[2] + termCount[4] +
/// headBoundary[4] + fstLen[8] + reserved[8]. Kept in sync with the builder.
const HEADER_SIZE: usize = 32;
/// Format version written into / accepted from the header.
const VERSION: u16 = 1;
/// Bits of the FST output `u64` used for the head posting's byte length; the
/// remaining high bits hold the block's byte offset in the postings region.
const SIZE_BITS: u32 = 24;
/// Low-bit mask selecting the head size out of an FST output.
const SIZE_MASK: u64 = (1 << SIZE_BITS) - 1;

/// Splits `text` into lowercased terms, mirroring Tantivy's `SimpleTokenizer`
/// (a token is a maximal run of `char::is_alphanumeric`) followed by its
/// `LowerCaser` (`char::to_lowercase`). The builder and the reader call this same
/// function, so a query tokenizes identically to the indexed text — the one
/// correctness invariant of a term index. Stop-word and stemming filters slot in
/// after this base step in a later phase.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// A term's head posting plus the location of its (lazily fetched) tail.
struct HeadBlock {
    head: RoaringBitmap,
    tail_off: u64,
    tail_size: usize,
}

/// A range-fetchable `RRTI` term index. Boot holds the FST term dictionary in
/// memory; each query range-fetches only the matched terms' postings.
pub struct TermIndex<F: RangeFetch> {
    fetch: F,
    fst: Map<Vec<u8>>,
    /// Byte offset of the postings region (right after the FST blob).
    postings_offset: u64,
    /// The head/tail doc-ID split baked into the postings (metadata).
    head_boundary: u32,
    /// Number of distinct terms in the dictionary.
    term_count: u32,
}

impl<F: RangeFetch> TermIndex<F> {
    /// Boots the index: reads the fixed header, then the whole FST dictionary blob
    /// in one ranged read, parsing it into memory. Subsequent queries fetch only
    /// per-term posting blocks.
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
        let term_count = read_u32(&header, 8);
        let head_boundary = read_u32(&header, 12);
        let fst_len = read_u64(&header, 16);
        let fst_bytes = fetch.read(HEADER_SIZE as u64, fst_len as usize).await?;
        let fst =
            Map::new(fst_bytes).map_err(|_| IndexError::Malformed("invalid FST dictionary"))?;
        Ok(Self {
            fetch,
            fst,
            postings_offset: HEADER_SIZE as u64 + fst_len,
            head_boundary,
            term_count,
        })
    }

    /// Number of distinct terms in the dictionary.
    pub fn len(&self) -> usize {
        self.term_count as usize
    }

    /// Reports whether the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.term_count == 0
    }

    /// The doc-ID head/tail boundary baked into the postings.
    pub fn head_boundary(&self) -> u32 {
        self.head_boundary
    }

    /// Resolves a term to its posting block `(head_off, head_size)` via the FST,
    /// or `None` if the term is absent. No fetch — the FST is resident.
    fn locate(&self, term: &str) -> Option<(u64, usize)> {
        self.fst
            .get(term.as_bytes())
            .map(|out| (out >> SIZE_BITS, (out & SIZE_MASK) as usize))
    }

    /// Fetches one term's head posting and learns its tail's location.
    async fn head_block(&self, head_off: u64, head_size: usize) -> Result<HeadBlock, IndexError> {
        let base = self.postings_offset + head_off;
        let block = self.fetch.read(base, 4 + head_size).await?;
        if block.len() < 4 + head_size {
            return Err(IndexError::Malformed("short term posting block"));
        }
        let tail_size = read_u32(&block, 0) as usize;
        let head = deserialize(&block[4..4 + head_size])?;
        Ok(HeadBlock {
            head,
            tail_off: base + 4 + head_size as u64,
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
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let mut locs = Vec::with_capacity(terms.len());
        for t in &terms {
            match self.locate(t) {
                Some(loc) => locs.push(loc),
                None => return Ok(Vec::new()),
            }
        }

        // Wave 1: fetch every term's head posting concurrently.
        let heads = join_all(locs.iter().map(|&(off, size)| self.head_block(off, size))).await;
        let mut blocks: Vec<HeadBlock> = Vec::with_capacity(heads.len());
        for h in heads {
            blocks.push(h?);
        }

        // AND the heads smallest-first; the rank-ordered head often fills top-K.
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

        // Wave 2: the head AND underflowed and tails exist — fetch them and AND
        // the full `(head | tail)` postings.
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
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::terms_build::write_term_index;
    use futures::executor::block_on;

    /// Builds an in-memory `RRTI` over the docs at the default head boundary.
    fn build(docs: &[(u32, &str)], head_boundary: u32) -> TermIndex<MemoryFetch> {
        let mut buf = Vec::new();
        write_term_index(&mut buf, docs, head_boundary).unwrap();
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
}
