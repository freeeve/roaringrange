//! End-to-end tests for the `RRVI` similarity index: train an IVFPQ index with
//! the native trainer, serialize it, then read it back over [`MemoryFetch`] and
//! check the search against a brute-force exact-nearest-neighbor baseline.
//!
//! Gated on the `vector` feature (run with `cargo test --features vector`); the
//! file compiles to nothing without it.
#![cfg(feature = "vector")]

use futures::executor::block_on;
use roaringrange::vector::{reciprocal_rank_fusion, Metric, RerankStore, VectorIndex};
use roaringrange::{
    build_ivfpq, build_ivfpq_from_parts, write_rerank, IvfpqParams, IvfpqParts, MemoryFetch,
    VectorBuildError, VectorHit,
};

/// A tiny deterministic PRNG (xorshift64*) so the synthetic corpora and queries
/// are reproducible without a dependency.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x1234_5678_9abc_def0
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
    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Standard-normal sample via Box–Muller.
    fn gauss(&mut self) -> f32 {
        let u1 = self.unit().max(1e-7);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

/// L2-normalizes a vector (zero vectors pass through).
fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Cosine similarity (inner product) of two L2-normalized vectors.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Exact top-`k` doc IDs by cosine over the (already-normalized) corpus.
fn exact_topk(corpus: &[(u32, Vec<f32>)], query: &[f32], k: usize) -> Vec<u32> {
    let mut scored: Vec<(f32, u32)> = corpus.iter().map(|(id, v)| (dot(v, query), *id)).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Builds an index from `corpus`, serializes it, and opens a [`VectorIndex`] over
/// the bytes via [`MemoryFetch`].
fn open_index(corpus: &[(u32, Vec<f32>)], params: &IvfpqParams) -> VectorIndex<MemoryFetch> {
    let trained = build_ivfpq(corpus, params).expect("build_ivfpq");
    let bytes = trained.to_bytes();
    block_on(VectorIndex::open(MemoryFetch::new(bytes))).expect("open RRVI")
}

/// Generates `nb` well-separated Gaussian blobs of `per` points each in `dim`
/// dimensions; returns the normalized corpus paired with ascending doc IDs.
fn blob_corpus(rng: &mut Rng, dim: usize, nb: usize, per: usize) -> Vec<(u32, Vec<f32>)> {
    let centers: Vec<Vec<f32>> = (0..nb)
        .map(|_| (0..dim).map(|_| rng.gauss() * 5.0).collect())
        .collect();
    let mut corpus = Vec::with_capacity(nb * per);
    let mut id = 0u32;
    for c in &centers {
        for _ in 0..per {
            let v: Vec<f32> = c.iter().map(|&m| m + rng.gauss() * 0.5).collect();
            corpus.push((id, normalize(&v)));
            id += 1;
        }
    }
    corpus
}

#[test]
fn header_roundtrips() {
    let mut rng = Rng::new(1);
    let corpus: Vec<(u32, Vec<f32>)> = (0..64)
        .map(|i| {
            (
                i,
                normalize(&(0..8).map(|_| rng.gauss()).collect::<Vec<_>>()),
            )
        })
        .collect();
    let params = IvfpqParams::new(8, 4, 4);
    let idx = open_index(&corpus, &params);
    assert_eq!(idx.dim(), 8);
    assert_eq!(idx.nlist(), 4);
    assert_eq!(idx.subquantizers(), 4);
    assert_eq!(idx.nbits(), 8);
    assert_eq!(idx.len(), 64);
    assert!(!idx.is_empty());
}

#[test]
fn returns_all_docs_once_when_scanning_every_cluster() {
    // With nprobe == nlist and k == N the search scans every cluster and keeps
    // every vector, so the result must be exactly the input doc IDs (each once),
    // ordered best-first (non-increasing score). This exercises the full
    // boot → directory → list-fetch → ADC → top-k path and the id round-trip.
    let mut rng = Rng::new(7);
    let n = 200usize;
    let corpus: Vec<(u32, Vec<f32>)> = (0..n as u32)
        .map(|i| {
            (
                i,
                normalize(&(0..16).map(|_| rng.gauss()).collect::<Vec<_>>()),
            )
        })
        .collect();
    let params = IvfpqParams::new(16, 8, 8);
    let idx = open_index(&corpus, &params);
    let query = corpus[0].1.clone();
    let hits = block_on(idx.search(&query, n, idx.nlist())).expect("search");
    assert_eq!(hits.len(), n, "should return every vector");
    let mut ids: Vec<u32> = hits.iter().map(|h| h.doc_id).collect();
    ids.sort_unstable();
    let expect: Vec<u32> = (0..n as u32).collect();
    assert_eq!(ids, expect, "every doc id present exactly once");
    for w in hits.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "scores must be non-increasing: {} then {}",
            w[0].score,
            w[1].score
        );
    }
}

