//! The `RRVI` range-fetchable similarity-search index reader.
//!
//! Companion to [`crate::index`] for *vector* (semantic) search in the same
//! static, range-fetched-from-S3 ethos as the trigram index. The index is an
//! IVFPQ structure (inverted file of coarse clusters, each vector stored as a
//! product-quantization code): a small **boot** region (coarse centroids, PQ
//! codebooks, an optional OPQ rotation, and a cluster directory) is downloaded
//! once, and each query range-fetches only the `nprobe` nearest clusters' code
//! lists — independent of corpus size, exactly like the trigram reader's
//! head/tail postings.
//!
//! [`VectorIndex::open`] performs the one-time boot; [`VectorIndex::search`]
//! then issues `nprobe` concurrent ranged reads (one wave) and scans the fetched
//! codes with asymmetric distance computation (ADC) to produce the top-k doc
//! IDs. `vector_id == doc_id`, so results map straight back through the existing
//! [`crate::records::RecordStore`]. The build-side trainer/writer that produces
//! an `RRVI` file lives in [`crate::vector_build`] (native only). See
//! `VECTORS.md` for the byte layout.

use crate::fetch::RangeFetch;
use crate::index::{read_u32, read_u64, IndexError};
use futures::future::join_all;
use std::collections::BinaryHeap;

/// `RRVI` magic.
pub(crate) const MAGIC: &[u8; 4] = b"RRVI";
/// Format version written into / accepted from the header.
pub(crate) const VERSION: u16 = 1;
/// Fixed header size in bytes (see `VECTORS.md`): magic[4] + version[2] +
/// metric[1] + flags[1] + dim[4] + nlist[4] + m[4] + nbits[1] + pad[3] + n[8] +
/// reserved[16].
pub(crate) const HEADER_SIZE: usize = 48;
/// Cluster-directory entry size: offset(u64) + count(u32).
pub(crate) const DIR_ENTRY: usize = 12;

/// Metric tag: inner product on L2-normalized vectors (cosine). The default; the
/// stored codes encode L2 residuals, but for unit vectors L2 and inner product
/// rank identically, so the same ADC scan serves both — only the reported score
/// differs (see [`VectorIndex::search`]). Pass to [`crate::IvfpqParams`].
pub const METRIC_IP: u8 = 0;
/// Metric tag: raw L2 distance. Pass to [`crate::IvfpqParams`].
pub const METRIC_L2: u8 = 1;

/// Header flag: an OPQ rotation matrix precedes the centroids in the boot region.
pub(crate) const FLAG_OPQ: u8 = 1 << 0;

/// `RRVR` re-rank-blob magic (the optional higher-precision sidecar).
pub(crate) const RRVR_MAGIC: &[u8; 4] = b"RRVR";
/// `RRVR` header size: magic[4] + version[2] + precision[1] + pad[1] + dim[4] + n[8].
pub(crate) const RRVR_HEADER_SIZE: usize = 20;
/// Re-rank precision tag: bf16 — the high 16 bits of each f32 (full f32 exponent
/// range, 8-bit mantissa). Trivially exact to decode, ~2-3 significant digits,
/// 2 bytes/dim — plenty to re-rank a small candidate set above 8-bit PQ codes.
pub(crate) const RERANK_BF16: u8 = 0;

/// One similarity-search result: a doc ID and its score (higher is more similar).
///
/// For an inner-product index the score is the approximate cosine similarity
/// (`1 - dist/2` over the PQ-reconstructed distance); for an L2 index it is the
/// negated approximate squared distance. Either way, sorting by `score`
/// descending orders results best-first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    /// The matching document's ID (identical to the trigram index's doc ID).
    pub doc_id: u32,
    /// Similarity score; higher is better.
    pub score: f32,
}

