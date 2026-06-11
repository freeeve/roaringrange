//! The `RRSB` BM25 impact sidecar (`.rrb`) — additive lexical scoring for the
//! `RRTI` term index. The existing `.rrt` is not modified in any way: the sidecar
//! is keyed by each term's posting-region `head_off` (unique and ascending in
//! dictionary order), so a reader that has resolved a query term against the
//! `.rrt` can address its impacts with no extra dictionary structure, and a
//! reader that never opts in never fetches a byte of it.
//!
//! Per (term, doc) the sidecar stores ONE quantized impact byte: the BM25
//! term-frequency component with the document-length norm **folded in at build
//! time** —
//!
//! ```text
//! s = tf·(k1+1) / (tf + k1·(1 − b + b·dl/avgdl))      bounded by k1+1
//! byte = clamp(round(s · 255 / scale), 1, 255)         scale = k1+1
//! ```
//!
//! so scoring at query time is `Σ idf(term) · byte·scale/255` with
//! `idf = ln(1 + (N − df + ½)/(df + ½))` — df is the posting cardinality the
//! reader already knows, N is in the header. No separate norms file.
//!
//! Layout (all little-endian):
//!
//! ```text
//! [0,4)    magic "RRSB"
//! [4,6)    version u16 = 1
//! [6,8)    flags u16 (reserved)
//! [8,12)   scale f32 (impact dequant scale, = k1+1 at build)
//! [12,16)  k1 f32 (informational)
//! [16,20)  b f32 (informational)
//! [20,24)  avgdl f32 (informational)
//! [24,28)  term_count u32 (must equal the .rrt's)
//! [28,32)  sparse_stride u32
//! [32,40)  entries_off u64 (absolute)
//! [40,48)  impacts_off u64 (absolute)
//! [48,56)  doc_count u64 (N for IDF)
//! [56,64)  reserved
//! [64,…)   sparse index: ceil(term_count/stride) × u64 — every stride-th
//!          entry's head_off, resident after open (the RRS sparse-dict shape)
//! entries: term_count × 20 B (head_off u64, impacts_rel u64, card u32),
//!          ascending head_off; one ranged read covers a query term's stride
//! impacts: per term, `card` bytes in ascending posting doc order — a candidate
//!          doc's byte sits at impacts_rel + (posting.rank(doc) − 1), so the
//!          posting bitmap the term search already fetched IS the addressing
//!          structure (head docs all precede tail docs, so a head-only bitmap
//!          ranks its own docs identically)
//! ```

use crate::fetch::{read_coalesced, RangeFetch, COALESCE_GAP};
use crate::index::{read_u16, read_u32, read_u64, IndexError};
use crate::terms::TermIndex;
use roaring::RoaringBitmap;

/// Default BM25 term-frequency saturation parameter.
pub const DEFAULT_K1: f32 = 1.2;
/// Default BM25 length-normalization strength.
pub const DEFAULT_B: f32 = 0.75;

const MAGIC: &[u8; 4] = b"RRSB";
const VERSION: u16 = 1;
const HEADER_SIZE: usize = 64;
const ENTRY_SIZE: usize = 20;
/// Entries covered per resident sparse key — one ~10 KB ranged read per query
/// term. Written by the (native-only) builder; the reader honors the header's.
#[cfg(not(target_arch = "wasm32"))]
const SPARSE_STRIDE: u32 = 512;

fn read_f32(buf: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// One scored document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredDoc {
    /// The shared rank-order doc ID.
    pub doc_id: u32,
    /// BM25 score (sum over query terms of idf × dequantized impact).
    pub score: f32,
}

/// A range-fetchable `RRSB` impact sidecar. Resident state is the header plus the
/// sparse entry index — O(vocabulary / stride), ~8 bytes per 512 terms.
pub struct ImpactIndex<F: RangeFetch> {
    fetch: F,
    scale: f32,
    doc_count: u64,
    term_count: u32,
    stride: u32,
    entries_off: u64,
    impacts_off: u64,
    /// Every stride-th entry's `head_off`.
    sparse: Vec<u64>,
}