#[test]
fn self_query_finds_itself_with_high_score() {
    // Querying with an indexed vector should rank that vector first with a near-1
    // cosine score (its own residual reconstructs to ~itself under PQ).
    let mut rng = Rng::new(11);
    let corpus = blob_corpus(&mut rng, 16, 12, 40);
    let params = IvfpqParams::new(16, 16, 8);
    let idx = open_index(&corpus, &params);
    for &probe in &[0usize, 137, 400, 479] {
        let (id, ref v) = corpus[probe];
        let hits = block_on(idx.search(v, 5, idx.nlist())).expect("search");
        assert_eq!(hits[0].doc_id, id, "self should be top-1");
        assert!(
            hits[0].score > 0.9,
            "self score should be ~1, got {}",
            hits[0].score
        );
    }
}

#[test]
fn recall_at_10_beats_brute_force_threshold() {
    // The core spec check: IVFPQ recall@10 against exact cosine NN on clustered
    // data. Deterministic (fixed seeds), so the threshold is a fixed floor.
    let mut rng = Rng::new(2024);
    let dim = 16usize;
    let corpus = blob_corpus(&mut rng, dim, 16, 100); // 1600 vectors
    let params = IvfpqParams::new(dim, 16, 8);
    let idx = open_index(&corpus, &params);

    let k = 10usize;
    let nprobe = 8usize;
    let queries = 40usize;
    let mut total = 0.0f64;
    let mut qrng = Rng::new(99);
    for _ in 0..queries {
        // A query near a random corpus point (same blob neighborhood).
        let base = &corpus[qrng.next_u64() as usize % corpus.len()].1;
        let q: Vec<f32> = base.iter().map(|&x| x + qrng.gauss() * 0.1).collect();
        let q = normalize(&q);

        let exact = exact_topk(&corpus, &q, k);
        let approx: Vec<u32> = block_on(idx.search(&q, k, nprobe))
            .expect("search")
            .into_iter()
            .map(|h| h.doc_id)
            .collect();
        let hit = approx.iter().filter(|id| exact.contains(id)).count();
        total += hit as f64 / k as f64;
    }
    let recall = total / queries as f64;
    assert!(
        recall >= 0.75,
        "mean recall@{k} = {recall:.3}, expected >= 0.75"
    );
}

#[test]
fn identity_opq_matches_no_opq() {
    // An identity OPQ rotation must leave results unchanged — it exercises the
    // reader's OPQ boot-region parsing and `q' = R·q` path while being a no-op.
    let mut rng = Rng::new(5);
    let corpus = blob_corpus(&mut rng, 8, 8, 30);
    let params = IvfpqParams::new(8, 8, 4);

    let plain = build_ivfpq(&corpus, &params).unwrap();
    let mut identity = vec![0f32; 8 * 8];
    for i in 0..8 {
        identity[i * 8 + i] = 1.0;
    }
    let with_opq = build_ivfpq(&corpus, &params).unwrap().with_opq(identity);

    let idx_a = block_on(VectorIndex::open(MemoryFetch::new(plain.to_bytes()))).unwrap();
    let idx_b = block_on(VectorIndex::open(MemoryFetch::new(with_opq.to_bytes()))).unwrap();

    let q = corpus[100].1.clone();
    let a: Vec<VectorHit> = block_on(idx_a.search(&q, 10, idx_a.nlist())).unwrap();
    let b: Vec<VectorHit> = block_on(idx_b.search(&q, 10, idx_b.nlist())).unwrap();
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.doc_id, y.doc_id);
        assert!(
            (x.score - y.score).abs() < 1e-6,
            "{} vs {}",
            x.score,
            y.score
        );
    }
}