/// Reads `count` little-endian `f32`s starting at `buf[off..]`.
fn read_f32_vec(buf: &[u8], off: usize, count: usize) -> Vec<f32> {
    buf[off..off + count * 4]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// A candidate in a bounded top-k search, ordered by ascending distance so a
/// [`BinaryHeap`] (a max-heap) keeps the *k smallest* distances: the heap's root
/// is the current worst (largest distance), popped when a better candidate
/// arrives. `NaN` distances are pushed to the end via [`f32::total_cmp`].
#[derive(Debug, Clone, Copy)]
struct Candidate {
    dist: f32,
    id: u32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.dist.total_cmp(&other.dist) == std::cmp::Ordering::Equal && self.id == other.id
    }
}
impl Eq for Candidate {}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Primary: by distance. Tie-break by id so a fixed input yields a stable
        // ordering; smaller id (more popular doc) is treated as "smaller".
        self.dist
            .total_cmp(&other.dist)
            .then(self.id.cmp(&other.id))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A range-fetchable `RRVI` similarity index. Holds the boot region (centroids,
/// PQ codebooks, optional OPQ rotation, cluster directory) in memory; the
/// per-cluster code lists are read on demand via `F`.
pub struct VectorIndex<F: RangeFetch> {
    fetch: F,
    /// Vector dimensionality.
    dim: usize,
    /// Number of coarse (IVF) clusters.
    nlist: usize,
    /// Number of PQ subquantizers (`dim % m == 0`).
    m: usize,
    /// Bits per PQ code (`1..=8`); `ksub == 1 << nbits` codes per subspace.
    nbits: u8,
    /// `1 << nbits`: codebook entries per subspace.
    ksub: usize,
    /// `dim / m`: dimensionality of each PQ subspace.
    dsub: usize,
    /// Metric tag ([`METRIC_IP`] or [`METRIC_L2`]).
    metric: u8,
    /// Total vector count.
    n: u64,
    /// Optional OPQ rotation matrix, `dim × dim` row-major (`q' = R · q`).
    opq: Option<Vec<f32>>,
    /// Coarse centroids, `nlist × dim` row-major.
    centroids: Vec<f32>,
    /// PQ codebooks, `m × ksub × dsub` row-major.
    codebooks: Vec<f32>,
    /// Per-cluster absolute file offset of its code list.
    dir_offsets: Vec<u64>,
    /// Per-cluster vector count.
    dir_counts: Vec<u32>,
}

impl<F: RangeFetch> VectorIndex<F> {
    /// Boots the index: reads the fixed header, then the whole boot region
    /// (OPQ + centroids + codebooks + directory) in one ranged read, parsing it
    /// into memory. Subsequent queries fetch only per-cluster code lists.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = crate::index::read_u16(&header, 4);
        if version != VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let metric = header[6];
        let flags = header[7];
        let dim = read_u32(&header, 8) as usize;
        let nlist = read_u32(&header, 12) as usize;
        let m = read_u32(&header, 16) as usize;
        let nbits = header[20];
        let n = read_u64(&header, 24);

        if dim == 0 || m == 0 || !dim.is_multiple_of(m) {
            return Err(IndexError::Malformed("RRVI dim/m invalid"));
        }
        if nbits == 0 || nbits > 8 {
            return Err(IndexError::Malformed("RRVI nbits out of range (1..=8)"));
        }
        if nlist == 0 {
            return Err(IndexError::Malformed("RRVI nlist is zero"));
        }
        let ksub = 1usize << nbits;
        let dsub = dim / m;

        // Boot-region layout, all sizes derived from the header.
        let opq_size = if flags & FLAG_OPQ != 0 {
            dim * dim * 4
        } else {
            0
        };
        let centroids_size = nlist * dim * 4;
        let codebooks_size = m * ksub * dsub * 4;
        let dir_size = nlist * DIR_ENTRY;
        let boot_size = opq_size + centroids_size + codebooks_size + dir_size;
        let boot = fetch.read(HEADER_SIZE as u64, boot_size).await?;

        let mut off = 0usize;
        let opq = if opq_size != 0 {
            let v = read_f32_vec(&boot, off, dim * dim);
            off += opq_size;
            Some(v)
        } else {
            None
        };
        let centroids = read_f32_vec(&boot, off, nlist * dim);
        off += centroids_size;
        let codebooks = read_f32_vec(&boot, off, m * ksub * dsub);
        off += codebooks_size;

        let mut dir_offsets = Vec::with_capacity(nlist);
        let mut dir_counts = Vec::with_capacity(nlist);
        for i in 0..nlist {
            let base = off + i * DIR_ENTRY;
            dir_offsets.push(read_u64(&boot, base));
            dir_counts.push(read_u32(&boot, base + 8));
        }

        Ok(Self {
            fetch,
            dim,
            nlist,
            m,
            nbits,
            ksub,
            dsub,
            metric,
            n,
            opq,
            centroids,
            codebooks,
            dir_offsets,
            dir_counts,
        })
    }

    /// Vector dimensionality the index was built with.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of coarse (IVF) clusters.
    pub fn nlist(&self) -> usize {
        self.nlist
    }

    /// Number of PQ subquantizers.
    pub fn subquantizers(&self) -> usize {
        self.m
    }

    /// Bits per PQ code.
    pub fn nbits(&self) -> u8 {
        self.nbits
    }

    /// Total number of indexed vectors.
    pub fn len(&self) -> u64 {
        self.n
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Applies the OPQ rotation to a query if present (`q' = R · q`), else clones
    /// the query. The rotation is orthonormal, so it preserves norms.
    fn rotate(&self, q: &[f32]) -> Vec<f32> {
        match &self.opq {
            None => q.to_vec(),
            Some(r) => (0..self.dim)
                .map(|i| {
                    let row = &r[i * self.dim..(i + 1) * self.dim];
                    row.iter().zip(q).map(|(a, b)| a * b).sum()
                })
                .collect(),
        }
    }

    /// Returns the indices of the `nprobe` clusters whose centroids are nearest
    /// (by squared L2) to the rotated query, nearest-first. Computed wholly in
    /// memory from the booted centroids — no fetch.
    fn nearest_clusters(&self, q: &[f32], nprobe: usize) -> Vec<usize> {
        // Keep the nprobe smallest distances in a max-heap of that size.
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::with_capacity(nprobe + 1);
        for (j, centroid) in self.centroids.chunks_exact(self.dim).enumerate() {
            let dist: f32 = centroid
                .iter()
                .zip(q)
                .map(|(c, x)| {
                    let d = c - x;
                    d * d
                })
                .sum();
            let cand = Candidate { dist, id: j as u32 };
            if heap.len() < nprobe {
                heap.push(cand);
            } else if let Some(worst) = heap.peek() {
                if cand < *worst {
                    heap.pop();
                    heap.push(cand);
                }
            }
        }
        let mut probed: Vec<Candidate> = heap.into_vec();
        probed.sort_unstable();
        probed.iter().map(|c| c.id as usize).collect()
    }

    /// Builds the ADC distance table for one probed cluster: `table[s*ksub + k]`
    /// is the squared L2 distance between the query residual's subvector `s` and
    /// codebook centroid `k` of subspace `s`. The database vectors are quantized;
    /// the query is not (asymmetric distance computation).
    fn adc_table(&self, residual: &[f32]) -> Vec<f32> {
        let mut table = vec![0f32; self.m * self.ksub];
        for s in 0..self.m {
            let rsub = &residual[s * self.dsub..(s + 1) * self.dsub];
            let cb = &self.codebooks[s * self.ksub * self.dsub..(s + 1) * self.ksub * self.dsub];
            for (k, cent) in cb.chunks_exact(self.dsub).enumerate() {
                let d: f32 = rsub
                    .iter()
                    .zip(cent)
                    .map(|(a, b)| {
                        let diff = a - b;
                        diff * diff
                    })
                    .sum();
                table[s * self.ksub + k] = d;
            }
        }
        table
    }

    /// Searches the index for the `k` nearest vectors to `query`, probing the
    /// `nprobe` nearest clusters.
    ///
    /// The `nprobe` clusters' code lists are read in one concurrent wave; each is
    /// scanned with its own ADC table (the residual `query - centroid` is
    /// per-cluster). Results come back sorted best-first. An empty query result
    /// (`k == 0` or `nprobe == 0`) is `Ok(vec![])`; a `query` whose length is not
    /// the index's `dim` is [`IndexError::BadQuery`].
    pub async fn search(
        &self,
        query: &[f32],
        k: usize,
        nprobe: usize,
    ) -> Result<Vec<VectorHit>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::BadQuery("query vector dim != index dim"));
        }
        let nprobe = nprobe.min(self.nlist);
        if k == 0 || nprobe == 0 {
            return Ok(Vec::new());
        }
        // Normalize a copy for an inner-product index so callers need not, then
        // rotate into the index's (optionally OPQ-) space.
        let q = if self.metric == METRIC_IP {
            normalize(query)
        } else {
            query.to_vec()
        };
        let q = self.rotate(&q);

        let probed = self.nearest_clusters(&q, nprobe);

        // WAVE: fetch every non-empty probed cluster's code list concurrently.
        // A list block is `[u32 id × count][u8 code × (count*m)]`.
        let active: Vec<usize> = probed
            .into_iter()
            .filter(|&j| self.dir_counts[j] > 0)
            .collect();
        let reads = active.iter().map(|&j| {
            let count = self.dir_counts[j] as usize;
            let len = count * (4 + self.m);
            self.fetch.read(self.dir_offsets[j], len)
        });
        let blocks = join_all(reads).await;

        let mut heap: BinaryHeap<Candidate> = BinaryHeap::with_capacity(k + 1);
        for (&j, block) in active.iter().zip(blocks) {
            let block = block?;
            let count = self.dir_counts[j] as usize;
            let ids_len = count * 4;
            if block.len() != ids_len + count * self.m {
                return Err(IndexError::Malformed("RRVI cluster block size mismatch"));
            }
            let centroid = &self.centroids[j * self.dim..(j + 1) * self.dim];
            let residual: Vec<f32> = q.iter().zip(centroid).map(|(a, b)| a - b).collect();
            let table = self.adc_table(&residual);

            let (ids, codes) = block.split_at(ids_len);
            for (id_bytes, code) in ids.chunks_exact(4).zip(codes.chunks_exact(self.m)) {
                let id = u32::from_le_bytes([id_bytes[0], id_bytes[1], id_bytes[2], id_bytes[3]]);
                let dist: f32 = code
                    .iter()
                    .enumerate()
                    .map(|(s, &c)| table[s * self.ksub + c as usize])
                    .sum();
                let cand = Candidate { dist, id };
                if heap.len() < k {
                    heap.push(cand);
                } else if let Some(worst) = heap.peek() {
                    if cand < *worst {
                        heap.pop();
                        heap.push(cand);
                    }
                }
            }
        }

        let mut found: Vec<Candidate> = heap.into_vec();
        found.sort_unstable(); // ascending distance, then id
        Ok(found
            .into_iter()
            .map(|c| VectorHit {
                doc_id: c.id,
                score: self.score_of(c.dist),
            })
            .collect())
    }

    /// Converts an approximate squared-L2 distance to the reported score. For an
    /// inner-product (unit-vector) index, `‖q-x‖² = 2 - 2·cos`, so the cosine is
    /// `1 - dist/2`; for an L2 index the score is the negated distance.
    fn score_of(&self, dist: f32) -> f32 {
        if self.metric == METRIC_IP {
            1.0 - 0.5 * dist
        } else {
            -dist
        }
    }

    /// Like [`VectorIndex::search`] but re-ranks the approximate ADC top-`r`
    /// candidates against their higher-precision vectors from a [`RerankStore`]
    /// sidecar, then returns the exact top-`k`.
    ///
    /// The PQ ADC scan is lossy; fetching the (bf16) original vectors for only the
    /// `r` best candidates and rescoring them exactly recovers most of that loss
    /// for a few extra small ranged reads. `r` is clamped up to `k`. With `r`
    /// large enough to cover the true neighbors this returns the exact-metric
    /// top-`k`. The re-rank score replaces the ADC score in each [`VectorHit`].
    pub async fn search_rerank(
        &self,
        query: &[f32],
        k: usize,
        nprobe: usize,
        r: usize,
        rerank: &RerankStore<F>,
    ) -> Result<Vec<VectorHit>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::BadQuery("query vector dim != index dim"));
        }
        if rerank.dim() != self.dim {
            return Err(IndexError::BadQuery("rerank dim != index dim"));
        }
        if k == 0 || nprobe == 0 {
            return Ok(Vec::new());
        }
        // Approximate candidate set (ADC top-r), then exact re-rank.
        let candidates = self.search(query, r.max(k), nprobe).await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<u32> = candidates.iter().map(|c| c.doc_id).collect();
        let vecs = rerank.get_many(&ids).await?;

        // The re-rank vectors live in the original (un-rotated) space, so use the
        // normalized-but-unrotated query — the same space the corpus was stored in.
        let q = if self.metric == METRIC_IP {
            normalize(query)
        } else {
            query.to_vec()
        };
        let mut scored: Vec<VectorHit> = ids
            .iter()
            .zip(&vecs)
            .map(|(&id, v)| {
                let score = if self.metric == METRIC_IP {
                    dot(&q, v)
                } else {
                    -l2_sq(&q, v)
                };
                VectorHit { doc_id: id, score }
            })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
        scored.truncate(k);
        Ok(scored)
    }
}

