//! Secondary (alternate-sort) index builder — **remap-based**, run as a separate
//! `-secondary` step against the *finished* primary outputs.
//!
//! A primary build orders docs by static rank (citations). A secondary index
//! orders the same docs by another key — here publication year, descending
//! ("newest") — and lets the demo page in that order while keeping records and
//! facets keyed by the primary doc ID. It is produced entirely by **remapping the
//! primary postings through a permutation**, with no source re-stream and no
//! re-tokenization:
//!
//!   1. derive a per-doc sort value (the year, read back out of the primary `.rrf`'s
//!      `year` facet field — so no record parsing is needed);
//!   2. sort doc IDs by `(year desc, primary-id asc)` to get the secondary order →
//!      the permutation `perm[secondary_id] = primary_id` (the `RRSC` perm column),
//!      and its inverse `secondary_of_primary` used for the remap;
//!   3. remap every primary text posting and facet posting through the inverse
//!      (each primary doc ID becomes its secondary doc ID), yielding a date-desc
//!      `.rrs` and a date-desc `.rrf`.
//!
//! Remapping a set is byte-equivalent to having re-tokenized in the new order — the
//! same set of docs is expressed in secondary IDs — so the secondary `.rrs` reads
//! back identically to a re-tokenized one (verified by search-equivalence in the
//! tests). Facet *counts* are set cardinalities and so are order-independent; only
//! which docs land on a page changes.
//!
//! This module touches no reader code: it parses the primary `.rrs`/`.rrf` byte
//! layouts directly and writes via the crate's public writers
//! ([`write_index`]/[`write_facets`]/[`write_perm`]). The text-index remap streams
//! by posting (only the dictionary + the inverse permutation stay resident) and
//! fans the per-posting remap across rayon, so a multi-GB index stays tractable.

use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::{
    split_posting, write_facets, write_perm, FacetCategory, FacetField, DEFAULT_HEAD_BOUNDARY,
};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use tracing::info;

/// Postings remapped per rayon batch (bounds peak RAM to one batch + the inverse
/// permutation, independent of the full index size).
const REMAP_BATCH: usize = 4096;

/// Reads the doc count `N` from a record-store `.idx` header (`RRSR`: magic[4],
/// version u16, reserved u16, count u32, …). `N` sizes the permutation, which
/// must cover every doc — including any with no indexed trigram.
pub fn read_record_count(idx_path: &str) -> io::Result<u32> {
    let mut f = File::open(idx_path)?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"RRSR" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "record idx: bad magic (expected RRSR)",
        ));
    }
    Ok(u32::from_le_bytes(hdr[8..12].try_into().unwrap()))
}

/// One primary facet field parsed from the `.rrf`: its name and `(category name,
/// full posting)` pairs (head and tail unioned back into the whole bitmap).
pub(crate) struct FacetFieldRaw {
    pub(crate) name: String,
    pub(crate) cats: Vec<(String, RoaringBitmap)>,
}

/// Parses the primary `.rrf` (`RRSF`) into per-field categories with full primary
/// postings. The whole file is read into memory (the facet sidecar is small
/// relative to the text index); postings are sliced by the category table's
/// absolute offsets. Mirrors the `write_facets` byte layout.
pub(crate) fn read_facets(path: &str) -> io::Result<Vec<FacetFieldRaw>> {
    let buf = std::fs::read(path)?;
    let u16le = |o: usize| u16::from_le_bytes(buf[o..o + 2].try_into().unwrap());
    let u32le = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    let u64le = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    if buf.len() < 24 || &buf[0..4] != b"RRSF" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "facets: bad magic",
        ));
    }
    let field_count = u32le(8) as usize;
    let total_cats = u32le(12) as usize;
    let str_bytes = u32le(16) as usize;

    let field_tbl = 24;
    let cat_tbl = field_tbl + field_count * 16;
    let str_blob = cat_tbl + total_cats * 36;
    let name = |off: usize, len: usize| -> String {
        String::from_utf8_lossy(&buf[str_blob + off..str_blob + off + len]).into_owned()
    };
    let _ = str_bytes;

    let read_posting = |off: u64, hlen: usize, tlen: usize| -> io::Result<RoaringBitmap> {
        let o = off as usize;
        let mut bm = RoaringBitmap::deserialize_from(&buf[o..o + hlen])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        bm |= RoaringBitmap::deserialize_from(&buf[o + hlen..o + hlen + tlen])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(bm)
    };

    let mut fields = Vec::with_capacity(field_count);
    for fi in 0..field_count {
        let fo = field_tbl + fi * 16;
        let fname = name(u32le(fo) as usize, u16le(fo + 4) as usize);
        let cat_start = u32le(fo + 8) as usize;
        let cat_count = u32le(fo + 12) as usize;
        let mut cats = Vec::with_capacity(cat_count);
        for ci in cat_start..cat_start + cat_count {
            let co = cat_tbl + ci * 36;
            let off = u64le(co + 8);
            let hlen = u32le(co + 16) as usize;
            let tlen = u32le(co + 20) as usize;
            let cname = name(u32le(co + 28) as usize, u16le(co + 32) as usize);
            cats.push((cname, read_posting(off, hlen, tlen)?));
        }
        fields.push(FacetFieldRaw { name: fname, cats });
    }
    Ok(fields)
}