#[test]
fn from_parts_matches_hand_computed_adc() {
    // Feed already-"trained" parts (as a FAISS export would) and check the reader
    // reproduces ADC distances computed by hand. L2 metric => score == -dist, so
    // the scores are directly comparable. dim=4, m=2 (dsub=2), nbits=2 (ksub=4),
    // nlist=2; each PQ subspace codebook is {[0,0],[1,0],[0,1],[1,1]}.
    let codebook_2d: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
    let mut codebooks = Vec::new();
    codebooks.extend_from_slice(&codebook_2d); // subspace 0
    codebooks.extend_from_slice(&codebook_2d); // subspace 1
    let centroids = vec![0.0, 0.0, 0.0, 0.0, 2.0, 2.0, 2.0, 2.0]; // c0, c1

    // (id, cluster, [code0, code1])
    let ids = vec![10u32, 11, 12, 13];
    let assignments = vec![0u32, 0, 1, 1];
    let codes = vec![3u8, 3, 0, 0, 0, 0, 3, 3];

    let parts = IvfpqParts {
        dim: 4,
        nlist: 2,
        m: 2,
        nbits: 2,
        metric: Metric::L2,
        centroids,
        codebooks,
        opq: None,
        ids,
        assignments,
        codes,
    };
    let idx = {
        let built = build_ivfpq_from_parts(parts).expect("from_parts");
        assert_eq!(built.len(), 4);
        block_on(VectorIndex::open(MemoryFetch::new(built.to_bytes()))).unwrap()
    };

    let q = [0.2f32, 0.3, 1.6, 1.7];
    let hits = block_on(idx.search(&q, 4, 2)).expect("search");

    // Hand-computed squared-L2 ADC distances (see comment above).
    let expect: [(u32, f32); 4] = [
        (10, 1.98),  // table0[3]+table1[3] = 1.13 + 0.85
        (11, 5.58),  // table0[0]+table1[0] = 0.13 + 5.45
        (12, 6.38),  // c1: 6.13 + 0.25
        (13, 18.78), // c1: 15.13 + 3.65
    ];
    assert_eq!(hits.len(), 4);
    for (h, (id, dist)) in hits.iter().zip(&expect) {
        assert_eq!(h.doc_id, *id, "order mismatch: {hits:?}");
        assert!(
            (h.score - (-dist)).abs() < 1e-4,
            "doc {id}: score {} != {}",
            h.score,
            -dist
        );
    }
}

#[test]
fn from_parts_rejects_inconsistent_arrays() {
    // A short codes array (should be n*m) is rejected, not written.
    let parts = IvfpqParts {
        dim: 4,
        nlist: 2,
        m: 2,
        nbits: 2,
        metric: Metric::L2,
        centroids: vec![0.0; 8],
        codebooks: vec![0.0; 2 * 4 * 2],
        opq: None,
        ids: vec![0, 1],
        assignments: vec![0, 1],
        codes: vec![0u8; 3], // should be 2*2 = 4
    };
    assert!(matches!(
        build_ivfpq_from_parts(parts),
        Err(VectorBuildError::BadParams(_))
    ));
}

/// Builds an `RRVR` re-rank sidecar from a corpus (in doc-ID order) and opens it.
fn open_rerank(corpus: &[(u32, Vec<f32>)], dim: usize) -> RerankStore<MemoryFetch> {
    let vecs: Vec<Vec<f32>> = corpus.iter().map(|(_, v)| v.clone()).collect();
    let mut bytes = Vec::new();
    write_rerank(&mut bytes, dim, &vecs, true).expect("write_rerank");
    block_on(RerankStore::open(MemoryFetch::new(bytes))).expect("open rerank")
}

#[test]
fn rerank_recovers_near_exact_topk() {
    // Re-ranking every probed candidate (nprobe = nlist, r = N) against the bf16
    // sidecar should reproduce the exact-cosine top-k almost perfectly — far
    // closer than the lossy PQ ADC scan alone. Uses non-degenerate random vectors
    // so the true top-k are well-separated (no cosine near-ties for bf16 to flip).
    let mut rng = Rng::new(31);
    let dim = 16usize;
    let n = 800u32;
    let corpus: Vec<(u32, Vec<f32>)> = (0..n)
        .map(|i| {
            (
                i,
                normalize(&(0..dim).map(|_| rng.gauss()).collect::<Vec<_>>()),
            )
        })
        .collect();
    let params = IvfpqParams::new(dim, 16, 8);
    let idx = open_index(&corpus, &params);
    let rerank = open_rerank(&corpus, dim);
    assert_eq!(rerank.dim(), dim);
    assert_eq!(rerank.len(), n);

    let k = 10usize;
    let mut qrng = Rng::new(64);
    let mut total = 0.0f64;
    let queries = 30usize;
    for _ in 0..queries {
        let base = &corpus[qrng.next_u64() as usize % corpus.len()].1;
        let q: Vec<f32> = base.iter().map(|&x| x + qrng.gauss() * 0.05).collect();
        let q = normalize(&q);
        let exact = exact_topk(&corpus, &q, k);
        let got: Vec<u32> = block_on(idx.search_rerank(&q, k, idx.nlist(), corpus.len(), &rerank))
            .expect("search_rerank")
            .into_iter()
            .map(|h| h.doc_id)
            .collect();
        total += got.iter().filter(|id| exact.contains(id)).count() as f64 / k as f64;
    }
    let recall = total / queries as f64;
    assert!(
        recall >= 0.95,
        "rerank recall@{k} = {recall:.3}, expected >= 0.95"
    );
}

