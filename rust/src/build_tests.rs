//! End-to-end tests that build a synthetic `RRS` buffer in-memory exactly per
//! `FORMAT.md`, back it with [`MemoryFetch`], and assert the reader's lookups
//! and search results.

use crate::facet::FacetIndex;
use crate::index::Index;
use crate::ngram::ngram_keys;
use crate::MemoryFetch;
use futures::executor::block_on;
use roaring::RoaringBitmap;

/// A 64K container-bucket boundary — used to craft test docs that span buckets.
const BUCKET: u32 = 65_536;

/// One dictionary entry to be laid out: a key and its serialized posting.
struct Posting {
    key: u64,
    posting: Vec<u8>,
}

/// Serializes `bm` as one v3 portable-roaring posting (no head/tail split).
fn serialize_posting(bm: &RoaringBitmap) -> Vec<u8> {
    let mut v = Vec::new();
    bm.serialize_into(&mut v).unwrap();
    v
}

/// Splits `bm` into serialized head (`[0, 65536)`) and tail (`[65536, ∞)`) postings — the
/// `RRSF` facet sidecar keeps the v2 head/tail layout (only the `RRS` index collapsed in v3).
fn split_head_tail(bm: &RoaringBitmap) -> (Vec<u8>, Vec<u8>) {
    let (mut head, mut tail) = (RoaringBitmap::new(), RoaringBitmap::new());
    for v in bm.iter() {
        if v < BUCKET {
            head.insert(v);
        } else {
            tail.insert(v);
        }
    }
    let mut hb = Vec::new();
    head.serialize_into(&mut hb).unwrap();
    let mut tb = Vec::new();
    tail.serialize_into(&mut tb).unwrap();
    (hb, tb)
}

