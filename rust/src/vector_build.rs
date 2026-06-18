//! Native build-side trainer and writer for the `RRVI` similarity index — the
//! build mirror of [`crate::vector`]. Excluded from the wasm reader build.
//!
//! [`build_ivfpq`] trains an IVFPQ index from a set of `(doc_id, vector)` pairs
//! with hand-rolled, dependency-free k-means: coarse centroids partition the
//! vectors into `nlist` clusters, residuals (`vector − assigned centroid`) are
//! product-quantized into `m` subspaces of `2^nbits` codes each, and every
//! vector is reduced to an `m`-byte PQ code stored in its cluster's list. The
//! resulting [`Ivfpq`] serializes to the byte layout in `VECTORS.md`, which the
//! pure-Rust [`crate::vector::VectorIndex`] reads back over HTTP Range.
//!
//! This trainer is intended for tests and small/medium corpora; the spec's
//! production path trains the index with FAISS and exports the same `RRVI`
//! layout. The output of either path is byte-compatible with the reader.

use crate::vector::{
    normalize, Metric, FLAG_OPQ, HEADER_SIZE, MAGIC, RERANK_BF16, RRVR_MAGIC, VERSION,
};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

/// Parameters for [`build_ivfpq`].
#[derive(Debug, Clone)]
pub struct IvfpqParams {
    /// Vector dimensionality. Every input vector must have this length.
    pub dim: usize,
    /// Number of coarse (IVF) clusters. Best `≤` the vector count; a common rule
    /// of thumb is `≈ 4·√N`.
    pub nlist: usize,
    /// Number of PQ subquantizers; must divide `dim`.
    pub m: usize,
    /// Bits per PQ code (`1..=8`); each subspace gets `2^nbits` codebook entries.
    pub nbits: u8,
    /// The similarity [`Metric`] to train/score for.
    pub metric: Metric,
    /// Lloyd's-iteration count for every k-means run (coarse and PQ).
    pub kmeans_iters: usize,
    /// Seed for the deterministic PRNG used to initialize k-means, so a build is
    /// reproducible.
    pub seed: u64,
}

impl IvfpqParams {
    /// Parameters with sensible defaults: 8-bit PQ codes, inner-product metric,
    /// 25 k-means iterations, a fixed seed. `m` must divide `dim`.
    pub fn new(dim: usize, nlist: usize, m: usize) -> Self {
        Self {
            dim,
            nlist,
            m,
            nbits: 8,
            metric: Metric::InnerProduct,
            kmeans_iters: 25,
            seed: 0x5251_5649_5252_5649,
        }
    }
}

/// An error from [`build_ivfpq`].
#[derive(Debug, PartialEq, Eq)]
pub enum VectorBuildError {
    /// No input vectors were supplied.
    Empty,
    /// A vector's length did not match [`IvfpqParams::dim`].
    DimMismatch,
    /// A parameter was out of range (the message says which).
    BadParams(&'static str),
}

impl fmt::Display for VectorBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VectorBuildError::Empty => write!(f, "no input vectors"),
            VectorBuildError::DimMismatch => write!(f, "a vector's length != params.dim"),
            VectorBuildError::BadParams(m) => write!(f, "bad params: {m}"),
        }
    }
}

impl Error for VectorBuildError {}

/// A trained IVFPQ index, ready to serialize with [`Ivfpq::write`] /
/// [`Ivfpq::to_bytes`]. Field order matches the on-disk boot region.
pub struct Ivfpq {
    dim: usize,
    nlist: usize,
    m: usize,
    nbits: u8,
    metric: Metric,
    n: u64,
    /// Optional OPQ rotation, `dim × dim` row-major. Trained externally (the
    /// hand-rolled trainer leaves it `None`); settable via [`Ivfpq::with_opq`].
    opq: Option<Vec<f32>>,
    /// Coarse centroids, `nlist × dim`.
    centroids: Vec<f32>,
    /// PQ codebooks, `m × 2^nbits × (dim/m)`.
    codebooks: Vec<f32>,
    /// Per-cluster vector IDs.
    list_ids: Vec<Vec<u32>>,
    /// Per-cluster PQ codes (`count × m` bytes, row-major).
    list_codes: Vec<Vec<u8>>,
}

