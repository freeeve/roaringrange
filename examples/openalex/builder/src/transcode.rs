//! Head-boundary transcode (`-transcode`): re-split a finished primary `.rrs` (and
//! its `.rrf`) at a new head/tail boundary `B` without re-indexing.
//!
//! A finished file stores each trigram's full posting as head∪tail, so re-splitting
//! that union at any `B'` yields exactly the head/tail a from-source build at `B'`
//! would — the split is a pure function of the doc-id set and `B'`. The text index
//! is transcoded by streaming its dictionary in key order: each posting is read,
//! unioned, re-split, and rewritten, so peak memory is one chunk of postings plus
//! the rebuilt dictionary, never the whole index. Chunks re-split in parallel; the
//! postings are written sequentially in key order. The facet sidecar is small
//! enough to transcode in memory via the crate's facet reader/writer.
//!
//! Postings stay uncompressed: the reader range-fetches sub-ranges of a posting (a
//! head whole, a tail's individual containers), which a compressed blob would
//! preclude.

use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::{write_facets, FacetCategory, FacetField};
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::time::Instant;
use tracing::{info, info_span};

use crate::secondary::read_facets;

/// Dictionary entry: key(8) + headOffset(8) + headSize(4) + tailSize(4).
const DICT_ENTRY: usize = 24;
/// Trigrams re-split per parallel chunk. Kept small because the densest postings
/// (the ASCII trigrams) sort to the front together; a large chunk would hold all
/// their re-split bytes in memory at once.
const CHUNK: usize = 1024;

/// Splits `full` at boundary `b` into serialized (head, tail): head = `full ∩
/// [0, b)`, tail = `full ∩ [b, ∞)`. Manual split (not the build-time const) so any
/// boundary is supported.
fn split_at(full: RoaringBitmap, b: u32) -> (Vec<u8>, Vec<u8>) {
    let mut head = full.clone();
    head.remove_range(b..);
    let mut tail = full; // reuse the input for the tail — no second clone
    tail.remove_range(0..b);
    let mut hb = Vec::with_capacity(head.serialized_size());
    head.serialize_into(&mut hb).expect("serialize head");
    let mut tb = Vec::with_capacity(tail.serialized_size());
    tail.serialize_into(&mut tb).expect("serialize tail");
    (hb, tb)
}

/// Reads a full posting (head∪tail) from `file` at the given byte ranges.
fn read_full(
    file: &File,
    head_off: u64,
    head_size: u32,
    tail_size: u32,
) -> io::Result<RoaringBitmap> {
    let mut buf = vec![0u8; (head_size + tail_size) as usize];
    file.read_exact_at(&mut buf, head_off)?;
    let hs = head_size as usize;
    let mut bm = RoaringBitmap::deserialize_from(&buf[..hs])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bm |= RoaringBitmap::deserialize_from(&buf[hs..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(bm)
}

/// `(gram_size, ngrams, stride, dict_start)` from an `RRSI` header. Handles both the
/// legacy 16-byte header (version 1, no head_boundary) and the 20-byte version-2
/// header, so the transcoder reads either and always writes version 2 — i.e. it
/// also migrates an old-format index to the self-describing format.
fn read_header(file: &File) -> io::Result<(u16, usize, usize, usize)> {
    let mut hdr = [0u8; 16];
    file.read_exact_at(&mut hdr, 0)?;
    if &hdr[0..4] != b"RRSI" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "rrs: bad magic"));
    }
    let version = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
    let gram = u16::from_le_bytes(hdr[6..8].try_into().unwrap());
    let ngrams = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
    let stride = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let header_size = if version >= 2 { 20 } else { 16 };
    let sparse_count = if ngrams == 0 {
        0
    } else {
        ngrams.div_ceil(stride)
    };
    Ok((gram, ngrams, stride, header_size + sparse_count * 8))
}

