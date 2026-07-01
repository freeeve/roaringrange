//! Mutation fuzzing of the on-disk parsers from an attacker's perspective.
//!
//! Threat model: the reader parses index files fetched over HTTP byte-range from
//! a possibly-hostile origin, in a victim's browser. Safe Rust rules out memory
//! corruption, so the realistic attacker goals are denial of service — a panic
//! (the wasm reader traps and the page breaks), an out-of-bounds slice, an
//! unbounded allocation (OOM), or a runaway loop — triggered by crafted bytes.
//!
//! The [`RangeFetch`](crate::RangeFetch) layer is the first trust boundary: both
//! [`MemoryFetch`] and the browser `WasmFetch` return *exactly* `len` bytes or an
//! error, so a truncated/over-long response cannot feed a short buffer into a
//! parser. This harness targets the residual surface that guarantee does *not*
//! cover: the offset/length/count fields a parser reads out of a block it already
//! fetched successfully. Each test builds a valid file with the public
//! `build::write_*` writers, then feeds a deterministic family of corrupted and
//! truncated variants to `open()` plus a query method, asserting the reader
//! returns `Err` (or empty results) rather than panicking. Mutations are
//! reproducible (a fixed xorshift seed), so any failure names the exact variant.

use crate::build::{
    write_facets, write_index, write_lookup, write_records, write_sortcols, ColumnValues,
    FacetCategory, FacetField, SortColumn,
};
use crate::facet::FacetIndex;
use crate::index::Index;
use crate::lookup::Lookup;
use crate::ngram::ngram_keys;
use crate::records::RecordStore;
use crate::sortcols::SortCols;
use crate::MemoryFetch;
use futures::executor::block_on;
use roaring::RoaringBitmap;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// A 64K container-bucket boundary — lets a seed straddle the head/tail split.
const BUCKET: u32 = 65_536;

/// Deterministic xorshift64* — reproducible corruption with no `rand` dependency.
struct Rng(u64);

impl Rng {
    fn byte(&mut self) -> u8 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
    }
}

/// A bitmap from an explicit doc-ID list.
fn bm(docs: &[u32]) -> RoaringBitmap {
    docs.iter().copied().collect()
}

/// Serializes `bm` as one portable-roaring posting.
fn ser(b: &RoaringBitmap) -> Vec<u8> {
    let mut v = Vec::new();
    b.serialize_into(&mut v).unwrap();
    v
}

/// Splits `bm` into serialized head (`[0, 65536)`) and tail (`[65536, ∞)`) postings,
/// the layout the `RRSF` facet sidecar keeps per category.
fn split_head_tail(b: &RoaringBitmap) -> (Vec<u8>, Vec<u8>) {
    let head: RoaringBitmap = b.iter().filter(|&v| v < BUCKET).collect();
    let tail: RoaringBitmap = b.iter().filter(|&v| v >= BUCKET).collect();
    (ser(&head), ser(&tail))
}