/// Builds a dense per-doc value array from a numeric facet field: each category
/// whose name parses as an integer sets that value for every doc in its posting.
/// Docs absent from the field keep `i64::MIN`, so "year desc" sorts them last.
fn values_from_field(fields: &[FacetFieldRaw], field_name: &str, n: usize) -> Vec<i64> {
    let mut vals = vec![i64::MIN; n];
    if let Some(f) = fields.iter().find(|f| f.name == field_name) {
        for (cat, bm) in &f.cats {
            if let Ok(v) = cat.parse::<i64>() {
                for d in bm {
                    if (d as usize) < n {
                        vals[d as usize] = v;
                    }
                }
            }
        }
    }
    vals
}

/// Computes the secondary ordering from per-doc sort values: doc IDs sorted by
/// `(value desc, primary-id asc)`. Returns `(perm, inverse)` where
/// `perm[secondary_id] = primary_id` (the `RRSC` perm column) and
/// `inverse[primary_id] = secondary_id` (used to remap postings). The ascending
/// primary-id tiebreak keeps equal-value docs in primary-rank order — i.e.
/// "newest, then most-cited".
fn order_perm(values: &[i64]) -> (Vec<u32>, Vec<u32>) {
    let n = values.len();
    let mut perm: Vec<u32> = (0..n as u32).collect();
    // Parallel sort; the explicit (value desc, id asc) total order is deterministic,
    // so an unstable parallel sort yields the identical permutation.
    perm.par_sort_unstable_by(|&a, &b| values[b as usize].cmp(&values[a as usize]).then(a.cmp(&b)));
    let mut inverse = vec![0u32; n];
    for (s, &p) in perm.iter().enumerate() {
        inverse[p as usize] = s as u32;
    }
    (perm, inverse)
}

/// Remaps a posting of primary doc IDs to secondary doc IDs via `inverse`. Collects
/// then sorts so the new bitmap is built from an ascending sequence (fast container
/// construction) rather than scattered inserts.
fn remap_bitmap(bm: &RoaringBitmap, inverse: &[u32]) -> RoaringBitmap {
    let mut v: Vec<u32> = bm.iter().map(|d| inverse[d as usize]).collect();
    v.sort_unstable();
    RoaringBitmap::from_sorted_iter(v).expect("sorted remap")
}