impl<F: RangeFetch> ImpactIndex<F> {
    /// Boots the sidecar: one header read plus the resident sparse index.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        if header.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRSB header"));
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
        let scale = read_f32(&header, 8);
        let term_count = read_u32(&header, 24);
        let stride = read_u32(&header, 28);
        let entries_off = read_u64(&header, 32);
        let impacts_off = read_u64(&header, 40);
        let doc_count = read_u64(&header, 48);
        if stride == 0 || scale <= 0.0 {
            return Err(IndexError::Malformed("bad RRSB stride/scale"));
        }
        let sparse_count = (term_count as usize).div_ceil(stride as usize);
        let sparse_bytes = fetch.read(HEADER_SIZE as u64, sparse_count * 8).await?;
        if sparse_bytes.len() < sparse_count * 8 {
            return Err(IndexError::Malformed("short RRSB sparse index"));
        }
        let sparse = (0..sparse_count)
            .map(|i| read_u64(&sparse_bytes, i * 8))
            .collect();
        Ok(Self {
            fetch,
            scale,
            doc_count,
            term_count,
            stride,
            entries_off,
            impacts_off,
            sparse,
        })
    }

    /// Total documents in the corpus the sidecar was built over (BM25's N).
    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// The byte range of the entry stripe whose sparse slot could contain
    /// `head_off`, and the index of its first entry.
    fn entry_stripe(&self, head_off: u64) -> (u64, usize, usize) {
        let slot = match self.sparse.binary_search(&head_off) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        let first = slot * self.stride as usize;
        let count = (self.term_count as usize - first).min(self.stride as usize);
        (
            self.entries_off + (first * ENTRY_SIZE) as u64,
            count * ENTRY_SIZE,
            first,
        )
    }

    /// Looks up `(impacts_rel, card)` for each term `head_off`, one coalesced
    /// wave of entry-stripe reads.
    async fn entries(&self, head_offs: &[u64]) -> Result<Vec<(u64, u32)>, IndexError> {
        let stripes: Vec<(u64, usize)> = head_offs
            .iter()
            .map(|&h| {
                let (off, len, _) = self.entry_stripe(h);
                (off, len)
            })
            .collect();
        let blocks = read_coalesced(&self.fetch, &stripes, COALESCE_GAP).await?;
        head_offs
            .iter()
            .zip(&blocks)
            .map(|(&h, block)| {
                let n = block.len() / ENTRY_SIZE;
                // Entries are ascending by head_off within the stripe.
                let mut lo = 0usize;
                let mut hi = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let key = read_u64(block, mid * ENTRY_SIZE);
                    match key.cmp(&h) {
                        std::cmp::Ordering::Less => lo = mid + 1,
                        std::cmp::Ordering::Greater => hi = mid,
                        std::cmp::Ordering::Equal => {
                            return Ok((
                                read_u64(block, mid * ENTRY_SIZE + 8),
                                read_u32(block, mid * ENTRY_SIZE + 16),
                            ))
                        }
                    }
                }
                Err(IndexError::Malformed("term head_off missing from RRSB"))
            })
            .collect()
    }

    /// Scores `candidates` (ascending doc IDs) against the query's `postings` —
    /// `(head_off, posting bitmap)` pairs from [`TermIndex::query_postings`] —
    /// returning the top `k` by BM25 score (ties broken by ascending doc ID, i.e.
    /// descending static rank). Each posting bitmap must contain every candidate
    /// it covers with its on-disk doc membership (full, or head-only when every
    /// candidate is below the head boundary): the bitmap's rank is the impact
    /// byte's address. Fetches one coalesced wave of single-byte impact reads.
    pub async fn rerank(
        &self,
        postings: &[(u64, RoaringBitmap)],
        candidates: &[u32],
        k: usize,
    ) -> Result<Vec<ScoredDoc>, IndexError> {
        if candidates.is_empty() || postings.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let head_offs: Vec<u64> = postings.iter().map(|p| p.0).collect();
        let entries = self.entries(&head_offs).await?;

        // One flat range list across every (term, candidate) pair, then one
        // coalesced wave. Candidate positions within a term ascend with doc ID,
        // so intersection-clustered candidates coalesce into few real reads.
        let mut ranges: Vec<(u64, usize)> = Vec::new();
        let mut owner: Vec<(usize, usize)> = Vec::new(); // (term idx, candidate idx)
        for (ti, ((_, bm), &(rel, _card))) in postings.iter().zip(&entries).enumerate() {
            for (ci, &doc) in candidates.iter().enumerate() {
                if bm.contains(doc) {
                    let pos = self.impacts_off + rel + (bm.rank(doc) - 1);
                    ranges.push((pos, 1));
                    owner.push((ti, ci));
                }
            }
        }
        let bytes = read_coalesced(&self.fetch, &ranges, COALESCE_GAP).await?;

        let idf: Vec<f32> = entries
            .iter()
            .map(|&(_, card)| {
                let df = card as f64;
                let n = self.doc_count as f64;
                (1.0 + (n - df + 0.5) / (df + 0.5)).ln() as f32
            })
            .collect();
        let mut scores = vec![0.0f32; candidates.len()];
        for ((ti, ci), b) in owner.into_iter().zip(&bytes) {
            let byte = *b
                .first()
                .ok_or(IndexError::Malformed("short RRSB impact read"))?;
            scores[ci] += idf[ti] * (byte as f32) * self.scale / 255.0;
        }

        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .partial_cmp(&scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(candidates[a].cmp(&candidates[b]))
        });
        Ok(order
            .into_iter()
            .take(k)
            .map(|i| ScoredDoc {
                doc_id: candidates[i],
                score: scores[i],
            })
            .collect())
    }
}