#[test]
fn rerank_improves_over_adc() {
    // At a realistic nprobe, exact re-ranking of the ADC top-r must do at least as
    // well as the ADC scan it post-processes (and noticeably better here).
    let mut rng = Rng::new(17);
    let dim = 16usize;
    let corpus = blob_corpus(&mut rng, dim, 16, 100);
    let params = IvfpqParams::new(dim, 16, 8);
    let idx = open_index(&corpus, &params);
    let rerank = open_rerank(&corpus, dim);

    let (k, nprobe, r) = (10usize, 8usize, 50usize);
    let mut qrng = Rng::new(88);
    let (mut adc_recall, mut rer_recall) = (0.0f64, 0.0f64);
    let queries = 40usize;
    for _ in 0..queries {
        let base = &corpus[qrng.next_u64() as usize % corpus.len()].1;
        let q: Vec<f32> = base.iter().map(|&x| x + qrng.gauss() * 0.1).collect();
        let q = normalize(&q);
        let exact = exact_topk(&corpus, &q, k);
        let adc: Vec<u32> = block_on(idx.search(&q, k, nprobe))
            .unwrap()
            .into_iter()
            .map(|h| h.doc_id)
            .collect();
        let rer: Vec<u32> = block_on(idx.search_rerank(&q, k, nprobe, r, &rerank))
            .unwrap()
            .into_iter()
            .map(|h| h.doc_id)
            .collect();
        adc_recall += adc.iter().filter(|id| exact.contains(id)).count() as f64 / k as f64;
        rer_recall += rer.iter().filter(|id| exact.contains(id)).count() as f64 / k as f64;
    }
    adc_recall /= queries as f64;
    rer_recall /= queries as f64;
    assert!(
        rer_recall >= adc_recall,
        "rerank ({rer_recall:.3}) should be >= adc ({adc_recall:.3})"
    );
    assert!(rer_recall >= 0.9, "rerank recall {rer_recall:.3} too low");
}

#[test]
fn rrf_orders_by_fused_rank() {
    // Reciprocal-rank fusion of [1,2,3] and [3,2,4] (k=60): doc 3 is top of list B
    // and present in both, edging out doc 2 (mid in both); then 1, then 4.
    let a: &[u32] = &[1, 2, 3];
    let b: &[u32] = &[3, 2, 4];
    let fused = reciprocal_rank_fusion(&[a, b], 60.0);
    let order: Vec<u32> = fused.iter().map(|(id, _)| *id).collect();
    assert_eq!(order, vec![3, 2, 1, 4]);
    // scores are descending
    for w in fused.windows(2) {
        assert!(w[0].1 >= w[1].1);
    }
}

#[test]
fn edge_cases() {
    let mut rng = Rng::new(3);
    let corpus: Vec<(u32, Vec<f32>)> = (0..32)
        .map(|i| {
            (
                i,
                normalize(&(0..8).map(|_| rng.gauss()).collect::<Vec<_>>()),
            )
        })
        .collect();
    let params = IvfpqParams::new(8, 4, 4);
    let idx = open_index(&corpus, &params);
    let q = corpus[0].1.clone();

    // k == 0 or nprobe == 0 yields an empty result, not an error.
    assert!(block_on(idx.search(&q, 0, 4)).unwrap().is_empty());
    assert!(block_on(idx.search(&q, 5, 0)).unwrap().is_empty());

    // A wrong-dimensionality query is a BadQuery, not a panic.
    let bad = vec![0.0f32; 7];
    assert!(matches!(
        block_on(idx.search(&bad, 5, 4)),
        Err(roaringrange::IndexError::BadQuery(_))
    ));

    // nprobe is clamped to nlist, so an over-large nprobe still works.
    assert!(!block_on(idx.search(&q, 5, 999)).unwrap().is_empty());

    // Building from no vectors is a clean error.
    match build_ivfpq(&[], &params) {
        Err(e) => assert_eq!(e, VectorBuildError::Empty),
        Ok(_) => panic!("expected VectorBuildError::Empty"),
    }
}