/// Remaps the facet postings into secondary space and writes the secondary `.rrf`.
/// Cardinalities are preserved (same sets), and categories are re-sorted by name so
/// the byte layout matches a primary-style build.
fn write_secondary_facets(fields: &[FacetFieldRaw], inverse: &[u32], out: &str) -> io::Result<()> {
    let out_fields: Vec<FacetField> = fields
        .iter()
        .map(|f| {
            // Remap categories in parallel (the topic field has thousands).
            let mut cats: Vec<FacetCategory> = f
                .cats
                .par_iter()
                .map(|(name, bm)| {
                    let remapped = remap_bitmap(bm, inverse);
                    let card = remapped.len() as u32;
                    let (head, tail) = split_posting(&remapped, DEFAULT_HEAD_BOUNDARY);
                    FacetCategory {
                        name: name.clone(),
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            cats.sort_by(|a, b| a.name.cmp(&b.name));
            FacetField {
                name: f.name.clone(),
                cats,
            }
        })
        .collect();
    let w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    write_facets(w, out_fields)
}

/// Streams the primary `.rrs`, remapping each trigram posting into secondary space,
/// and writes the secondary `.rrs`. Reads the header + dictionary (keys are
/// key-sorted and unchanged by the remap), then walks the postings in dictionary
/// order, remapping a batch at a time across rayon and writing them back in order.
/// Only one batch of postings + the inverse permutation stay resident, so a
/// multi-GB index does not need to be held in memory. The output dictionary keys,
/// order, and sparse index match the input's — only posting bytes and offsets
/// change. Mirrors the `write_index` layout.
fn remap_text_index(in_path: &str, inverse: &[u32], out_path: &str) -> io::Result<()> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(in_path)?);
    let mut hdr = [0u8; 16];
    r.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"RRSI" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "rrs: bad magic"));
    }
    let version = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
    let gram = u16::from_le_bytes(hdr[6..8].try_into().unwrap());
    let ngrams = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
    let stride = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
    let sparse_count = if ngrams == 0 {
        0
    } else {
        ngrams.div_ceil(stride.max(1) as usize)
    };

    // A version-2 header carries a trailing 4-byte head_boundary; consume it so the
    // cursor reaches the sparse index. Skip the sparse index (rebuilt from the
    // dictionary below) and read the dictionary — the cursor then lands at the first
    // posting (dictionary order).
    let header_extra = if version >= 2 { 4i64 } else { 0 };
    r.seek(SeekFrom::Current(header_extra + (sparse_count * 8) as i64))?;
    let mut keys: Vec<u64> = Vec::with_capacity(ngrams);
    let mut sizes: Vec<(usize, usize)> = Vec::with_capacity(ngrams);
    for _ in 0..ngrams {
        let mut e = [0u8; 24];
        r.read_exact(&mut e)?;
        keys.push(u64::from_le_bytes(e[0..8].try_into().unwrap()));
        let hlen = u32::from_le_bytes(e[16..20].try_into().unwrap()) as usize;
        let tlen = u32::from_le_bytes(e[20..24].try_into().unwrap()) as usize;
        sizes.push((hlen, tlen));
    }

    // Output layout uses the 20-byte version-2 header.
    let dict_start = 20 + sparse_count * 8;
    let postings_start = (dict_start + ngrams * 24) as u64;

    // Write postings first (after the reserved header/dict region), recording the
    // output dictionary as we go; then seek back and write header + sparse + dict.
    let mut out = File::create(out_path)?;
    out.seek(SeekFrom::Start(postings_start))?;
    let mut bw = BufWriter::with_capacity(1 << 20, out);
    let mut out_dict: Vec<(u64, u64, u32, u32)> = Vec::with_capacity(ngrams); // key, off, hlen, tlen
    let mut off = postings_start;

    let mut idx = 0usize;
    while idx < ngrams {
        let end = (idx + REMAP_BATCH).min(ngrams);
        // Read this batch's raw postings sequentially.
        let mut raw: Vec<Vec<u8>> = Vec::with_capacity(end - idx);
        for &(hlen, tlen) in &sizes[idx..end] {
            let mut b = vec![0u8; hlen + tlen];
            r.read_exact(&mut b)?;
            raw.push(b);
        }
        // Remap in parallel: deserialize head+tail, union, remap, re-split.
        let remapped: Vec<(Vec<u8>, Vec<u8>)> = raw
            .par_iter()
            .zip(&sizes[idx..end])
            .map(|(bytes, &(hlen, _))| {
                let mut bm = RoaringBitmap::deserialize_from(&bytes[..hlen]).expect("head");
                bm |= RoaringBitmap::deserialize_from(&bytes[hlen..]).expect("tail");
                let s = remap_bitmap(&bm, inverse);
                split_posting(&s, DEFAULT_HEAD_BOUNDARY)
            })
            .collect();
        // Write in dictionary order.
        for (i, (head, tail)) in remapped.into_iter().enumerate() {
            bw.write_all(&head)?;
            bw.write_all(&tail)?;
            out_dict.push((keys[idx + i], off, head.len() as u32, tail.len() as u32));
            off += (head.len() + tail.len()) as u64;
        }
        idx = end;
    }
    bw.flush()?;
    let mut out = bw.into_inner().map_err(|e| e.into_error())?;

    // Header + sparse index + dictionary (keys already in sorted dictionary order).
    out.seek(SeekFrom::Start(0))?;
    out.write_all(b"RRSI")?;
    out.write_all(&2u16.to_le_bytes())?;
    out.write_all(&gram.to_le_bytes())?;
    out.write_all(&(ngrams as u32).to_le_bytes())?;
    out.write_all(&stride.to_le_bytes())?;
    out.write_all(&DEFAULT_HEAD_BOUNDARY.to_le_bytes())?;
    for i in 0..sparse_count {
        out.write_all(&out_dict[i * stride as usize].0.to_le_bytes())?;
    }
    for (key, poff, hlen, tlen) in &out_dict {
        out.write_all(&key.to_le_bytes())?;
        out.write_all(&poff.to_le_bytes())?;
        out.write_all(&hlen.to_le_bytes())?;
        out.write_all(&tlen.to_le_bytes())?;
    }
    out.flush()
}