impl Ivfpq {
    /// Attaches an OPQ rotation matrix (`dim × dim`, row-major; the reader
    /// applies `q' = R · q`). Sets the [`FLAG_OPQ`] header bit on write.
    pub fn with_opq(mut self, matrix: Vec<f32>) -> Self {
        self.opq = Some(matrix);
        self
    }

    /// Number of indexed vectors.
    pub fn len(&self) -> u64 {
        self.n
    }

    /// Whether the index holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Vector dimensionality.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of coarse (IVF) clusters actually trained. May be smaller than the
    /// requested `nlist` — [`build_ivfpq`] clamps it to the vector count.
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

    /// Serializes the index to `w` in the `RRVI` layout (`VECTORS.md`).
    pub fn write<W: Write>(&self, mut w: W) -> io::Result<()> {
        let ksub = 1usize << self.nbits;
        let dsub = self.dim / self.m;
        let opq_size = self.opq.as_ref().map_or(0, |_| self.dim * self.dim * 4);
        let centroids_size = self.nlist * self.dim * 4;
        let codebooks_size = self.m * ksub * dsub * 4;
        let dir_size = self.nlist * 12;
        let lists_off =
            (HEADER_SIZE + opq_size + centroids_size + codebooks_size + dir_size) as u64;

        // Header (48 B).
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&[u8::from(self.metric)])?;
        w.write_all(&[if self.opq.is_some() { FLAG_OPQ } else { 0 }])?;
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        w.write_all(&(self.nlist as u32).to_le_bytes())?;
        w.write_all(&(self.m as u32).to_le_bytes())?;
        w.write_all(&[self.nbits])?;
        w.write_all(&[0u8; 3])?; // pad
        w.write_all(&self.n.to_le_bytes())?;
        w.write_all(&[0u8; 16])?; // reserved

        // Boot blobs.
        if let Some(opq) = &self.opq {
            write_f32_all(&mut w, opq)?;
        }
        write_f32_all(&mut w, &self.centroids)?;
        write_f32_all(&mut w, &self.codebooks)?;

        // Cluster directory: absolute list offsets + counts.
        let mut off = lists_off;
        for (ids, codes) in self.list_ids.iter().zip(&self.list_codes) {
            let count = ids.len() as u32;
            w.write_all(&off.to_le_bytes())?;
            w.write_all(&count.to_le_bytes())?;
            off += (ids.len() * 4 + codes.len()) as u64;
        }

        // Range-fetched region: per cluster, `[ids][codes]`.
        for (ids, codes) in self.list_ids.iter().zip(&self.list_codes) {
            let mut buf = Vec::with_capacity(ids.len() * 4);
            for &id in ids {
                buf.extend_from_slice(&id.to_le_bytes());
            }
            w.write_all(&buf)?;
            w.write_all(codes)?;
        }
        Ok(())
    }

    /// Serializes the index to an in-memory byte vector (a convenience over
    /// [`Ivfpq::write`] for feeding [`crate::MemoryFetch`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out).expect("writing to a Vec cannot fail");
        out
    }
}