/// Builds a complete v3 `RRS` byte buffer from `(key, bitmap)` pairs and the given
/// sparse stride — laid out by hand exactly per `FORMAT.md` to verify the reader against the
/// spec. Entries are sorted by key (the format requires a key-sorted dictionary).
fn build_rrs(gram_size: u16, stride: u32, entries: &[(u64, RoaringBitmap)]) -> Vec<u8> {
    let mut postings: Vec<Posting> = entries
        .iter()
        .map(|(key, bm)| Posting {
            key: *key,
            posting: serialize_posting(bm),
        })
        .collect();
    postings.sort_by_key(|p| p.key);

    let ngrams = postings.len() as u32;
    let sparse_count = if ngrams == 0 || stride == 0 {
        0
    } else {
        ngrams.div_ceil(stride) as usize
    };

    let mut out = Vec::new();
    // Header (16 B): magic, version=3, gram, ngrams, stride.
    out.extend_from_slice(b"RRSI");
    out.extend_from_slice(&3u16.to_le_bytes()); // version
    out.extend_from_slice(&gram_size.to_le_bytes());
    out.extend_from_slice(&ngrams.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());

    // Sparse index: dict[i*stride].key for i in 0..sparse_count.
    for i in 0..sparse_count {
        let key = postings[i * stride as usize].key;
        out.extend_from_slice(&key.to_le_bytes());
    }

    // Dictionary (20 B each): key + absolute posting offset + size.
    let dict_start = 16 + sparse_count * 8;
    let postings_start = dict_start + postings.len() * 20;
    let mut off = postings_start as u64;
    for p in &postings {
        let size = p.posting.len() as u32;
        out.extend_from_slice(&p.key.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        off += size as u64;
    }

    // Postings: one bitmap per entry, in dict order.
    for p in &postings {
        out.extend_from_slice(&p.posting);
    }

    out
}

/// Convenience: a bitmap from an explicit doc-ID list.
fn bm(docs: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &d in docs {
        b.insert(d);
    }
    b
}

#[test]
fn search_intersects_heads_ascending() {
    // Trigrams for "abc" -> "abc" only (key 6382179).
    // Trigrams for "abcd" -> "abc"(6382179), "bcd".
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    assert_eq!(abc, 6382179);

    // "abc" matches docs {1,3,5,7}; "bcd" matches {3,5,9}. AND -> {3,5}.
    let entries = vec![
        (abc, bm(&[1, 3, 5, 7])),
        (bcd, bm(&[3, 5, 9])),
        // a third key so the dictionary spans multiple sparse blocks at stride 2.
        (ngram_keys("xyz", 3)[0], bm(&[2, 4])),
    ];
    let buf = build_rrs(3, 2, &entries);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();
    assert_eq!(idx.gram_size(), 3);
    assert_eq!(idx.ngram_count(), 3);

    // Single trigram query returns docs ascending (= popularity rank).
    assert_eq!(block_on(idx.search("abc", 10)).unwrap(), vec![1, 3, 5, 7]);
    // Limit truncates to the top-K.
    assert_eq!(block_on(idx.search("abc", 2)).unwrap(), vec![1, 3]);
    // Two-trigram AND.
    assert_eq!(block_on(idx.search("abcd", 10)).unwrap(), vec![3, 5]);
    // A missing trigram yields no results.
    assert!(block_on(idx.search("abq", 10)).unwrap().is_empty());
}

#[test]
fn posting_spans_buckets_and_search_pages_in_order() {
    // Docs spanning several 64K buckets, including beyond the eager prefix (16 buckets = 1M).
    let abc = ngram_keys("abc", 3)[0];
    let beyond = 16 * BUCKET + 9; // past the eager prefix -> reached by tail paging
    let docs = [3u32, 5, BUCKET, BUCKET + 7, 100_000, beyond];
    let entries = vec![(abc, bm(&docs))];
    let buf = build_rrs(3, 2, &entries);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    // The single posting holds every doc, in ascending order.
    let posting = block_on(idx.posting(abc)).unwrap().unwrap();
    assert_eq!(posting.iter().collect::<Vec<_>>(), docs);

    // Search returns every match in ascending (rank) order, paging past the eager prefix.
    assert_eq!(block_on(idx.search("abc", 10)).unwrap(), docs);
    // A small limit is satisfied by the eager prefix alone (no deep paging).
    assert_eq!(block_on(idx.search("abc", 2)).unwrap(), vec![3, 5]);
}

#[test]
fn search_and_with_tail_intersection() {
    // Two keys whose intersection only appears in the tail (>=65536).
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    let big_a = BUCKET + 10;
    let big_b = BUCKET + 20;
    let entries = vec![(abc, bm(&[1, big_a, big_b])), (bcd, bm(&[2, big_a, big_b]))];
    let buf = build_rrs(3, 2, &entries);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    // Head AND of "abcd" -> {} (no shared head doc). Tail AND -> {big_a, big_b}.
    assert_eq!(
        block_on(idx.search("abcd", 10)).unwrap(),
        vec![big_a, big_b]
    );
}

#[test]
fn sparse_path_with_many_entries() {
    // Force several sparse blocks: 9 keys at stride 2 -> sparseCount = 5.
    let mut entries = Vec::new();
    // distinct trigram keys with controlled doc sets
    let keys: Vec<u64> = (0..9)
        .map(|i| {
            // build trigrams "a0a".."a8a" style; just use ngram of unique strings
            let s = format!("k{i}x");
            ngram_keys(&s, 3)[0]
        })
        .collect();
    for (i, k) in keys.iter().enumerate() {
        entries.push((*k, bm(&[i as u32, 100 + i as u32])));
    }
    let buf = build_rrs(3, 2, &entries);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();
    assert_eq!(idx.ngram_count(), 9);

    // Every key must resolve through the sparse + block binary search.
    for (i, k) in keys.iter().enumerate() {
        let posting = block_on(idx.posting(*k)).unwrap().unwrap();
        assert_eq!(
            posting.iter().collect::<Vec<_>>(),
            vec![i as u32, 100 + i as u32]
        );
    }
    // A key smaller than the first dictionary key is absent.
    let smallest = *keys.iter().min().unwrap();
    assert!(block_on(idx.posting(smallest - 1)).unwrap().is_none());
    // A key larger than all is absent.
    let largest = *keys.iter().max().unwrap();
    assert!(block_on(idx.posting(largest + 1)).unwrap().is_none());
}

/// Builds a synthetic RRS index over `texts` (doc id = slice index), deriving
/// trigrams with the same `ngram_keys` the reader uses — so a doc's index
/// membership matches re-deriving its text's trigrams (what verification does).
fn build_index_from_texts(texts: &[&str]) -> Vec<u8> {
    use std::collections::BTreeMap;
    let mut by_key: BTreeMap<u64, RoaringBitmap> = BTreeMap::new();
    for (doc, text) in texts.iter().enumerate() {
        for key in ngram_keys(text, 3) {
            by_key.entry(key).or_default().insert(doc as u32);
        }
    }
    let entries: Vec<(u64, RoaringBitmap)> = by_key.into_iter().collect();
    build_rrs(3, 2, &entries)
}

#[test]
fn candidates_then_verify_equals_full_and() {
    use std::collections::HashSet;
    // "abc" is common (6 docs); "bcd" is rare (4). doc2 has only bcd; doc4 has
    // both trigrams in separate tokens (still a true trigram-AND match).
    let texts = [
        "abcd",    // 0: abc + bcd
        "abc",     // 1: abc only
        "bcd",     // 2: bcd only
        "xabcd",   // 3: abc + bcd
        "abc bcd", // 4: abc + bcd (separate tokens)
        "abc",     // 5: abc only
        "abc",     // 6: abc only
    ];
    let idx = block_on(Index::open(MemoryFetch::new(build_index_from_texts(
        &texts,
    ))))
    .unwrap();

    let full_and = block_on(idx.search("abcd", 100)).unwrap();
    assert_eq!(full_and, vec![0, 3, 4]); // abc{0,1,3,4,5,6} ∩ bcd{0,2,3,4}

    // Seed from the single rarest trigram (bcd) -> a superset of the AND.
    let candidates = block_on(idx.search_candidates("abcd", 1)).unwrap();
    assert_eq!(candidates, vec![0, 2, 3, 4]); // = bcd's posting
    for &d in &full_and {
        assert!(
            candidates.contains(&d),
            "candidates must be a superset of the AND"
        );
    }

    // Verify each candidate against its text: keep those whose trigrams cover the
    // query. This is exactly what the browser does with the record's stored text.
    let qkeys: HashSet<u64> = ngram_keys("abcd", 3).into_iter().collect();
    let verified: Vec<u32> = candidates
        .into_iter()
        .filter(|&d| {
            let dkeys: HashSet<u64> = ngram_keys(texts[d as usize], 3).into_iter().collect();
            qkeys.is_subset(&dkeys)
        })
        .collect();
    assert_eq!(verified, full_and);

    // Seeding from all trigrams already yields the exact AND (verification is a no-op).
    assert_eq!(
        block_on(idx.search_candidates("abcd", 9)).unwrap(),
        full_and
    );
    // An absent trigram makes the strict AND impossible -> empty candidates.
    assert!(block_on(idx.search_candidates("abqz", 2))
        .unwrap()
        .is_empty());
}

#[test]
fn open_rejects_bad_magic() {
    let mut buf = build_rrs(3, 2, &[(6382179, bm(&[1]))]);
    buf[0] = b'X';
    assert!(block_on(Index::open(MemoryFetch::new(buf))).is_err());
}

#[test]
fn query_cost_prices_postings_from_the_dictionary() {
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    let abc_bm = bm(&[1, 3, 5, 7]);
    let bcd_bm = bm(&[3, 5, 9]);
    let abc_size = serialize_posting(&abc_bm).len() as u64;
    let bcd_size = serialize_posting(&bcd_bm).len() as u64;
    let buf = build_rrs(3, 2, &[(abc, abc_bm), (bcd, bcd_bm)]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    // Costs come straight from the dictionary's recorded posting sizes.
    assert_eq!(block_on(idx.query_cost("abc")).unwrap(), abc_size);
    assert_eq!(
        block_on(idx.query_cost("abcd")).unwrap(),
        abc_size + bcd_size
    );
    // An absent trigram short-circuits the strict-AND search to empty -> cost 0,
    // and an unkeyable query prices to 0 too.
    assert_eq!(block_on(idx.query_cost("abq")).unwrap(), 0);
    assert_eq!(block_on(idx.query_cost("")).unwrap(), 0);
}

#[test]
fn filter_cost_prices_categories_from_resident_meta() {
    let en = bm(&[1, 2, 3]);
    let es = bm(&[4, 5, BUCKET + 1]);
    let (en_h, en_t) = split_head_tail(&en);
    let (es_h, es_t) = split_head_tail(&es);
    let facets = block_on(FacetIndex::open(MemoryFetch::new(build_rrsf(&[(
        "language",
        vec![("en", en), ("es", es)],
    )]))))
    .unwrap();

    let pair = |f: &str, c: &str| (f.to_string(), c.to_string());
    assert_eq!(
        facets.filter_cost(&[pair("language", "en")]),
        (en_h.len() + en_t.len()) as u64
    );
    assert_eq!(
        facets.filter_cost(&[pair("language", "en"), pair("language", "es")]),
        (en_h.len() + en_t.len() + es_h.len() + es_t.len()) as u64
    );
    // Unknown selections fetch nothing, so they price to 0.
    assert_eq!(facets.filter_cost(&[pair("language", "fr")]), 0);
    assert_eq!(facets.filter_cost(&[pair("nope", "x")]), 0);
}

#[test]
fn membership_bitmap_matches_full_bitmap_via_container_seeks() {
    // Tail postings dense enough to exceed the subset reader's whole-read
    // threshold: several buckets each holding a >4096-card container (an ~8 KB
    // bitmap container apiece), so the offset-table seek path actually runs.
    let dense = |buckets: &[u32]| {
        let mut b = RoaringBitmap::new();
        for &bk in buckets {
            let base = bk * 65_536;
            b.insert_range(base..base + 5_000);
        }
        b
    };
    let en = dense(&[0, 1, 3, 7]); // head bucket + three tail buckets
    let es = dense(&[2, 3, 8]);
    let art = dense(&[1, 2, 3, 7, 8]);
    let facets = block_on(FacetIndex::open(MemoryFetch::new(build_rrsf(&[
        ("language", vec![("en", en), ("es", es)]),
        ("type", vec![("article", art)]),
    ]))))
    .unwrap();

    // Candidates scattered across the head bucket, several tail buckets (both in
    // and out of the categories' ranges), and a bucket no category touches.
    let cand: RoaringBitmap = [5u32, 70_000, 196_700, 230_000, 459_000, 525_000, 9_000_000]
        .into_iter()
        .collect();

    let pair = |f: &str, c: &str| (f.to_string(), c.to_string());
    let cases = vec![
        vec![pair("language", "en")],
        vec![pair("language", "en"), pair("language", "es")],
        vec![pair("language", "en"), pair("type", "article")],
        vec![pair("type", "article")],
        vec![pair("language", "nope")], // unresolvable -> matches nothing
    ];
    for pairs in cases {
        let filter = facets.resolve(&pairs);
        let full = block_on(filter.full_bitmap()).unwrap();
        let expect: RoaringBitmap = cand.iter().filter(|id| full.contains(*id)).collect();
        let got = block_on(filter.membership_bitmap(&cand)).unwrap();
        assert_eq!(got, expect, "pairs {pairs:?}");
    }
    assert!(
        block_on(
            facets
                .resolve(&[pair("language", "en")])
                .membership_bitmap(&RoaringBitmap::new())
        )
        .unwrap()
        .is_empty(),
        "empty candidates short-circuit"
    );
}

#[test]
fn count_estimate_from_headers_matches_search() {
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    // abc: a posting large enough (multi-bucket, >4 KB serialized) that the
    // count must come from the descriptive header, not a whole-posting read;
    // bcd: a small posting exercising the whole-read fallback.
    let mut abc_bm = RoaringBitmap::new();
    for bk in [0u32, 2, 5, 9] {
        abc_bm.insert_range(bk * 65_536..bk * 65_536 + 5_000);
    }
    let bcd_bm = bm(&[3, 5, 9, 70_000]);
    let buf = build_rrs(3, 2, &[(abc, abc_bm.clone()), (bcd, bcd_bm.clone())]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    // Per-key counts are exact.
    assert_eq!(
        block_on(idx.term_count(abc)).unwrap(),
        Some(abc_bm.len()),
        "header-derived cardinality"
    );
    assert_eq!(block_on(idx.term_count(bcd)).unwrap(), Some(bcd_bm.len()));
    assert_eq!(
        block_on(idx.term_count(ngram_keys("zzz", 3)[0])).unwrap(),
        None
    );

    // Single-key query: exact. Multi-key: an upper bound >= the true intersection.
    assert_eq!(
        block_on(idx.count_estimate("abc")).unwrap(),
        (abc_bm.len(), true)
    );
    let (bound, exact) = block_on(idx.count_estimate("abcd")).unwrap();
    assert!(!exact);
    let truth = block_on(idx.search("abcd", usize::MAX)).unwrap().len() as u64;
    assert!(
        bound >= truth,
        "bound {bound} must cover the true count {truth}"
    );
    assert_eq!(
        bound,
        bcd_bm.len(),
        "the smallest per-key count is the bound"
    );
    // An absent key makes the strict AND empty: exact zero.
    assert_eq!(block_on(idx.count_estimate("abq")).unwrap(), (0, true));
}

#[test]
fn filter_count_bound_uses_resident_counts() {
    let facets = block_on(FacetIndex::open(MemoryFetch::new(build_rrsf(&[
        (
            "format",
            vec![("ebook", bm(&[1, 3, 5])), ("audiobook", bm(&[2, 4]))],
        ),
        ("language", vec![("en", bm(&[1, 2]))]),
    ]))))
    .unwrap();
    let pair = |f: &str, c: &str| (f.to_string(), c.to_string());

    assert_eq!(facets.filter_count_bound(&[]), None);
    assert_eq!(
        facets.filter_count_bound(&[pair("format", "ebook")]),
        Some(3)
    );
    // Within a field, selections OR: the bound is the sum.
    assert_eq!(
        facets.filter_count_bound(&[pair("format", "ebook"), pair("format", "audiobook")]),
        Some(5)
    );
    // Across fields they AND: the bound is the min of the field sums.
    assert_eq!(
        facets.filter_count_bound(&[pair("format", "ebook"), pair("language", "en")]),
        Some(2)
    );
    // An unresolvable field bounds the filter at zero.
    assert_eq!(facets.filter_count_bound(&[pair("nope", "x")]), Some(0));
}

#[test]
fn sparse_result_paging_is_bounded_per_call_and_completes() {
    // One doc per bucket across 400 buckets — total matches (400) exceed nothing,
    // but they are scattered so far apart that filling any page must scan many
    // tail windows. A single page() call must do BOUNDED work (return partial
    // with the tail pending), and repeated calls / load_tail must converge on
    // exactly the full set.
    let abc = ngram_keys("abc", 3)[0];
    let mut b = RoaringBitmap::new();
    for bk in 1..=400u32 {
        b.insert(bk * BUCKET + 7);
    }
    let want: Vec<u32> = b.iter().collect();
    let buf = build_rrs(3, 2, &[(abc, b)]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    let mut cur = block_on(idx.search_cursor("abc", 0)).unwrap();
    // Ask for more than one budget's worth: the call returns what its window
    // budget reached, not the whole answer.
    let first = block_on(cur.page(0, 300)).unwrap();
    assert!(
        !first.is_empty() && first.len() < 300,
        "one call is budgeted: got {} of 300",
        first.len()
    );
    assert!(
        cur.pending_tail(),
        "tail must still be pending after one call"
    );

    // Repeated calls continue the scan and eventually fill the request.
    let mut page = first;
    let mut calls = 1;
    while page.len() < 300 && cur.pending_tail() {
        page = block_on(cur.page(0, 300)).unwrap();
        calls += 1;
        assert!(calls < 100, "scan must progress every call");
    }
    assert_eq!(page, want[..300].to_vec());

    // load_tail loops the budget to completion; the set is exact.
    block_on(cur.load_tail()).unwrap();
    assert!(!cur.pending_tail());
    assert_eq!(cur.loaded(), want.len());
    assert_eq!(block_on(cur.page(0, 500)).unwrap(), want);
}

#[test]
fn open_rejects_zero_stride_with_nonempty_dict() {
    // stride 0 with ngrams > 0 is corruption: sparse_count would silently be 0
    // and every query would return empty instead of surfacing an error.
    let mut buf = build_rrs(3, 2, &[(6382179, bm(&[1]))]);
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    assert!(block_on(Index::open(MemoryFetch::new(buf))).is_err());
}

#[test]
fn empty_query_returns_nothing() {
    let buf = build_rrs(3, 2, &[(6382179, bm(&[1, 2, 3]))]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();
    assert!(block_on(idx.search("ab", 10)).unwrap().is_empty()); // too short
    assert!(block_on(idx.search("abc", 0)).unwrap().is_empty()); // zero limit
}

#[test]
fn cursor_paginates_head_then_tail() {
    let abc = ngram_keys("abc", 3)[0];
    // Four head docs (<65536) then three tail docs (>=65536) for one trigram.
    let docs = [1u32, 3, 5, 7, BUCKET, BUCKET + 2, BUCKET + 4];
    let buf = build_rrs(3, 2, &[(abc, bm(&docs))]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    let mut cur = block_on(idx.search_cursor("abc", 0)).unwrap();
    // Pages drawn from the in-memory head set.
    assert_eq!(block_on(cur.next(2)).unwrap(), vec![1, 3]);
    assert_eq!(block_on(cur.next(2)).unwrap(), vec![5, 7]);
    // Crossing into the lazily-fetched tail, still globally ascending.
    assert_eq!(block_on(cur.next(2)).unwrap(), vec![BUCKET, BUCKET + 2]);
    assert_eq!(block_on(cur.next(10)).unwrap(), vec![BUCKET + 4]);
    assert!(block_on(cur.next(10)).unwrap().is_empty()); // exhausted

    // Random-access paging: forward, backward (free), and across the tail.
    let mut c2 = block_on(idx.search_cursor("abc", 0)).unwrap();
    assert_eq!(block_on(c2.page(0, 2)).unwrap(), vec![1, 3]);
    assert_eq!(block_on(c2.page(2, 2)).unwrap(), vec![5, 7]);
    assert_eq!(block_on(c2.page(0, 2)).unwrap(), vec![1, 3]); // backward, no fetch
    assert_eq!(block_on(c2.page(4, 2)).unwrap(), vec![BUCKET, BUCKET + 2]); // crosses into the tail
    assert_eq!(block_on(c2.page(1, 3)).unwrap(), vec![3, 5, 7]); // back again, all materialized

    // Concatenated pages equal a single full search.
    assert_eq!(block_on(idx.search("abc", 100)).unwrap(), docs.to_vec());

    // Absent query yields an empty cursor.
    let mut none = block_on(idx.search_cursor("abq", 0)).unwrap();
    assert!(block_on(none.next(10)).unwrap().is_empty());
}

#[test]
fn fuzzy_tolerates_missing_trigram() {
    let abc = ngram_keys("abc", 3)[0];
    let bcd = ngram_keys("bcd", 3)[0];
    // "abc" matches {1,2,3}; "bcd" matches {2,3,4}.
    let buf = build_rrs(3, 2, &[(abc, bm(&[1, 2, 3])), (bcd, bm(&[2, 3, 4]))]);
    let idx = block_on(Index::open(MemoryFetch::new(buf))).unwrap();

    // "abcd" -> trigrams abc, bcd. Strict (0 missing) = both -> {2,3}.
    let mut strict = block_on(idx.search_cursor("abcd", 0)).unwrap();
    assert_eq!(block_on(strict.page(0, 10)).unwrap(), vec![2, 3]);
    // Fuzzy (tolerate 1) -> match >= 1 -> union {1,2,3,4}.
    let mut fuzzy = block_on(idx.search_cursor("abcd", 1)).unwrap();
    assert_eq!(block_on(fuzzy.page(0, 10)).unwrap(), vec![1, 2, 3, 4]);
    // "abcz": 2nd trigram "bcz" is absent; fuzzy still matches via "abc".
    let mut viaabc = block_on(idx.search_cursor("abcz", 1)).unwrap();
    assert_eq!(block_on(viaabc.page(0, 10)).unwrap(), vec![1, 2, 3]);
    // ...strict needs both, and "bcz" is absent -> empty.
    let mut none = block_on(idx.search_cursor("abcz", 0)).unwrap();
    assert!(block_on(none.page(0, 10)).unwrap().is_empty());
}

/// Appends `s` to the string blob and returns its (offset, length).
fn push_str(blob: &mut Vec<u8>, s: &str) -> (u32, u16) {
    let off = blob.len() as u32;
    blob.extend_from_slice(s.as_bytes());
    (off, s.len() as u16)
}

/// Builds a synthetic `RRSF` facet sidecar from `(field, [(category, bitmap)])`
/// exactly per `FACETS.md`: header, field table, category table (head/tail
/// offsets + cardinality), string blob, then `[head][tail]` postings.
fn build_rrsf(fields: &[(&str, Vec<(&str, RoaringBitmap)>)]) -> Vec<u8> {
    struct Cat {
        name_off: u32,
        name_len: u16,
        card: u32,
        head: Vec<u8>,
        tail: Vec<u8>,
    }
    struct Fld {
        name_off: u32,
        name_len: u16,
        cat_start: u32,
        cats: Vec<Cat>,
    }

    let mut blob: Vec<u8> = Vec::new();
    let mut flds: Vec<Fld> = Vec::new();
    let mut total_cats: u32 = 0;
    for (fname, cats) in fields {
        let (fno, fnl) = push_str(&mut blob, fname);
        let cat_start = total_cats;
        let mut cs = Vec::new();
        for (cname, bitmap) in cats {
            let (cno, cnl) = push_str(&mut blob, cname);
            let (head, tail) = split_head_tail(bitmap);
            cs.push(Cat {
                name_off: cno,
                name_len: cnl,
                card: bitmap.len() as u32,
                head,
                tail,
            });
        }
        total_cats += cs.len() as u32;
        flds.push(Fld {
            name_off: fno,
            name_len: fnl,
            cat_start,
            cats: cs,
        });
    }

    let str_blob_off = 24 + flds.len() * 16 + total_cats as usize * 36;
    let postings_start = str_blob_off + blob.len();

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"RRSF");
    out.extend_from_slice(&1u16.to_le_bytes()); // version
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&(flds.len() as u32).to_le_bytes());
    out.extend_from_slice(&total_cats.to_le_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved2

    for f in &flds {
        out.extend_from_slice(&f.name_off.to_le_bytes());
        out.extend_from_slice(&f.name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // pad
        out.extend_from_slice(&f.cat_start.to_le_bytes());
        out.extend_from_slice(&(f.cats.len() as u32).to_le_bytes());
    }

    let mut off = postings_start as u64;
    for f in &flds {
        for c in &f.cats {
            out.extend_from_slice(&0u64.to_le_bytes()); // key (reader matches by name)
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&(c.head.len() as u32).to_le_bytes());
            out.extend_from_slice(&(c.tail.len() as u32).to_le_bytes());
            out.extend_from_slice(&c.card.to_le_bytes());
            out.extend_from_slice(&c.name_off.to_le_bytes());
            out.extend_from_slice(&c.name_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // pad
            off += (c.head.len() + c.tail.len()) as u64;
        }
    }

    out.extend_from_slice(&blob);
    for f in &flds {
        for c in &f.cats {
            out.extend_from_slice(&c.head);
            out.extend_from_slice(&c.tail);
        }
    }
    out
}

#[test]
fn facet_filtering_within_or_across_and() {
    let abc = ngram_keys("abc", 3)[0];
    let tail_doc = BUCKET + 1;
    // text: "abc" matches docs 1..=5 and one tail doc.
    let idx = block_on(Index::open(MemoryFetch::new(build_rrs(
        3,
        2,
        &[(abc, bm(&[1, 2, 3, 4, 5, tail_doc]))],
    ))))
    .unwrap();

    let facets = block_on(FacetIndex::open(MemoryFetch::new(build_rrsf(&[
        (
            "format",
            vec![
                ("ebook", bm(&[1, 3, 5, tail_doc])),
                ("audiobook", bm(&[2, 4])),
            ],
        ),
        (
            "language",
            vec![("en", bm(&[1, 2, 3])), ("es", bm(&[4, 5, tail_doc]))],
        ),
    ]))))
    .unwrap();

    // Metadata is available without fetching postings.
    assert_eq!(facets.fields().len(), 2);
    let fmt = facets.fields().iter().find(|f| f.name == "format").unwrap();
    assert_eq!(
        fmt.categories
            .iter()
            .find(|c| c.name == "ebook")
            .unwrap()
            .count,
        4
    );

    let page = |pairs: &[(&str, &str)]| -> Vec<u32> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(f, c)| (f.to_string(), c.to_string()))
            .collect();
        let filter = facets.resolve(&owned);
        let mut cur = block_on(idx.search_cursor_filtered("abc", 0, Some(filter))).unwrap();
        block_on(cur.page(0, 100)).unwrap()
    };

    // Single category: text ∩ ebook{1,3,5,tail}.
    assert_eq!(page(&[("format", "ebook")]), vec![1, 3, 5, tail_doc]);
    // Within-field OR: ebook ∪ audiobook covers all text docs.
    assert_eq!(
        page(&[("format", "ebook"), ("format", "audiobook")]),
        vec![1, 2, 3, 4, 5, tail_doc]
    );
    // Across-field AND: ebook{1,3,5,tail} ∩ en{1,2,3} = {1,3}.
    assert_eq!(page(&[("format", "ebook"), ("language", "en")]), vec![1, 3]);
    // Tail exclusion: en has no tail docs, so the tail doc is filtered out.
    assert_eq!(page(&[("language", "en")]), vec![1, 2, 3]);
    // An unresolvable selection (unknown field or category) matches nothing —
    // the user asked for docs in a category none of these docs are in; it must
    // not silently degrade to "unfiltered" (a per-split sidecar may lack a
    // globally-selected category, and a typo'd filter must not return everything).
    assert_eq!(page(&[("nope", "x")]), Vec::<u32>::new());
    assert_eq!(page(&[("format", "nope")]), Vec::<u32>::new());
    // ...but an unresolvable category ORed with a resolvable one in the same
    // field still matches the resolvable side.
    assert_eq!(
        page(&[("format", "ebook"), ("format", "nope")]),
        vec![1, 3, 5, tail_doc]
    );

    // Search-filtered facet counts over the unfiltered query's head result
    // {1,2,3,4,5} (tail_doc is in the tail, not the head): format ebook{1,3,5}=3,
    // audiobook{2,4}=2; language en{1,2,3}=3, es{4,5}=2.
    let cur = block_on(idx.search_cursor("abc", 0)).unwrap();
    assert_eq!(
        facets.counts(cur.head_bitmap()),
        vec![vec![3u64, 2], vec![3, 2]]
    );
}

#[test]
fn facet_open_rejects_out_of_bounds_category_range() {
    let mut rrsf = build_rrsf(&[
        (
            "format",
            vec![("ebook", bm(&[1, 2])), ("audiobook", bm(&[3]))],
        ),
        ("language", vec![("en", bm(&[1])), ("es", bm(&[2]))]),
    ]);
    // Field 0's catCount lives at field_tab(24) + 12 = byte 36. Set it past the
    // 4 categories so cat_start + cat_count exceeds cats_n: open must error, not
    // panic on the out-of-bounds `cats[cat_start..cat_end]` slice.
    rrsf[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
    let got = block_on(FacetIndex::open(MemoryFetch::new(rrsf)));
    assert!(
        matches!(&got, Err(crate::index::IndexError::Malformed(_))),
        "expected Malformed error for an out-of-bounds category range"
    );
}

#[test]
fn facet_open_lazy_loads_only_top_n_heads_per_field() {
    // Force the large-sidecar path (eager_limit = 0) with top_n = 1: only the
    // highest-count category per field gets a head loaded, so filtered counts
    // cover the top category and report 0 for the rest (whose full-corpus counts
    // still come from the meta). This is what keeps boot small for a huge sidecar.
    let rrsf = build_rrsf(&[(
        "format",
        vec![("ebook", bm(&[1, 2, 3])), ("audiobook", bm(&[1, 2]))],
    )]);
    let facets = block_on(FacetIndex::open_tuned(MemoryFetch::new(rrsf), 0, 1)).unwrap();
    // Full-corpus counts (from the meta) are intact for both categories.
    assert_eq!(facets.fields()[0].categories[0].count, 3); // ebook
    assert_eq!(facets.fields()[0].categories[1].count, 2); // audiobook
                                                           // Filtered counts over {1,2,3}: ebook (top-1, head loaded) = 3; audiobook
                                                           // (beyond top-1, head not loaded) = 0.
    assert_eq!(facets.counts(&bm(&[1, 2, 3])), vec![vec![3u64, 0]]);
}

/// Reads the exact `RRSR` bytes the Go writer emits (the golden layout asserted
/// by roaringrange's Go `TestWriteRecordsGoldenLayout`) through the Rust
/// [`RecordStore`]. This is the cross-language guard: it pins that Go-written
/// record-store bytes deserialize in the Rust reader, so a Go build → Rust read
/// round-trip stays byte-compatible without a generated fixture file.
#[test]
fn reads_go_written_rrsr_golden_bytes() {
    use crate::records::RecordStore;
    use crate::MemoryFetch;

    // Records: a JSON-ish row, a zero-length record, and "hello" — identical to
    // the Go golden test. Cumulative end offsets: 0, 16, 16, 21.
    let bin = b"{\"id\":\"A\",\"c\":9}hello".to_vec();
    let mut idx = Vec::new();
    idx.extend_from_slice(b"RRSR"); // magic
    idx.extend_from_slice(&1u16.to_le_bytes()); // version
    idx.extend_from_slice(&0u16.to_le_bytes()); // reserved
    idx.extend_from_slice(&3u32.to_le_bytes()); // count
    idx.extend_from_slice(&0u32.to_le_bytes()); // reserved2
    for off in [0u64, 16, 16, 21] {
        idx.extend_from_slice(&off.to_le_bytes());
    }

    let store = block_on(RecordStore::open(
        MemoryFetch::new(idx),
        MemoryFetch::new(bin),
    ))
    .unwrap();
    assert_eq!(store.len(), 3);
    assert_eq!(
        block_on(store.get(0)).unwrap().unwrap(),
        br#"{"id":"A","c":9}"#
    );
    assert_eq!(block_on(store.get(1)).unwrap().unwrap(), b"");
    assert_eq!(block_on(store.get(2)).unwrap().unwrap(), b"hello");
    assert!(block_on(store.get(3)).unwrap().is_none());
}

/// Round-trips the `RRIL` identifier index through [`crate::build::write_lookup`]
/// and the [`crate::lookup::Lookup`] reader: a write → open → lookup loop must
/// resolve known identifiers (hyphen/case-insensitively, both editions of a
/// shared ISBN in ascending doc order) and miss unknown ones.
#[test]
fn write_lookup_round_trips_through_reader() {
    use crate::build::write_lookup;
    use crate::lookup::Lookup;

    // The same ISBN on two editions (docs 5, 7); an ASIN on doc 10.
    let entries = vec![
        ("978-1-234567-89-0".to_string(), 5u32),
        ("B00ABC123X".to_string(), 10),
        ("978-1-234567-89-0".to_string(), 7),
    ];
    let mut buf = Vec::new();
    write_lookup(&mut buf, &entries).unwrap();

    let lk = block_on(Lookup::open(MemoryFetch::new(buf))).unwrap();
    assert_eq!(lk.len(), 3);
    assert!(!lk.is_empty());
    // Hyphen/case-insensitive ISBN -> both editions, ascending doc (rank) order.
    assert_eq!(block_on(lk.lookup("9781234567890")).unwrap(), vec![5, 7]);
    // ASIN, case-insensitive.
    assert_eq!(block_on(lk.lookup("b00abc123x")).unwrap(), vec![10]);
    // Misses return an empty result.
    assert!(block_on(lk.lookup("0000000000000")).unwrap().is_empty());
    assert!(block_on(lk.lookup("")).unwrap().is_empty());
}

/// Round-trips a zstd-compressed (version-2) record store: train a shared
/// dictionary over the records, write them with
/// [`crate::build::write_records_zstd`], then read them back via
/// [`crate::records::RecordStore::open_with_dict`] and assert each decoded record
/// equals the original. A zero-length record stays addressable. Also asserts that
/// opening the same compressed store *without* the dictionary surfaces an error
/// (never panics) on a compressed record. Gated on the `zstd` feature.
#[cfg(feature = "zstd")]
#[test]
fn write_records_zstd_round_trips_with_dict() {
    use crate::build::{train_record_dict, write_records_zstd};
    use crate::records::RecordStore;

    // Self-similar JSON-ish records (repeated keys) so the dictionary has signal.
    let recs: Vec<Vec<u8>> = (0..64u32)
        .map(|i| {
            format!(
                r#"{{"id":"W{i}","title":"a study of widgets number {i}","venue":"Journal of Widgets","year":20{:02}}}"#,
                i % 25
            )
            .into_bytes()
        })
        .chain(std::iter::once(Vec::new())) // a zero-length record stays addressable
        .collect();

    let samples: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
    let dict = train_record_dict(&samples, 4096).unwrap();
    assert!(!dict.is_empty(), "trained dictionary should be non-empty");

    let mut bin = Vec::new();
    let mut idx = Vec::new();
    write_records_zstd(&mut bin, &mut idx, &recs, &dict, 19).unwrap();
    // Version-2 framed store.
    assert_eq!(u16::from_le_bytes(idx[4..6].try_into().unwrap()), 2);

    let store = block_on(RecordStore::open_with_dict(
        MemoryFetch::new(idx.clone()),
        MemoryFetch::new(bin.clone()),
        dict,
    ))
    .unwrap();
    assert_eq!(store.len() as usize, recs.len());
    for (d, rec) in recs.iter().enumerate() {
        assert_eq!(
            &block_on(store.get(d as u32)).unwrap().unwrap(),
            rec,
            "record {d} must round-trip"
        );
    }

    // The same compressed store opened without a dictionary must error (not
    // panic) on a compressed record. Record 0 is non-trivial JSON, so it was
    // compressed (tag 1); decoding it without the dictionary fails cleanly.
    let no_dict = block_on(RecordStore::open(
        MemoryFetch::new(idx),
        MemoryFetch::new(bin),
    ))
    .unwrap();
    let got = block_on(no_dict.get(0));
    assert!(
        matches!(got, Err(crate::index::IndexError::Malformed(_))),
        "expected Malformed without a dictionary, got {got:?}"
    );
}

/// Reads `go/testdata/<name>_build_golden.txt` (`<name> <hex>`) and asserts `got`
/// matches it byte-for-byte — the shared-golden conformance both the Go tests and
/// these Rust tests assert, so neither port drifts. Regenerate via the matching
/// `gen_<name>_golden` example if the format intentionally changes.
fn assert_build_golden(name: &str, got: &[u8]) {
    let path = format!("../go/testdata/{name}_build_golden.txt");
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let hex = golden
        .trim()
        .strip_prefix(&format!("{name} "))
        .unwrap_or_else(|| panic!("golden {name:?} missing '<name> <hex>' prefix"));
    let want: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    assert_eq!(
        got,
        want.as_slice(),
        "{name} build drifted from the committed golden"
    );
}

/// `write_lookup` over the fixed entries must equal the committed golden that
/// `go/lookup_test.go` also asserts (mirrors `gen_rril_golden.rs`).
#[test]
fn rril_golden_matches() {
    let entries: Vec<(String, u32)> = [
        ("978-0-13-468599-1", 5u32),
        ("B07XYZ1234", 2),
        ("978-0-13-468599-1", 9),
        ("isbn:0262033844", 7),
        ("", 3),
        ("!!!", 4),
        ("AbC123", 1),
        ("b07xyz1234", 8),
    ]
    .iter()
    .map(|(s, d)| (s.to_string(), *d))
    .collect();
    let mut out = Vec::new();
    crate::build::write_lookup(&mut out, &entries).unwrap();
    assert_build_golden("rril", &out);
}