/// Builds the secondary index from the finished primary `.rrs`/`.rrf`: derives the
/// `sort_field` (e.g. `"year"`) values from the primary facets, computes the
/// `(value desc, primary asc)` permutation, and writes the perm column, the
/// remapped secondary `.rrs`, and the remapped secondary `.rrf`. `n` is the doc
/// count (the perm spans `0..n`).
#[allow(clippy::too_many_arguments)]
pub fn build_secondary(
    rrs_in: &str,
    rrf_in: &str,
    n: usize,
    sort_field: &str,
    out_rrs: &str,
    out_rrf: &str,
    out_perm: &str,
) -> io::Result<()> {
    let t0 = std::time::Instant::now();
    let fields = read_facets(rrf_in)?;
    let values = values_from_field(&fields, sort_field, n);
    let present = values.iter().filter(|&&v| v != i64::MIN).count();
    let (perm, inverse) = order_perm(&values);
    info!(
        docs = n, present, sort_field = %sort_field,
        elapsed_s = t0.elapsed().as_secs_f64(), "computed permutation"
    );

    let tp = std::time::Instant::now();
    {
        let w = BufWriter::with_capacity(1 << 20, File::create(out_perm)?);
        write_perm(w, perm)?;
    }
    info!(
        elapsed_s = tp.elapsed().as_secs_f64(),
        "wrote perm column {out_perm}"
    );

    let tf = std::time::Instant::now();
    write_secondary_facets(&fields, &inverse, out_rrf)?;
    info!(
        elapsed_s = tf.elapsed().as_secs_f64(),
        "wrote secondary facets {out_rrf}"
    );

    let tr = std::time::Instant::now();
    remap_text_index(rrs_in, &inverse, out_rrs)?;
    info!(
        elapsed_s = tr.elapsed().as_secs_f64(),
        "wrote secondary index {out_rrs}"
    );

    info!(
        elapsed_s = t0.elapsed().as_secs_f64(),
        "secondary build complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use roaringrange::build::{write_index, DEFAULT_STRIDE};
    use roaringrange::ngram_keys;
    use roaringrange::{FacetIndex, Index, MemoryFetch, SortCols};

    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    /// `order_perm` sorts by value-desc with an ascending primary-id tiebreak, and
    /// `perm`/`inverse` are consistent inverses over the full doc range.
    #[test]
    fn perm_is_date_desc_and_consistent() {
        //          doc: 0    1    2    3    4
        let years = [2001, 2020, 2020, 1999, i64::MIN];
        let (perm, inverse) = order_perm(&years);
        // 2020 (docs 1,2 — tiebreak asc) > 2001 (0) > 1999 (3) > missing (4).
        assert_eq!(perm, vec![1, 2, 0, 3, 4]);
        for (s, &p) in perm.iter().enumerate() {
            assert_eq!(inverse[p as usize], s as u32);
        }
    }

    /// A remapped posting holds exactly the primary docs' secondary IDs.
    #[test]
    fn remap_bitmap_maps_every_doc() {
        let inverse = [3u32, 0, 1, 2]; // primary -> secondary
        let got = remap_bitmap(&bm(&[0, 2, 3]), &inverse);
        assert_eq!(got, bm(&[3, 1, 2]));
    }

    /// End-to-end on a tiny 2-trigram corpus: build a primary `.rrs`/`.rrf`, run the
    /// secondary build, and confirm via the readers that (a) the perm column round
    /// trips, (b) searching the secondary index and mapping results back through the
    /// perm yields the *same primary doc set* as searching the primary index, and
    /// (c) the page order is newest-first.
    #[test]
    fn secondary_search_equivalent_to_primary() {
        let dir = std::env::temp_dir();
        let p = |s: &str| {
            dir.join(format!("rr_sec_{s}"))
                .to_str()
                .unwrap()
                .to_string()
        };
        let (rrs_in, rrf_in) = (p("in.rrs"), p("in.rrf"));
        let (o_rrs, o_rrf, o_perm) = (p("out.rrs"), p("out.rrf"), p("out.perm"));

        // 5 docs in primary (citation) order; "abc" in docs {0,2,4}, "bcd" in {1,2,3}.
        let n = 5usize;
        let abc = ngram_keys("abc", 3)[0];
        let bcd = ngram_keys("bcd", 3)[0];
        let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = vec![
            {
                let (h, t) = split_posting(&bm(&[0, 2, 4]), DEFAULT_HEAD_BOUNDARY);
                (abc, h, t)
            },
            {
                let (h, t) = split_posting(&bm(&[1, 2, 3]), DEFAULT_HEAD_BOUNDARY);
                (bcd, h, t)
            },
        ];
        {
            let w = BufWriter::new(File::create(&rrs_in).unwrap());
            write_index(w, 3, DEFAULT_STRIDE, DEFAULT_HEAD_BOUNDARY, entries).unwrap();
        }
        // A "year" facet field encoding per-doc years: 2020 -> {1,4}, 2010 -> {0,2}, 1990 -> {3}.
        let mk = |name: &str, b: RoaringBitmap| {
            let (head, tail) = split_posting(&b, DEFAULT_HEAD_BOUNDARY);
            FacetCategory {
                name: name.to_string(),
                card: b.len() as u32,
                head,
                tail,
            }
        };
        {
            let w = BufWriter::new(File::create(&rrf_in).unwrap());
            write_facets(
                w,
                vec![FacetField {
                    name: "year".to_string(),
                    cats: vec![
                        mk("1990", bm(&[3])),
                        mk("2010", bm(&[0, 2])),
                        mk("2020", bm(&[1, 4])),
                    ],
                }],
            )
            .unwrap();
        }

        build_secondary(&rrs_in, &rrf_in, n, "year", &o_rrs, &o_rrf, &o_perm).unwrap();

        // Newest-first order: 2020{1,4} (asc) > 2010{0,2} > 1990{3} -> perm [1,4,0,2,3].
        let perm = block_on(SortCols::open(MemoryFetch::new(
            std::fs::read(&o_perm).unwrap(),
        )))
        .unwrap();
        let primary_of_secondary = block_on(perm.slice_u32(0, 0, n)).unwrap();
        assert_eq!(primary_of_secondary, vec![1, 4, 0, 2, 3]);

        // Search "abc" in both spaces; the secondary results mapped back through the
        // perm must equal the primary result set.
        let pidx = block_on(Index::open(MemoryFetch::new(
            std::fs::read(&rrs_in).unwrap(),
        )))
        .unwrap();
        let sidx = block_on(Index::open(MemoryFetch::new(
            std::fs::read(&o_rrs).unwrap(),
        )))
        .unwrap();
        let prim: std::collections::BTreeSet<u32> = block_on(pidx.search("abc", 10))
            .unwrap()
            .into_iter()
            .collect();
        let sec_ids = block_on(sidx.search("abc", 10)).unwrap();
        let sec_as_primary: std::collections::BTreeSet<u32> = sec_ids
            .iter()
            .map(|&s| primary_of_secondary[s as usize])
            .collect();
        assert_eq!(
            prim, sec_as_primary,
            "secondary search set differs from primary"
        );
        // {0,2,4} are years {2010,2010,2020}; newest-first that's doc 4 then 0,2.
        assert_eq!(
            sec_ids
                .iter()
                .map(|&s| primary_of_secondary[s as usize])
                .collect::<Vec<_>>(),
            vec![4, 0, 2]
        );

        // The secondary facet sidecar opens and preserves cardinalities.
        let sfac = block_on(FacetIndex::open(MemoryFetch::new(
            std::fs::read(&o_rrf).unwrap(),
        )))
        .unwrap();
        let yf = sfac.fields.iter().find(|f| f.name == "year").unwrap();
        assert_eq!(
            yf.categories
                .iter()
                .find(|c| c.name == "2020")
                .unwrap()
                .count,
            2
        );

        for s in ["in.rrs", "in.rrf", "out.rrs", "out.rrf", "out.perm"] {
            let _ = std::fs::remove_file(dir.join(format!("rr_sec_{s}")));
        }
    }
}