/// Writes a slice of `f32`s to `w` little-endian, batched through one buffer.
fn write_f32_all<W: Write>(w: &mut W, xs: &[f32]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(xs.len() * 4);
    for &x in xs {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    w.write_all(&buf)
}

/// Rounds an `f32` to bf16 (its high 16 bits) with round-to-nearest-even. Finite
/// inputs in a normalized vector's range never overflow; `NaN` stays `NaN`.
fn f32_to_bf16(x: f32) -> u16 {
    let bits = x.to_bits();
    if x.is_nan() {
        return ((bits >> 16) as u16) | 0x0040; // keep it a (quiet) NaN
    }
    let rounding_bias = ((bits >> 16) & 1) + 0x7fff;
    (bits.wrapping_add(rounding_bias) >> 16) as u16
}

/// Writes the `RRVR` re-rank sidecar read by [`crate::vector::RerankStore`]: a
/// 20-byte header then a dense bf16 array of `vectors` keyed by doc ID (the slice
/// index is the doc ID). Every vector must have length `dim`. Set `l2_normalize`
/// for an inner-product index so the stored vectors match the unit-sphere space
/// the index was built in. Pair the file with the `RRVI` index it re-ranks.
pub fn write_rerank<W: Write>(
    mut w: W,
    dim: usize,
    vectors: &[Vec<f32>],
    l2_normalize: bool,
) -> io::Result<()> {
    w.write_all(RRVR_MAGIC)?;
    w.write_all(&1u16.to_le_bytes())?; // version
    w.write_all(&[RERANK_BF16, 0])?; // precision, pad
    w.write_all(&(dim as u32).to_le_bytes())?;
    w.write_all(&(vectors.len() as u64).to_le_bytes())?;

    let mut buf = Vec::with_capacity(vectors.len() * dim * 2);
    for v in vectors {
        if v.len() != dim {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rerank vector length != dim",
            ));
        }
        if l2_normalize {
            for &x in &normalize(v) {
                buf.extend_from_slice(&f32_to_bf16(x).to_le_bytes());
            }
        } else {
            for &x in v {
                buf.extend_from_slice(&f32_to_bf16(x).to_le_bytes());
            }
        }
    }
    w.write_all(&buf)
}

/// Trains an IVFPQ index from `vectors` (each `(doc_id, vector)`), per `params`.
///
/// Vectors are L2-normalized first for an inner-product index. Coarse k-means
/// partitions them into `nlist` clusters; per-subspace k-means over the
/// residuals builds the `m` PQ codebooks and, in the same pass, the per-vector
/// codes. The returned [`Ivfpq`] is ready to [`Ivfpq::write`].
pub fn build_ivfpq(
    vectors: &[(u32, Vec<f32>)],
    params: &IvfpqParams,
) -> Result<Ivfpq, VectorBuildError> {
    if vectors.is_empty() {
        return Err(VectorBuildError::Empty);
    }
    let dim = params.dim;
    let m = params.m;
    if dim == 0 || m == 0 || !dim.is_multiple_of(m) {
        return Err(VectorBuildError::BadParams("dim/m: need dim>0, m>0, m|dim"));
    }
    if params.nbits == 0 || params.nbits > 8 {
        return Err(VectorBuildError::BadParams("nbits must be in 1..=8"));
    }
    if params.nlist == 0 {
        return Err(VectorBuildError::BadParams("nlist must be >= 1"));
    }
    if vectors.iter().any(|(_, v)| v.len() != dim) {
        return Err(VectorBuildError::DimMismatch);
    }

    let n = vectors.len();
    let ksub = 1usize << params.nbits;
    let dsub = dim / m;
    let iters = params.kmeans_iters.max(1);
    let nlist = params.nlist.min(n.max(1)); // never more clusters than vectors

    // Flatten (and normalize for IP) the inputs into one contiguous buffer.
    let mut pts = Vec::with_capacity(n * dim);
    let mut ids = Vec::with_capacity(n);
    for (id, v) in vectors {
        ids.push(*id);
        if params.metric == Metric::InnerProduct {
            pts.extend_from_slice(&normalize(v));
        } else {
            pts.extend_from_slice(v);
        }
    }

    let mut rng = Rng::new(params.seed);

    // Coarse quantizer.
    let (centroids, assign) = kmeans(&pts, n, dim, nlist, iters, &mut rng);

    // Residuals: vector − its assigned centroid.
    let mut res = vec![0f32; n * dim];
    for (i, (p, &a)) in pts.chunks_exact(dim).zip(assign.iter()).enumerate() {
        let c = &centroids[a as usize * dim..(a as usize + 1) * dim];
        let r = &mut res[i * dim..(i + 1) * dim];
        for ((slot, &x), &cv) in r.iter_mut().zip(p).zip(c) {
            *slot = x - cv;
        }
    }

    // PQ codebooks + per-vector codes, one subspace at a time. The subspace
    // k-means assignment *is* the code byte for that subspace.
    let mut codebooks = vec![0f32; m * ksub * dsub];
    let mut codes = vec![0u8; n * m];
    let mut subpts = vec![0f32; n * dsub];
    for s in 0..m {
        for (i, r) in res.chunks_exact(dim).enumerate() {
            subpts[i * dsub..(i + 1) * dsub].copy_from_slice(&r[s * dsub..(s + 1) * dsub]);
        }
        let (cb, code_s) = kmeans(&subpts, n, dsub, ksub, iters, &mut rng);
        codebooks[s * ksub * dsub..(s + 1) * ksub * dsub].copy_from_slice(&cb);
        for (i, &c) in code_s.iter().enumerate() {
            codes[i * m + s] = c as u8;
        }
    }

    // Scatter every vector into its cluster's list.
    let mut list_ids: Vec<Vec<u32>> = vec![Vec::new(); nlist];
    let mut list_codes: Vec<Vec<u8>> = vec![Vec::new(); nlist];
    for (i, &a) in assign.iter().enumerate() {
        let a = a as usize;
        list_ids[a].push(ids[i]);
        list_codes[a].extend_from_slice(&codes[i * m..(i + 1) * m]);
    }

    Ok(Ivfpq {
        dim,
        nlist,
        m,
        nbits: params.nbits,
        metric: params.metric,
        n: n as u64,
        opq: None,
        centroids,
        codebooks,
        list_ids,
        list_codes,
    })
}