/// Reads dictionary entry `i` (`(key, head_off, head_size, tail_size)`).
fn read_entry(file: &File, dict_start: usize, i: usize) -> io::Result<(u64, u64, u32, u32)> {
    let mut e = [0u8; DICT_ENTRY];
    file.read_exact_at(&mut e, (dict_start + i * DICT_ENTRY) as u64)?;
    Ok((
        u64::from_le_bytes(e[0..8].try_into().unwrap()),
        u64::from_le_bytes(e[8..16].try_into().unwrap()),
        u32::from_le_bytes(e[16..20].try_into().unwrap()),
        u32::from_le_bytes(e[20..24].try_into().unwrap()),
    ))
}

/// Transcodes the text index `in_path` to `out_path`, re-splitting every posting at
/// `new_b`. Streams the dictionary in key order; re-splits each chunk in parallel,
/// writes postings sequentially.
pub fn transcode_rrs(in_path: &str, out_path: &str, new_b: u32) -> io::Result<()> {
    let inf = File::open(in_path)?;
    let (gram, ngrams, stride, in_dict_start) = read_header(&inf)?;
    let sparse_count = if ngrams == 0 {
        0
    } else {
        ngrams.div_ceil(stride)
    };
    // The output is always written with the 20-byte version-2 header.
    let out_postings_start = (20 + sparse_count * 8 + ngrams * DICT_ENTRY) as u64;

    let outf = File::create(out_path)?;

    // Pass 1: stream the dict in key order, re-split each chunk in parallel, write
    // postings sequentially, recording the rebuilt dictionary.
    let mut new_dict: Vec<(u64, u64, u32, u32)> = Vec::with_capacity(ngrams);
    let mut off = out_postings_start;
    {
        let mut bw = BufWriter::with_capacity(1 << 22, &outf);
        bw.seek(SeekFrom::Start(out_postings_start))?;
        let mut processed = 0usize;
        let mut dbuf = vec![0u8; CHUNK * DICT_ENTRY];
        while processed < ngrams {
            let this = CHUNK.min(ngrams - processed);
            inf.read_exact_at(
                &mut dbuf[..this * DICT_ENTRY],
                (in_dict_start + processed * DICT_ENTRY) as u64,
            )?;
            let entries: Vec<(u64, u64, u32, u32)> = dbuf[..this * DICT_ENTRY]
                .chunks_exact(DICT_ENTRY)
                .map(|e| {
                    (
                        u64::from_le_bytes(e[0..8].try_into().unwrap()),
                        u64::from_le_bytes(e[8..16].try_into().unwrap()),
                        u32::from_le_bytes(e[16..20].try_into().unwrap()),
                        u32::from_le_bytes(e[20..24].try_into().unwrap()),
                    )
                })
                .collect();
            let resplit: Vec<(Vec<u8>, Vec<u8>)> = entries
                .par_iter()
                .map(|&(_k, ho, hs, ts)| {
                    let full = read_full(&inf, ho, hs, ts).expect("read posting");
                    split_at(full, new_b)
                })
                .collect();
            for (&(k, _, _, _), (hb, tb)) in entries.iter().zip(resplit.iter()) {
                bw.write_all(hb)?;
                bw.write_all(tb)?;
                new_dict.push((k, off, hb.len() as u32, tb.len() as u32));
                off += (hb.len() + tb.len()) as u64;
            }
            processed += this;
        }
        bw.flush()?;
    }

    // Pass 2: 20-byte version-2 header + sparse index + dictionary at the front.
    {
        let mut bw = BufWriter::with_capacity(1 << 22, &outf);
        bw.seek(SeekFrom::Start(0))?;
        bw.write_all(b"RRSI")?;
        bw.write_all(&2u16.to_le_bytes())?; // version 2
        bw.write_all(&gram.to_le_bytes())?;
        bw.write_all(&(ngrams as u32).to_le_bytes())?;
        bw.write_all(&(stride as u32).to_le_bytes())?;
        bw.write_all(&new_b.to_le_bytes())?; // head_boundary
        for i in 0..sparse_count {
            bw.write_all(&new_dict[i * stride].0.to_le_bytes())?;
        }
        for &(k, o, hl, tl) in &new_dict {
            bw.write_all(&k.to_le_bytes())?;
            bw.write_all(&o.to_le_bytes())?;
            bw.write_all(&hl.to_le_bytes())?;
            bw.write_all(&tl.to_le_bytes())?;
        }
        bw.flush()?;
    }
    Ok(())
}