/// Strict-AND term search reranked by BM25: intersect the query terms' postings,
/// take the first `m` candidates in static-rank order (the candidate window), and
/// return the top `k` by BM25 score. `m` bounds the rerank cost; a relevant doc
/// outside the static-rank top-`m` is invisible to the reranker (hybrid's vector
/// arm is the usual recovery). The two indexes may live behind different fetchers
/// (two files of one composition).
pub async fn search_bm25<F: RangeFetch, G: RangeFetch>(
    terms: &TermIndex<F>,
    impacts: &ImpactIndex<G>,
    query: &str,
    m: usize,
    k: usize,
) -> Result<Vec<ScoredDoc>, IndexError> {
    let postings = match terms.query_postings(query).await? {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    let mut sorted: Vec<&RoaringBitmap> = postings.iter().map(|(_, b)| b).collect();
    sorted.sort_by_key(|b| b.len());
    let mut acc = sorted[0].clone();
    for b in &sorted[1..] {
        acc &= *b;
        if acc.is_empty() {
            return Ok(Vec::new());
        }
    }
    let candidates: Vec<u32> = acc.iter().take(m).collect();
    impacts.rerank(&postings, &candidates, k).await
}

/// Native build side: accumulates per-(term, doc) frequencies and document
/// lengths with the SAME tokenizer the `.rrt` build used, then joins against the
/// finished index's dictionary ([`TermIndex::dict_terms`]) so the sidecar's
/// head_off keys are byte-true to the layout actually on disk.
#[cfg(not(target_arch = "wasm32"))]
pub use native::{write_impacts, ImpactsAccumulator};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{ENTRY_SIZE, HEADER_SIZE, MAGIC, SPARSE_STRIDE, VERSION};
    use crate::terms::Tokenizer;
    use std::collections::BTreeMap;
    use std::io::{self, Write};

    /// Accumulates `(doc, tf)` per term plus per-doc lengths. Docs must be added
    /// in ascending doc-ID order (the shared rank order) — the per-term lists are
    /// then ascending by construction, matching posting iteration order.
    pub struct ImpactsAccumulator {
        tokenizer: Tokenizer,
        terms: BTreeMap<String, Vec<(u32, u32)>>,
        doc_lens: Vec<u64>,
        next_doc: u32,
    }

    impl ImpactsAccumulator {
        /// `tokenizer` must match the `.rrt` build's (same language/stopword
        /// config), or the vocabularies diverge and [`write_impacts`] errors.
        pub fn new(tokenizer: Tokenizer) -> Self {
            ImpactsAccumulator {
                tokenizer,
                terms: BTreeMap::new(),
                doc_lens: Vec::new(),
                next_doc: 0,
            }
        }

        /// Tokenizes `text` as the next sequential doc ID and returns that ID.
        pub fn add_doc(&mut self, text: &str) -> u32 {
            let doc = self.next_doc;
            self.next_doc += 1;
            let toks = self.tokenizer.tokenize(text);
            self.doc_lens.push(toks.len() as u64);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in toks {
                *tf.entry(t).or_default() += 1;
            }
            for (t, n) in tf {
                self.terms.entry(t).or_default().push((doc, n));
            }
            doc
        }

        /// Documents accumulated so far.
        pub fn doc_count(&self) -> u64 {
            self.doc_lens.len() as u64
        }
    }

    /// Writes the `RRSB` sidecar for a finished `.rrt` whose dictionary is `dict`
    /// (`(term, head_off)` in dictionary order, from [`crate::TermIndex::dict_terms`])
    /// over the stats in `acc`. Every dictionary term must have accumulated stats —
    /// a mismatch means the tokenizer configs diverged, and the sidecar would
    /// mis-address every later term, so it fails loudly instead.
    pub fn write_impacts<W: Write>(
        mut w: W,
        dict: &[(String, u64)],
        acc: &ImpactsAccumulator,
        k1: f32,
        b: f32,
    ) -> io::Result<()> {
        let n_docs = acc.doc_lens.len() as u64;
        if n_docs == 0 {
            return Err(io::Error::other("RRSB build over zero documents"));
        }
        let avgdl = acc.doc_lens.iter().sum::<u64>() as f32 / n_docs as f32;
        let scale = k1 + 1.0;

        let mut entries: Vec<(u64, u64, u32)> = Vec::with_capacity(dict.len());
        let mut impacts: Vec<u8> = Vec::new();
        let mut prev_head_off: Option<u64> = None;
        for (term, head_off) in dict {
            if let Some(p) = prev_head_off {
                if *head_off <= p {
                    return Err(io::Error::other("RRSB dict head_offs not ascending"));
                }
            }
            prev_head_off = Some(*head_off);
            let tfs = acc.terms.get(term).ok_or_else(|| {
                io::Error::other(format!(
                    "dictionary term {term:?} has no accumulated stats — tokenizer mismatch?"
                ))
            })?;
            let rel = impacts.len() as u64;
            for &(doc, tf) in tfs {
                let dl = acc.doc_lens[doc as usize] as f32;
                let tf = tf as f32;
                let s = tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avgdl));
                let q = ((s * 255.0 / scale).round() as i64).clamp(1, 255) as u8;
                impacts.push(q);
            }
            entries.push((*head_off, rel, tfs.len() as u32));
        }

        let term_count = entries.len() as u32;
        let sparse_count = (term_count as usize).div_ceil(SPARSE_STRIDE as usize);
        let entries_off = (HEADER_SIZE + sparse_count * 8) as u64;
        let impacts_off = entries_off + (entries.len() * ENTRY_SIZE) as u64;

        let mut header = Vec::with_capacity(HEADER_SIZE);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&scale.to_le_bytes());
        header.extend_from_slice(&k1.to_le_bytes());
        header.extend_from_slice(&b.to_le_bytes());
        header.extend_from_slice(&avgdl.to_le_bytes());
        header.extend_from_slice(&term_count.to_le_bytes());
        header.extend_from_slice(&SPARSE_STRIDE.to_le_bytes());
        header.extend_from_slice(&entries_off.to_le_bytes());
        header.extend_from_slice(&impacts_off.to_le_bytes());
        header.extend_from_slice(&n_docs.to_le_bytes());
        header.extend_from_slice(&[0u8; 8]);
        debug_assert_eq!(header.len(), HEADER_SIZE);
        w.write_all(&header)?;
        for i in 0..sparse_count {
            w.write_all(&entries[i * SPARSE_STRIDE as usize].0.to_le_bytes())?;
        }
        for &(head_off, rel, card) in &entries {
            w.write_all(&head_off.to_le_bytes())?;
            w.write_all(&rel.to_le_bytes())?;
            w.write_all(&card.to_le_bytes())?;
        }
        w.write_all(&impacts)?;
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::terms::Tokenizer;
    use crate::terms_build::{TermIndexBuilder, TermIndexConfig};
    use crate::MemoryFetch;
    use futures::executor::block_on;

    const CFG: TermIndexConfig = TermIndexConfig {
        head_boundary: 65536,
        language: None,
        stopwords: false,
        block_cap: 0,
    };

    /// Builds a corpus's `.rrt` + `.rrb` in memory and returns both readers.
    fn build(
        docs: &[&str],
    ) -> (
        TermIndex<MemoryFetch>,
        ImpactIndex<MemoryFetch>,
        Vec<&'static str>,
    ) {
        let mut tb = TermIndexBuilder::new(&CFG);
        let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
        for (i, d) in docs.iter().enumerate() {
            tb.add(i as u32, d);
            acc.add_doc(d);
        }
        let mut rrt = Vec::new();
        tb.finish(&mut rrt).unwrap();
        let terms = block_on(TermIndex::open(MemoryFetch::new(rrt))).unwrap();
        let dict = block_on(terms.dict_terms()).unwrap();
        let mut rrb = Vec::new();
        write_impacts(&mut rrb, &dict, &acc, DEFAULT_K1, DEFAULT_B).unwrap();
        let impacts = block_on(ImpactIndex::open(MemoryFetch::new(rrb))).unwrap();
        (terms, impacts, Vec::new())
    }

    /// Brute-force BM25 over the same quantization, for comparison.
    fn brute(docs: &[&str], query: &str, k: usize) -> Vec<u32> {
        let tok = Tokenizer::plain();
        let n = docs.len() as f64;
        let lens: Vec<usize> = docs.iter().map(|d| tok.tokenize(d).len()).collect();
        let avgdl = lens.iter().sum::<usize>() as f32 / docs.len() as f32;
        let mut qt = tok.tokenize(query);
        qt.sort();
        qt.dedup();
        let mut scored: Vec<(u32, f32)> = Vec::new();
        'docs: for (i, d) in docs.iter().enumerate() {
            let toks = tok.tokenize(d);
            let mut score = 0.0f32;
            for t in &qt {
                let tf = toks.iter().filter(|x| *x == t).count() as f32;
                if tf == 0.0 {
                    continue 'docs; // strict AND
                }
                let df = docs.iter().filter(|d| tok.tokenize(d).contains(t)).count() as f64;
                let dl = lens[i] as f32;
                let s = tf * (DEFAULT_K1 + 1.0)
                    / (tf + DEFAULT_K1 * (1.0 - DEFAULT_B + DEFAULT_B * dl / avgdl));
                let q = ((s * 255.0 / (DEFAULT_K1 + 1.0)).round() as i64).clamp(1, 255) as f32;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln() as f32;
                score += idf * q * (DEFAULT_K1 + 1.0) / 255.0;
            }
            scored.push((i as u32, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        scored.into_iter().take(k).map(|(d, _)| d).collect()
    }

    #[test]
    fn rerank_matches_brute_force_bm25() {
        let docs: Vec<String> = (0..200)
            .map(|i| {
                // Vary tf and dl: doc i repeats "alpha" (i % 7) times, "beta"
                // (i % 3) times, padded with i % 11 filler words.
                let mut s = String::new();
                for _ in 0..(i % 7) {
                    s.push_str("alpha ");
                }
                for _ in 0..(i % 3) {
                    s.push_str("beta ");
                }
                for j in 0..(i % 11) {
                    s.push_str(&format!("filler{j} "));
                }
                s.push_str("common");
                s
            })
            .collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let (terms, impacts, _) = build(&refs);
        let got = block_on(search_bm25(&terms, &impacts, "alpha beta", refs.len(), 10)).unwrap();
        let got_ids: Vec<u32> = got.iter().map(|s| s.doc_id).collect();
        assert_eq!(got_ids, brute(&refs, "alpha beta", 10));
        // Scores are descending and positive.
        for w in got.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
        assert!(got.last().unwrap().score > 0.0);
    }

    #[test]
    fn rerank_promotes_high_tf_over_static_rank() {
        // Doc 0 outranks statically; doc 3 is far more lexically relevant.
        let docs = [
            "rust mentioned once among many other words entirely",
            "nothing relevant here at all",
            "still nothing relevant",
            "rust rust rust rust rust rust",
        ];
        let (terms, impacts, _) = build(&docs);
        let plain = block_on(terms.search("rust", 10)).unwrap();
        assert_eq!(plain, vec![0, 3]); // static rank order
        let scored = block_on(search_bm25(&terms, &impacts, "rust", 10, 10)).unwrap();
        assert_eq!(scored[0].doc_id, 3);
        assert_eq!(scored[1].doc_id, 0);
    }

    #[test]
    fn candidate_window_bounds_the_rerank() {
        let docs = [
            "zebra zebra zebra",
            "zebra once only here with padding words",
            "zebra zebra zebra zebra zebra",
        ];
        let (terms, impacts, _) = build(&docs);
        // m=2: doc 2 (most relevant) is outside the candidate window.
        let scored = block_on(search_bm25(&terms, &impacts, "zebra", 2, 10)).unwrap();
        let ids: Vec<u32> = scored.iter().map(|s| s.doc_id).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn absent_term_yields_empty() {
        let docs = ["alpha beta", "beta gamma"];
        let (terms, impacts, _) = build(&docs);
        assert!(block_on(search_bm25(&terms, &impacts, "alpha zzz", 10, 10))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn open_rejects_bad_magic_and_version() {
        let err = block_on(ImpactIndex::open(MemoryFetch::new(vec![0u8; HEADER_SIZE])));
        assert!(matches!(err, Err(IndexError::BadMagic(_))));
        let docs = ["alpha"];
        let mut tb = TermIndexBuilder::new(&CFG);
        tb.add(0, docs[0]);
        let mut rrt = Vec::new();
        tb.finish(&mut rrt).unwrap();
        let terms = block_on(TermIndex::open(MemoryFetch::new(rrt))).unwrap();
        let dict = block_on(terms.dict_terms()).unwrap();
        let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
        acc.add_doc(docs[0]);
        let mut rrb = Vec::new();
        write_impacts(&mut rrb, &dict, &acc, DEFAULT_K1, DEFAULT_B).unwrap();
        rrb[4] = 99; // version
        let err = block_on(ImpactIndex::open(MemoryFetch::new(rrb)));
        assert!(matches!(err, Err(IndexError::BadVersion(_))));
    }

    #[test]
    fn tokenizer_mismatch_fails_loudly() {
        let mut tb = TermIndexBuilder::new(&CFG);
        tb.add(0, "alpha beta");
        let mut rrt = Vec::new();
        tb.finish(&mut rrt).unwrap();
        let terms = block_on(TermIndex::open(MemoryFetch::new(rrt))).unwrap();
        let dict = block_on(terms.dict_terms()).unwrap();
        let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
        acc.add_doc("alpha only"); // "beta" never accumulated
        let mut rrb = Vec::new();
        assert!(write_impacts(&mut rrb, &dict, &acc, DEFAULT_K1, DEFAULT_B).is_err());
    }
}