/// Already-trained IVFPQ parts for [`build_ivfpq_from_parts`] — the output of an
/// external trainer (e.g. FAISS `OPQ,IVF,PQ`) ready to serialize without
/// re-training. All arrays are row-major and in the (optionally OPQ-rotated)
/// space the centroids/codebooks live in.
pub struct IvfpqParts {
    /// Vector dimensionality.
    pub dim: usize,
    /// Number of coarse (IVF) clusters.
    pub nlist: usize,
    /// PQ subquantizers (`dim % m == 0`).
    pub m: usize,
    /// Bits per PQ code (`1..=8`); `ksub = 1<<nbits`.
    pub nbits: u8,
    /// The similarity [`Metric`].
    pub metric: Metric,
    /// Coarse centroids, `nlist × dim`.
    pub centroids: Vec<f32>,
    /// PQ codebooks, `m × ksub × (dim/m)`.
    pub codebooks: Vec<f32>,
    /// Optional OPQ rotation, `dim × dim` row-major (the reader applies `q' = R·q`).
    pub opq: Option<Vec<f32>>,
    /// Per-vector doc IDs, length `n`.
    pub ids: Vec<u32>,
    /// Per-vector coarse-cluster assignment (`< nlist`), length `n`.
    pub assignments: Vec<u32>,
    /// Per-vector PQ codes, `n × m` bytes row-major.
    pub codes: Vec<u8>,
}

/// Assembles an [`Ivfpq`] from already-trained `parts` (e.g. exported from FAISS),
/// scattering each vector's code into its assigned cluster's list — no training.
/// The result is ready to [`Ivfpq::write`]. Validates that every array length is
/// consistent with `dim`/`nlist`/`m`/`nbits` and that assignments and codes are in
/// range, so a malformed export is rejected rather than written.
pub fn build_ivfpq_from_parts(parts: IvfpqParts) -> Result<Ivfpq, VectorBuildError> {
    let IvfpqParts {
        dim,
        nlist,
        m,
        nbits,
        metric,
        centroids,
        codebooks,
        opq,
        ids,
        assignments,
        codes,
    } = parts;

    if dim == 0 || m == 0 || !dim.is_multiple_of(m) {
        return Err(VectorBuildError::BadParams("dim/m: need dim>0, m>0, m|dim"));
    }
    if nbits == 0 || nbits > 8 {
        return Err(VectorBuildError::BadParams("nbits must be in 1..=8"));
    }
    if nlist == 0 {
        return Err(VectorBuildError::BadParams("nlist must be >= 1"));
    }
    let ksub = 1usize << nbits;
    let dsub = dim / m;
    if centroids.len() != nlist * dim {
        return Err(VectorBuildError::BadParams("centroids length != nlist*dim"));
    }
    if codebooks.len() != m * ksub * dsub {
        return Err(VectorBuildError::BadParams(
            "codebooks length != m*ksub*dsub",
        ));
    }
    if let Some(r) = &opq {
        if r.len() != dim * dim {
            return Err(VectorBuildError::BadParams("opq length != dim*dim"));
        }
    }
    let n = ids.len();
    if assignments.len() != n {
        return Err(VectorBuildError::BadParams(
            "assignments length != ids length",
        ));
    }
    if codes.len() != n * m {
        return Err(VectorBuildError::BadParams(
            "codes length != ids length * m",
        ));
    }
    if assignments.iter().any(|&a| a as usize >= nlist) {
        return Err(VectorBuildError::BadParams("an assignment >= nlist"));
    }
    if (nbits < 8) && codes.iter().any(|&c| c as usize >= ksub) {
        return Err(VectorBuildError::BadParams("a code >= ksub (1<<nbits)"));
    }

    // Scatter each vector's id + code into its assigned cluster's list.
    let mut list_ids: Vec<Vec<u32>> = vec![Vec::new(); nlist];
    let mut list_codes: Vec<Vec<u8>> = vec![Vec::new(); nlist];
    for (i, &a) in assignments.iter().enumerate() {
        let a = a as usize;
        list_ids[a].push(ids[i]);
        list_codes[a].extend_from_slice(&codes[i * m..(i + 1) * m]);
    }

    Ok(Ivfpq {
        dim,
        nlist,
        m,
        nbits,
        metric,
        n: n as u64,
        opq,
        centroids,
        codebooks,
        list_ids,
        list_codes,
    })
}