/// Transcodes the facet sidecar `in_path` to `out_path` at `new_b` (in memory — the
/// sidecar is small). Re-splits each category's full posting.
pub fn transcode_rrf(in_path: &str, out_path: &str, new_b: u32) -> io::Result<()> {
    let raw = read_facets(in_path)?;
    let fields: Vec<FacetField> = raw
        .into_iter()
        .map(|f| {
            let cats = f
                .cats
                .into_iter()
                .map(|(name, full)| {
                    let card = full.len() as u32;
                    let (head, tail) = split_at(full, new_b);
                    FacetCategory {
                        name,
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            FacetField { name: f.name, cats }
        })
        .collect();
    let w = BufWriter::new(File::create(out_path)?);
    write_facets(w, fields)
}

/// Samples `samples` keys spread across the dictionaries of `orig` and `new` and
/// asserts the keys match and each full posting (head∪tail) is identical — the
/// transcode must move the split point, never change the doc set. Panics on
/// mismatch.
fn verify_rrs(orig: &str, new: &str, samples: usize) -> io::Result<()> {
    let of = File::open(orig)?;
    let nf = File::open(new)?;
    let (_, n1, _, ds1) = read_header(&of)?;
    let (_, n2, _, ds2) = read_header(&nf)?;
    assert_eq!(n1, n2, "ngram count changed by transcode");
    let step = (n1 / samples.max(1)).max(1);
    let mut checked = 0usize;
    let mut i = 0usize;
    while i < n1 {
        let (k1, o1, h1, t1) = read_entry(&of, ds1, i)?;
        let (k2, o2, h2, t2) = read_entry(&nf, ds2, i)?;
        assert_eq!(k1, k2, "key order changed at entry {i}");
        let f1 = read_full(&of, o1, h1, t1)?;
        let f2 = read_full(&nf, o2, h2, t2)?;
        assert!(f1 == f2, "full posting changed for key {k1} at entry {i}");
        checked += 1;
        i += step;
    }
    info!(checked, "verify: sampled full postings identical");
    Ok(())
}

/// `-transcode` entry. Flags: `-head B` (default 1<<20 = 1M); `-in-rrs`/`-out-rrs`,
/// `-in-rrf`/`-out-rrf`; `-verify` to sample-check the rrs round-trip.
pub fn run(args: &[String]) {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let new_b: u32 = flag("-head")
        .map(|s| s.parse().expect("-head"))
        .unwrap_or(1 << 20);
    let verify = args.iter().any(|a| a == "-verify");
    let _span = info_span!("transcode", head = new_b).entered();
    let t0 = Instant::now();

    if let (Some(inp), Some(outp)) = (flag("-in-rrs"), flag("-out-rrs")) {
        let t = Instant::now();
        transcode_rrs(&inp, &outp, new_b).expect("transcode rrs");
        info!(out = %outp, elapsed_s = t.elapsed().as_secs_f64(), "rrs transcoded");
        if verify {
            verify_rrs(&inp, &outp, 2000).expect("verify rrs");
        }
    }
    if let (Some(inp), Some(outp)) = (flag("-in-rrf"), flag("-out-rrf")) {
        let t = Instant::now();
        transcode_rrf(&inp, &outp, new_b).expect("transcode rrf");
        info!(out = %outp, elapsed_s = t.elapsed().as_secs_f64(), "rrf transcoded");
    }
    info!(
        head = new_b,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "transcode done"
    );
}