/// Returns an L2-normalized copy of `v`. A zero vector is returned unchanged
/// (its norm is zero), which keeps the result finite.
pub(crate) fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Decodes a bf16 (the high 16 bits of an f32) back to `f32` — exact, by placing
/// the stored bits in the high half of the f32 and zeroing the low mantissa.
fn bf16_to_f32(b: u16) -> f32 {
    f32::from_bits((b as u32) << 16)
}

/// Inner product of two equal-length vectors.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Squared L2 distance between two equal-length vectors.
fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// A range-fetchable `RRVR` re-rank sidecar: higher-precision (bf16) vectors keyed
/// densely by doc ID, so a query can fetch the exact vectors for only its top-`r`
/// PQ candidates and rescore them. See [`VectorIndex::search_rerank`].
pub struct RerankStore<F: RangeFetch> {
    fetch: F,
    dim: usize,
    n: u64,
    precision: u8,
}

impl<F: RangeFetch> RerankStore<F> {
    /// Boots the sidecar: reads and validates the 20-byte header.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let h = fetch.read(0, RRVR_HEADER_SIZE).await?;
        if &h[0..4] != RRVR_MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&h[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = crate::index::read_u16(&h, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let precision = h[6];
        if precision != RERANK_BF16 {
            return Err(IndexError::Malformed("RRVR unsupported precision"));
        }
        let dim = read_u32(&h, 8) as usize;
        if dim == 0 {
            return Err(IndexError::Malformed("RRVR dim is zero"));
        }
        let n = read_u64(&h, 12);
        Ok(Self {
            fetch,
            dim,
            n,
            precision,
        })
    }

    /// Vector dimensionality (must match the `RRVI` index it re-ranks).
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of stored vectors (doc IDs `0..n`).
    pub fn len(&self) -> u64 {
        self.n
    }

    /// Whether the sidecar holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Precision tag of the stored vectors ([`RERANK_BF16`]).
    pub fn precision(&self) -> u8 {
        self.precision
    }

    /// Fetches and decodes the stored vectors for `ids` (output aligned with
    /// `ids`) in one concurrent wave of ranged reads. Each vector is `dim` bf16
    /// values at `RRVR_HEADER_SIZE + id*dim*2`.
    pub async fn get_many(&self, ids: &[u32]) -> Result<Vec<Vec<f32>>, IndexError> {
        let stride = self.dim * 2;
        let reads = ids.iter().map(|&id| {
            let off = RRVR_HEADER_SIZE as u64 + id as u64 * stride as u64;
            self.fetch.read(off, stride)
        });
        let results = join_all(reads).await;
        let mut out = Vec::with_capacity(ids.len());
        for bytes in results {
            let b = bytes?;
            if b.len() != stride {
                return Err(IndexError::Malformed("RRVR short read"));
            }
            out.push(
                b.chunks_exact(2)
                    .map(|c| bf16_to_f32(u16::from_le_bytes([c[0], c[1]])))
                    .collect(),
            );
        }
        Ok(out)
    }
}

/// Reciprocal-rank fusion of several ranked doc-ID lists into one ranking. Each
/// list contributes `1/(k_param + rank)` to a doc's score (rank starting at 1,
/// best-first), so a doc near the top of several lists ranks highest without the
/// lists needing comparable scores — the standard way to blend the trigram
/// (`RRS`) and vector (`RRVI`) result sets. Returns `(doc_id, fused_score)` sorted
/// best-first, ties broken by ascending doc ID. `k_param` is conventionally ~60.
pub fn reciprocal_rank_fusion(lists: &[&[u32]], k_param: f64) -> Vec<(u32, f64)> {
    use std::collections::HashMap;
    let mut acc: HashMap<u32, f64> = HashMap::new();
    for list in lists {
        for (rank, &id) in list.iter().enumerate() {
            *acc.entry(id).or_insert(0.0) += 1.0 / (k_param + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(u32, f64)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}