/// Runs Lloyd's k-means on `n` points of `dim` dims (flattened row-major) into
/// `k` clusters for `iters` iterations, returning the `k × dim` centroids and the
/// final per-point assignment. Centroids are seeded from random points; empty
/// clusters are reseeded to a random point each iteration.
fn kmeans(
    points: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    iters: usize,
    rng: &mut Rng,
) -> (Vec<f32>, Vec<u32>) {
    let mut centroids = vec![0f32; k * dim];
    for c in centroids.chunks_exact_mut(dim) {
        let r = rng.next_usize(n);
        c.copy_from_slice(&points[r * dim..(r + 1) * dim]);
    }

    let mut assign = vec![0u32; n];
    let mut sums = vec![0f32; k * dim];
    let mut counts = vec![0u32; k];
    for _ in 0..iters {
        for (i, p) in points.chunks_exact(dim).enumerate() {
            assign[i] = nearest(p, &centroids, dim) as u32;
        }
        sums.iter_mut().for_each(|x| *x = 0.0);
        counts.iter_mut().for_each(|x| *x = 0);
        for (p, &a) in points.chunks_exact(dim).zip(assign.iter()) {
            let a = a as usize;
            counts[a] += 1;
            for (acc, &x) in sums[a * dim..(a + 1) * dim].iter_mut().zip(p) {
                *acc += x;
            }
        }
        for c in 0..k {
            if counts[c] == 0 {
                let r = rng.next_usize(n);
                centroids[c * dim..(c + 1) * dim].copy_from_slice(&points[r * dim..(r + 1) * dim]);
            } else {
                let inv = 1.0 / counts[c] as f32;
                for (cen, &sm) in centroids[c * dim..(c + 1) * dim]
                    .iter_mut()
                    .zip(&sums[c * dim..(c + 1) * dim])
                {
                    *cen = sm * inv;
                }
            }
        }
    }
    // Final assignment consistent with the returned centroids.
    for (i, p) in points.chunks_exact(dim).enumerate() {
        assign[i] = nearest(p, &centroids, dim) as u32;
    }
    (centroids, assign)
}

/// Index of the centroid nearest `p` by squared L2 distance.
fn nearest(p: &[f32], centroids: &[f32], dim: usize) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (j, c) in centroids.chunks_exact(dim).enumerate() {
        let d: f32 = p
            .iter()
            .zip(c)
            .map(|(a, b)| {
                let diff = a - b;
                diff * diff
            })
            .sum();
        if d < best_d {
            best_d = d;
            best = j;
        }
    }
    best
}

/// A tiny deterministic PRNG (xorshift64*), so a build is reproducible without a
/// dependency on `rand`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A pseudo-random index in `0..n` (`n` must be nonzero).
    fn next_usize(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}