/// A deterministic family of corrupted variants of `seed`:
///  - every short prefix and a sampling of longer truncations,
///  - the full seed (control) and one with trailing garbage,
///  - single-byte writes (`0x00`, `0xFF`, pseudo-random) at sampled positions,
///  - eight-byte `0xFF` windows that inflate a count/offset/length field to its max,
///  - `u32`/`u64` boundary values (`0`, `1`, the file length and its neighbours, and
///    the all-ones max) written at every position — these hit count/offset/length
///    fields precisely, exercising off-by-one bounds checks a random flip misses.
fn mutants(seed: &[u8]) -> Vec<Vec<u8>> {
    let n = seed.len();
    let step = (n / 256).max(1);
    let mut out: Vec<Vec<u8>> = Vec::new();

    for len in (0..=n).filter(|&l| l <= 48 || l % step == 0) {
        out.push(seed[..len].to_vec());
    }
    let mut longer = seed.to_vec();
    longer.extend(std::iter::repeat_n(0xAB, 32));
    out.push(longer);

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for pos in (0..n).step_by(step) {
        for v in [0x00u8, 0xFF, rng.byte()] {
            let mut m = seed.to_vec();
            m[pos] = v;
            out.push(m);
        }
    }
    for pos in (0..n.saturating_sub(8)).step_by(step) {
        let mut m = seed.to_vec();
        m[pos..pos + 8].fill(0xFF);
        out.push(m);
    }

    let u32s: [u32; 6] = [
        0,
        1,
        n as u32,
        (n as u32).wrapping_add(1),
        (n as u32).wrapping_sub(1),
        u32::MAX,
    ];
    for pos in (0..n.saturating_sub(4)).step_by(step) {
        for v in u32s {
            let mut m = seed.to_vec();
            m[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
            out.push(m);
        }
    }
    let u64s: [u64; 6] = [0, 1, n as u64, n as u64 + 1, u32::MAX as u64, u64::MAX];
    for pos in (0..n.saturating_sub(8)).step_by(step) {
        for v in u64s {
            let mut m = seed.to_vec();
            m[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
            out.push(m);
        }
    }
    out
}

/// Runs every mutant of `seed` through `exercise` (which opens the index and hits
/// a query path), asserting none panics. A panic — not an `Err` — is the bug we
/// hunt: it traps the wasm reader. The failing mutant's index and length are
/// reported so it can be reproduced from the fixed `Rng` seed.
fn assert_no_panic(label: &str, seed: Vec<u8>, exercise: impl Fn(MemoryFetch)) {
    let mut panicked: Vec<(usize, usize)> = Vec::new();
    for (i, m) in mutants(&seed).into_iter().enumerate() {
        let len = m.len();
        if catch_unwind(AssertUnwindSafe(|| exercise(MemoryFetch::new(m)))).is_err() {
            panicked.push((i, len));
        }
    }
    assert!(
        panicked.is_empty(),
        "{label}: {} mutant(s) panicked (index, len): {:?}",
        panicked.len(),
        &panicked[..panicked.len().min(8)]
    );
}

/// Two-file variant for parsers that read from a pair of fetchers (the record
/// store's offset index + record blob). Corrupts each buffer in turn while the
/// other stays valid, so a bad offset in `a` is exercised against a sound `b`
/// and vice versa. The `'a'`/`'b'` tag in a failure names which buffer broke it.
fn assert_no_panic_2(
    label: &str,
    a_seed: Vec<u8>,
    b_seed: Vec<u8>,
    exercise: impl Fn(MemoryFetch, MemoryFetch),
) {
    let mut panicked: Vec<(char, usize, usize)> = Vec::new();
    for (i, m) in mutants(&a_seed).into_iter().enumerate() {
        let len = m.len();
        let b = b_seed.clone();
        if catch_unwind(AssertUnwindSafe(|| {
            exercise(MemoryFetch::new(m), MemoryFetch::new(b))
        }))
        .is_err()
        {
            panicked.push(('a', i, len));
        }
    }
    for (i, m) in mutants(&b_seed).into_iter().enumerate() {
        let len = m.len();
        let a = a_seed.clone();
        if catch_unwind(AssertUnwindSafe(|| {
            exercise(MemoryFetch::new(a), MemoryFetch::new(m))
        }))
        .is_err()
        {
            panicked.push(('b', i, len));
        }
    }
    assert!(
        panicked.is_empty(),
        "{label}: {} mutant(s) panicked (buffer, index, len): {:?}",
        panicked.len(),
        &panicked[..panicked.len().min(8)]
    );
}

#[test]
fn fuzz_rrs_index_no_panic() {
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    let entries = vec![
        (abc, ser(&bm(&[1, 3, 5, 70_000]))),
        (bcd, ser(&bm(&[3, 5, 9, 131_000]))),
    ];
    let mut seed = Vec::new();
    write_index(&mut seed, 3, 2, entries).unwrap();

    assert_no_panic("RRS", seed, |f| {
        if let Ok(idx) = block_on(Index::open(f)) {
            let _ = block_on(idx.search("abc", 10));
            let _ = block_on(idx.search("abcd", 10));
            let _ = block_on(idx.posting(abc));
        }
    });
}

#[test]
fn fuzz_rrsf_facets_no_panic() {
    let (h1, t1) = split_head_tail(&bm(&[1, 2, 70_000]));
    let (h2, t2) = split_head_tail(&bm(&[2, 3, 131_000]));
    let fields = vec![FacetField {
        name: "lang".into(),
        cats: vec![
            FacetCategory {
                name: "en".into(),
                card: 3,
                head: h1,
                tail: t1,
            },
            FacetCategory {
                name: "fr".into(),
                card: 3,
                head: h2,
                tail: t2,
            },
        ],
    }];
    let mut seed = Vec::new();
    write_facets(&mut seed, fields).unwrap();

    let result = bm(&[1, 2, 3, 70_000, 131_000]);
    assert_no_panic("RRSF", seed, move |f| {
        if let Ok(idx) = block_on(FacetIndex::open(f)) {
            let _ = idx.counts(&result);
            let _ = block_on(idx.counts_full(&result, 0));
            let _ = block_on(idx.counts_full(&result, 1));
        }
    });
}

#[test]
fn fuzz_rril_lookup_no_panic() {
    let entries = vec![
        ("W1000001".to_string(), 1u32),
        ("W1000002".to_string(), 2u32),
        ("W1000003".to_string(), 3u32),
    ];
    let mut seed = Vec::new();
    write_lookup(&mut seed, &entries).unwrap();

    assert_no_panic("RRIL", seed, |f| {
        if let Ok(idx) = block_on(Lookup::open(f)) {
            let _ = block_on(idx.lookup("W1000002"));
            let _ = block_on(idx.lookup("absent"));
        }
    });
}

/// A crafted `RRVI` header whose centroid region (`nlist * dim * 4`) overflows
/// `usize` must be rejected as malformed — not panic on multiply-with-overflow
/// (debug) nor wrap to a short boot fetch whose `read_f32_vec` slices out of
/// bounds (release wasm). Regression for the checked boot-size arithmetic in
/// [`VectorIndex::open`](crate::vector::VectorIndex::open).
#[cfg(feature = "vector")]
#[test]
fn fuzz_rrvi_boot_size_overflow_rejected() {
    use crate::vector::VectorIndex;
    let mut h = vec![0u8; 48];
    h[0..4].copy_from_slice(b"RRVI");
    h[4..6].copy_from_slice(&1u16.to_le_bytes()); // version
    h[6] = 0; // metric = InnerProduct
    h[7] = 0; // flags (no OPQ)
    h[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dim
    h[12..16].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // nlist
    h[16..20].copy_from_slice(&3u32.to_le_bytes()); // m (0xFFFFFFFF is divisible by 3)
    h[20] = 8; // nbits -> ksub 256

    let got = block_on(VectorIndex::open(MemoryFetch::new(h)));
    assert!(
        matches!(got, Err(crate::index::IndexError::Malformed(_))),
        "expected Malformed on overflowing RRVI boot size"
    );
}

#[cfg(feature = "vector")]
#[test]
fn fuzz_rrvi_search_no_panic() {
    use crate::vector::VectorIndex;
    use crate::vector_build::{build_ivfpq, IvfpqParams};

    // A tiny trained index; corrupting the boot region hits the cluster directory's
    // counts/offsets, so `search` drives each per-cluster code-list read through the
    // checked block-size path (a wrapping `count*(4+m)` on wasm32, or a short read
    // here, must reject rather than decode garbage).
    let mut s = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0
    };
    let corpus: Vec<(u32, Vec<f32>)> = (0..64)
        .map(|i| (i, (0..8).map(|_| next()).collect()))
        .collect();
    let seed = build_ivfpq(&corpus, &IvfpqParams::new(8, 4, 4))
        .expect("build_ivfpq")
        .to_bytes();

    let query: Vec<f32> = vec![0.1, -0.2, 0.3, 0.4, -0.5, 0.6, -0.7, 0.8];
    assert_no_panic("RRVI-search", seed, move |f| {
        if let Ok(idx) = block_on(VectorIndex::open(f)) {
            let _ = block_on(idx.search(&query, 10, idx.nlist()));
        }
    });
}

#[cfg(feature = "splits")]
#[test]
fn bloom_contains_rejects_out_of_range_k() {
    // A per-split Bloom summary claiming `k = u32::MAX` must not spin billions of
    // hash rounds (a reader-hang DoS): a bad header conservatively returns "possibly
    // present" and returns at once. `k = 0` is likewise invalid.
    let mut bloom = Vec::new();
    bloom.extend_from_slice(&u32::MAX.to_le_bytes()); // k
    bloom.extend_from_slice(&64u32.to_le_bytes()); // nbits
    bloom.extend_from_slice(&[0u8; 8]); // bits
    assert!(crate::splitset::bloom_contains(&bloom, 0xdead_beef));
    bloom[0..4].copy_from_slice(&0u32.to_le_bytes());
    assert!(crate::splitset::bloom_contains(&bloom, 0xdead_beef));
}

#[cfg(feature = "terms")]
#[test]
fn rrsb_open_rejects_nonfinite_scale() {
    use crate::bm25::{ImpactIndex, HEADER_SIZE, MAGIC, VERSION};
    // A finite, positive scale is required: NaN (every comparison false) would slip
    // past a `scale <= 0.0` bound and make every score NaN, sorting arbitrarily.
    for bits in [f32::NAN.to_bits(), f32::INFINITY.to_bits(), 0u32] {
        let mut h = vec![0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(MAGIC);
        h[4..6].copy_from_slice(&VERSION.to_le_bytes());
        h[8..12].copy_from_slice(&bits.to_le_bytes()); // scale
        h[28..32].copy_from_slice(&1u32.to_le_bytes()); // stride
        let got = block_on(ImpactIndex::open(MemoryFetch::new(h)));
        assert!(
            matches!(got, Err(crate::index::IndexError::Malformed(_))),
            "expected Malformed for scale bits {bits:#x}"
        );
    }
}

#[cfg(feature = "terms")]
#[test]
fn rrti_open_rejects_implausible_router_len() {
    use crate::terms::TermIndex;
    // A header claiming a terabyte router must be rejected before the fetch, not
    // drive a resident multi-GB allocation. Header layout: magic, version(2), then
    // routerLen at offset 16.
    let mut h = vec![0u8; 40];
    h[0..4].copy_from_slice(b"RRTI");
    h[4..6].copy_from_slice(&2u16.to_le_bytes()); // version
    h[16..24].copy_from_slice(&(1u64 << 40).to_le_bytes()); // routerLen = 1 TiB
    let got = block_on(TermIndex::open(MemoryFetch::new(h)));
    assert!(
        matches!(got, Err(crate::index::IndexError::Malformed(_))),
        "expected Malformed for an implausible router length"
    );
}

#[test]
fn fuzz_rrsc_sortcols_no_panic() {
    let cols = vec![
        SortColumn {
            name: "year".into(),
            values: ColumnValues::U32(vec![2020, 2021, 2022, 2023]),
        },
        SortColumn {
            name: "score".into(),
            values: ColumnValues::F32(vec![1.0, 2.5, 3.0, 4.0]),
        },
    ];
    let mut seed = Vec::new();
    write_sortcols(&mut seed, cols).unwrap();

    assert_no_panic("RRSC", seed, |f| {
        if let Ok(idx) = block_on(SortCols::open(f)) {
            let _ = block_on(idx.value(0, 0));
            let _ = block_on(idx.values(0, &[0, 1, 2, 3, 9_999]));
            let _ = block_on(idx.values(99, &[0]));
            // The u32 column's gather/slice paths read `data_off`-relative bytes; a
            // corrupted `data_off` must saturate and length-check, not read wrong
            // bytes or panic the direct-indexing decode.
            let _ = block_on(idx.values_u32(0, &[0, 1, 2, 3]));
            let _ = block_on(idx.slice_u32(0, 0, 8));
            let _ = block_on(idx.slice_u32(0, 2, 100));
        }
    });
}

#[test]
fn fuzz_rrsr_records_no_panic() {
    // A version-2 store (framed records); one empty and one large record so the
    // offset-pair index and the decode path both have non-trivial spans to corrupt.
    let recs: Vec<Vec<u8>> = vec![
        b"first record".to_vec(),
        Vec::new(),
        b"third".to_vec(),
        vec![0xCD; 300],
    ];
    let mut bin = Vec::new();
    let mut idx = Vec::new();
    write_records(&mut bin, &mut idx, &recs).unwrap();

    // `RecordStore::open(idx, bin)` — corrupt the offset index against a sound blob
    // and the blob against a sound index; `get` exercises both the index read and
    // the record-slice read.
    assert_no_panic_2("RRSR", idx, bin, |idx_f, bin_f| {
        if let Ok(store) = block_on(RecordStore::open(idx_f, bin_f)) {
            for id in 0..6 {
                let _ = block_on(store.get(id));
            }
        }
    });
}

#[cfg(feature = "terms")]
#[test]
fn fuzz_rrti_terms_no_panic() {
    use crate::terms::TermIndex;
    use crate::terms_build::write_term_index;

    let docs: &[(u32, &str)] = &[
        (0, "hello world example"),
        (1, "world peace and foo"),
        (2, "bar baz hello again"),
    ];
    let mut seed = Vec::new();
    write_term_index(&mut seed, docs, 1).unwrap();

    assert_no_panic("RRTI", seed, |f| {
        if let Ok(idx) = block_on(TermIndex::open(f)) {
            let _ = block_on(idx.search("world", 10));
            let _ = block_on(idx.search("hello", 10));
            let _ = block_on(idx.search_prefix("ba", 10));
        }
    });
}

#[cfg(feature = "terms")]
#[test]
fn fuzz_rrsb_impacts_no_panic() {
    use crate::bm25::{write_impacts, ImpactIndex, ImpactsAccumulator, DEFAULT_B, DEFAULT_K1};
    use crate::terms::{TermIndex, Tokenizer};
    use crate::terms_build::write_term_index;

    let docs: &[(u32, &str)] = &[
        (0, "hello world example"),
        (1, "world peace and foo"),
        (2, "bar baz hello again"),
    ];
    let mut rrt = Vec::new();
    write_term_index(&mut rrt, docs, 1).unwrap();
    let terms = block_on(TermIndex::open(MemoryFetch::new(rrt))).unwrap();
    let dict = block_on(terms.dict_terms()).unwrap();

    let mut acc = ImpactsAccumulator::new(Tokenizer::plain());
    for &(_, d) in docs {
        acc.add_doc(d);
    }
    let mut seed = Vec::new();
    write_impacts(&mut seed, &dict, &acc, DEFAULT_K1, DEFAULT_B).unwrap();

    // The head offsets come from the valid dictionary; `rerank` reads each one's
    // impact entry out of the (mutated) sidecar, so a corrupted entry table or a
    // bad sparse index is driven through the real query path, not just `open`.
    let postings: Vec<(u64, RoaringBitmap)> =
        dict.iter().map(|(_, off)| (*off, bm(&[0, 1, 2]))).collect();
    assert_no_panic("RRSB", seed, move |f| {
        if let Ok(idx) = block_on(ImpactIndex::open(f)) {
            let _ = block_on(idx.rerank(&postings, &[0, 1, 2], 10));
        }
    });
}

/// Variant for parsers that take the whole buffer synchronously (not a fetcher),
/// e.g. the model2vec embedder. Same contract: every mutant must yield `Err`, not
/// a panic.
#[cfg(feature = "vector")]
fn assert_no_panic_bytes(label: &str, seed: Vec<u8>, exercise: impl Fn(&[u8])) {
    let mut panicked: Vec<(usize, usize)> = Vec::new();
    for (i, m) in mutants(&seed).into_iter().enumerate() {
        let len = m.len();
        if catch_unwind(AssertUnwindSafe(|| exercise(&m))).is_err() {
            panicked.push((i, len));
        }
    }
    assert!(
        panicked.is_empty(),
        "{label}: {} mutant(s) panicked (index, len): {:?}",
        panicked.len(),
        &panicked[..panicked.len().min(8)]
    );
}

#[cfg(feature = "vector")]
#[test]
fn fuzz_rrm2_model2vec_no_panic() {
    use crate::model2vec::Model2vec;

    // A tiny valid RRM2 (dim 2, 5 tokens, int8 quant), mirroring the reader fixture.
    let toks = ["[UNK]", "hello", "world", "!", "##ing"];
    let codes: [[i8; 2]; 5] = [[10, 10], [127, 0], [0, 127], [40, 40], [-127, 0]];
    let mut seed = Vec::new();
    seed.extend_from_slice(b"RRM2");
    seed.extend_from_slice(&1u16.to_le_bytes()); // version
    seed.extend_from_slice(&2u32.to_le_bytes()); // dim
    seed.extend_from_slice(&(toks.len() as u32).to_le_bytes()); // token count
    seed.push(0); // quant: int8
    seed.push(1); // flags: lowercase
    seed.extend_from_slice(&0u32.to_le_bytes()); // unk_id
    seed.resize(32, 0); // HEADER_SIZE
    for _ in 0..toks.len() {
        seed.extend_from_slice(&1.0f32.to_le_bytes()); // per-token scales
    }
    for row in &codes {
        seed.extend_from_slice(&[row[0] as u8, row[1] as u8]); // int8 rows
    }
    for t in toks {
        seed.extend_from_slice(&(t.len() as u16).to_le_bytes());
        seed.extend_from_slice(t.as_bytes());
    }

    assert_no_panic_bytes("RRM2", seed, |b| {
        if let Ok(model) = Model2vec::from_bytes(b) {
            let _ = model.embed("hello world ##ing !");
            let _ = model.dim();
        }
    });
}

#[cfg(feature = "vector")]
#[test]
fn fuzz_rrvr_rerank_no_panic() {
    use crate::vector::RerankStore;
    use crate::vector_build::write_rerank;

    let vectors: Vec<Vec<f32>> = vec![
        vec![1.0, 0.0, 0.0, 0.5],
        vec![0.0, 1.0, 0.5, 0.0],
        vec![0.3, 0.3, 0.3, 0.3],
    ];
    let mut seed = Vec::new();
    write_rerank(&mut seed, 4, &vectors, false).unwrap();

    assert_no_panic("RRVR", seed, |f| {
        if let Ok(store) = block_on(RerankStore::open(f)) {
            let _ = block_on(store.get_many(&[0, 1, 2, 9_999]));
        }
    });
}

#[cfg(feature = "hotcache")]
#[test]
fn fuzz_rrhc_hotcache_no_panic() {
    use crate::hotcache::{Hotcache, MemberTag};
    use crate::hotcache_build::{write_hotcache, MemberSpec};

    let boot = vec![0xABu8; 24];
    let members = vec![
        MemberSpec {
            tag: MemberTag::Rrs,
            data_file: "trigram.rrs".into(),
            boot_off: 0,
            boot_len: boot.len() as u32,
            boot_bytes: boot.clone(),
        },
        MemberSpec {
            tag: MemberTag::Rrsf,
            data_file: "facets.rrf".into(),
            boot_off: 0,
            boot_len: boot.len() as u32,
            boot_bytes: boot,
        },
    ];
    let mut seed = Vec::new();
    write_hotcache(&mut seed, &members, 1024).unwrap(); // threshold high -> members inlined

    assert_no_panic("RRHC", seed, |f| {
        if let Ok(hc) = block_on(Hotcache::open(f)) {
            let _ = hc.members();
        }
    });
}

#[cfg(feature = "splits")]
#[test]
fn fuzz_rrss_manifest_no_panic() {
    use crate::splitset::{BodyKind, Policy, SplitSet};
    use crate::splitset_build::{write_splitset, SplitSetConfig, SplitSpec};

    let splits = vec![
        SplitSpec {
            data_file: "split0.rrs".into(),
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
            data_file: "split1.rrs".into(),
            tier: 1,
            doc_count: 50,
            doc_id_lo: 100,
            doc_id_hi: 149,
            epoch: 0,
            byte_size: 2048,
            flags: 0,
            summary: Vec::new(),
        },
    ];
    let config = SplitSetConfig {
        policy: Policy::Tiered,
        tier_count: 2,
        base_count: 2,
        byte_cap: 1 << 20,
        gram_size: 3,
        body_kind: BodyKind::Trigram,
        sortcol: None,
        flags: 0,
    };
    let mut seed = Vec::new();
    write_splitset(&mut seed, &splits, &config).unwrap();

    // The manifest is fully resident after open — split entries, string blob, and
    // summary blob are all parsed there, so `open` is the whole attack surface.
    assert_no_panic("RRSS", seed, |f| {
        let _ = block_on(SplitSet::open(f));
    });
}

#[cfg(feature = "splits")]
#[test]
fn fuzz_rrss_split_search_no_panic() {
    use crate::splitset::{Policy, SplitFetcher, SplitSet};
    use crate::splitset_build::{SplitBuildConfig, SplitSetBuilder};
    use std::collections::HashMap;

    /// Resolves a split/facet data-file name to its (possibly corrupted) bytes.
    struct MapResolver(HashMap<String, Vec<u8>>);
    impl SplitFetcher for MapResolver {
        type Fetch = MemoryFetch;
        fn fetch_named(&self, name: &str) -> MemoryFetch {
            MemoryFetch::new(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    // A faceted corpus with a tiny byte cap so it seals into several splits — the
    // manifest stays valid while each split/facet body is corrupted below.
    let docs: &[(&str, &[(&str, &str)])] = &[
        (
            "the quick brown fox jumps",
            &[("color", "brown"), ("animal", "fox")],
        ),
        (
            "quick brown dogs run fast",
            &[("color", "brown"), ("animal", "dog")],
        ),
        (
            "lazy red fox sleeps now",
            &[("color", "red"), ("animal", "fox")],
        ),
        (
            "red dogs and brown cats",
            &[("color", "red"), ("animal", "dog")],
        ),
        (
            "brown cats jump very high",
            &[("color", "brown"), ("animal", "cat")],
        ),
        (
            "green frogs and red fish",
            &[("color", "green"), ("animal", "frog")],
        ),
    ];
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap: 1000,
        byte_cap_max: 0,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "corpus".into(),
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
    let built = b.finish().unwrap();
    let ss = block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap();

    let mut base: HashMap<String, Vec<u8>> = HashMap::new();
    for (name, bytes) in built.splits.iter().chain(built.facets.iter()) {
        base.insert(name.clone(), bytes.clone());
    }
    // Sorted for a deterministic, reproducible iteration order over the file set.
    let mut names: Vec<String> = base.keys().cloned().collect();
    names.sort();

    // Corrupt each split/facet body in turn (others held valid) and drive the full
    // query path: bloom prune, per-split open, cross-split intersect, facet aggregate.
    // Each split is opened+searched (heavier than a single parse), so subsample the
    // mutant set per file: `step_by` keeps the spread of byte flips and boundary
    // values while holding this multi-split test to a few seconds.
    let mut panicked: Vec<(String, usize, usize)> = Vec::new();
    for name in &names {
        for (mi, m) in mutants(&base[name]).into_iter().enumerate().step_by(3) {
            let len = m.len();
            let mut files = base.clone();
            files.insert(name.clone(), m);
            let resolver = MapResolver(files);
            let ssr = &ss;
            if catch_unwind(AssertUnwindSafe(|| {
                let _ = block_on(ssr.search(&resolver, "brown", 10));
                let _ = block_on(ssr.search_filtered(
                    &resolver,
                    "brown",
                    &[("color".into(), "red".into())],
                    10,
                ));
                let _ = block_on(ssr.facet_counts(&resolver, &[0, 1, 2, 3, 4, 5]));
            }))
            .is_err()
            {
                panicked.push((name.clone(), mi, len));
            }
        }
    }
    assert!(
        panicked.is_empty(),
        "RRSS split search: {} mutant(s) panicked (file, index, len): {:?}",
        panicked.len(),
        &panicked[..panicked.len().min(8)]
    );
}

/// A corpus of adversarial query strings — the *caller*-controlled input surface
/// (a malicious URL can pre-fill `?q=`). Targets the slice-on-non-char-boundary
/// panic class plus tokenizer/embedder edge cases: empty/whitespace-only, sub-gram
/// lengths, combining marks, ZWJ/flag emoji, astral-plane (4-byte) chars, control
/// and zero-width characters, bidi overrides, mixed scripts, and large inputs.
fn pathological_queries() -> Vec<String> {
    let mut q: Vec<String> = [
        "",
        " ",
        "\t\n\r ",
        "a",
        "ab",
        "abc",
        "é",
        "日",
        "café",
        "naïve résumé",
        "Ω≈ç√∫µ",
        "日本語のテキスト",
        "한국어 텍스트입니다",
        "العربية مرحبا بالعالم",
        "עברית שלום",
        "Ελληνικά κείμενο",
        "e\u{0301}",                          // e + combining acute
        "a\u{0300}\u{0301}\u{0302}\u{0303}b", // stacked combining marks
        "👨‍👩‍👧‍👦",                                 // ZWJ family
        "🏳️‍🌈 flag",
        "💥🔥✨🎉",
        "🇺🇸🇬🇧🇯🇵",          // regional-indicator flags
        "𝕊𝕥𝕣𝕒𝕟𝕘𝕖 𝓉𝑒𝓍𝓉",    // astral-plane letters
        "𠀀𠀁𠀂𠀃",        // astral CJK ext-B
        "\u{0}\u{0}\u{0}", // nulls
        "a\u{0}b\u{0}c",
        "\u{1}\u{2}\u{1F}",                      // control chars
        "a日b語c한d文e",                         // 1-byte/3-byte interleave
        "abc\u{202E}def\u{202D}ghi",             // bidi overrides (RLO/LRO)
        "a\u{200B}b\u{200C}c\u{200D}d\u{FEFF}e", // zero-width + BOM
        "a\u{00A0}b\u{3000}c\u{2028}d\u{2029}e", // exotic whitespace separators
        "!!!",
        "@#$%^&*()_+",
        "12345",
        "①②③Ⅰ②", // letter/other numbers (filtered)
        "the quick brown fox jumps",
        "aAaAaA",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Large inputs: a single repeated char (dedups to one gram), a repeated 4-byte
    // char, many distinct CJK grams, and many sub-gram words.
    q.push("a".repeat(100_000));
    q.push("💥".repeat(20_000));
    q.push(
        (0..20_000u32)
            .map(|i| char::from_u32(0x4E00 + (i % 0x3000)).unwrap_or('x'))
            .collect(),
    );
    q.push("ab ".repeat(30_000));
    q
}

/// Runs every pathological query through `exercise`, asserting none panics.
fn assert_queries_no_panic(label: &str, exercise: impl Fn(&str)) {
    let mut panicked: Vec<String> = Vec::new();
    for q in pathological_queries() {
        if catch_unwind(AssertUnwindSafe(|| exercise(&q))).is_err() {
            let head: String = q.chars().take(24).collect();
            panicked.push(format!("{head:?}(len {})", q.len()));
        }
    }
    assert!(
        panicked.is_empty(),
        "{label}: {} query/queries panicked: {:?}",
        panicked.len(),
        &panicked[..panicked.len().min(8)]
    );
}

#[test]
fn fuzz_query_strings_ngram_and_rrs() {
    let abc = ngram_keys("abc", 3)[0];
    let entries = vec![(abc, ser(&bm(&[1, 2, 3])))];
    let mut buf = Vec::new();
    write_index(&mut buf, 3, 2, entries).unwrap();
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    assert_queries_no_panic("query/ngram+rrs", |q| {
        let _ = ngram_keys(q, 2);
        let _ = ngram_keys(q, 3);
        let _ = ngram_keys(q, 8);
        let _ = block_on(idx.search(q, 10));
    });
}

#[cfg(feature = "terms")]
#[test]
fn fuzz_query_strings_terms() {
    use crate::terms::{TermIndex, Tokenizer};
    use crate::terms_build::write_term_index;

    let docs: &[(u32, &str)] = &[(0, "hello world café"), (1, "日本語 text here")];
    let mut buf = Vec::new();
    write_term_index(&mut buf, docs, 1).unwrap();
    let ti = block_on(TermIndex::open(MemoryFetch::new(buf))).unwrap();
    let tok = Tokenizer::plain();

    assert_queries_no_panic("query/terms", |q| {
        let _ = tok.tokenize(q);
        let _ = block_on(ti.search(q, 10));
        let _ = block_on(ti.search_prefix(q, 10));
    });
}

#[cfg(feature = "vector")]
#[test]
fn fuzz_query_strings_model2vec() {
    use crate::model2vec::Model2vec;

    // Minimal valid RRM2 (dim 2, 5 tokens) — same fixture as the parser fuzz.
    let toks = ["[UNK]", "hello", "world", "!", "##ing"];
    let codes: [[i8; 2]; 5] = [[10, 10], [127, 0], [0, 127], [40, 40], [-127, 0]];
    let mut seed = Vec::new();
    seed.extend_from_slice(b"RRM2");
    seed.extend_from_slice(&1u16.to_le_bytes());
    seed.extend_from_slice(&2u32.to_le_bytes());
    seed.extend_from_slice(&(toks.len() as u32).to_le_bytes());
    seed.push(0);
    seed.push(1);
    seed.extend_from_slice(&0u32.to_le_bytes());
    seed.resize(32, 0);
    for _ in 0..toks.len() {
        seed.extend_from_slice(&1.0f32.to_le_bytes());
    }
    for row in &codes {
        seed.extend_from_slice(&[row[0] as u8, row[1] as u8]);
    }
    for t in toks {
        seed.extend_from_slice(&(t.len() as u16).to_le_bytes());
        seed.extend_from_slice(t.as_bytes());
    }
    let model = Model2vec::from_bytes(&seed).unwrap();

    assert_queries_no_panic("query/model2vec", |q| {
        let _ = model.embed(q);
    });
}
