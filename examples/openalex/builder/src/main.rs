//! Parallel Rust builder for the OpenAlex demo index.
//!
//! Produces the same `.rrs` (text index), `.rrf` (facet sidecar), and record
//! store as the Go loader in `examples/openalex`, but builds them directly with
//! the reader crate's `build` module — no roaringsearch, no FTSR, no transcode,
//! and no GC. It streams the works twice so the source text never lives in RAM:
//!
//!   pass 1: read only (id, cited_by_count) for every work, rank by citations
//!           descending to assign doc IDs (doc 0 = most cited);
//!   pass 2: re-stream, tokenize each work's text into trigram keys, insert its
//!           doc ID into key-sharded roaring bitmaps + facet bitmaps, and emit
//!           its record. Parsing/tokenizing fan out across files with rayon.
//!
//! Then each posting is split head/tail and the index, facets, and record store
//! are written. Peak memory is the index + facet bitmaps + the records — never
//! the works' text.

use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange_reader::build::{
    split_posting, write_records, write_rrs, write_rrsf, FacetCatOut, FacetFieldOut, DEFAULT_STRIDE,
};
use roaringrange_reader::ngram_keys;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// Trigram size (matches the index/reader contract).
const GRAM: usize = 3;
/// Byte cap on the reconstructed abstract, bounding indexed text per work.
const ABSTRACT_CHAR_CAP: usize = 2000;
/// Number of key shards; each is an independently-locked bitmap map, so parse
/// threads insert with low contention.
const KEY_SHARDS: usize = 256;
/// Facet fields, emitted in this order.
const FACET_FIELDS: [&str; 5] = ["year", "type", "oa", "language", "topic"];

/// Pass-1 view: just enough to rank a work.
#[derive(Deserialize)]
struct RankRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    cited_by_count: Option<i64>,
}

/// Pass-2 view: the fields the index, facets, and record need. Unknown fields
/// are ignored; nullable scalars are `Option` so a `null` doesn't fail the line.
#[derive(Deserialize)]
struct Work {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    abstract_inverted_index: Option<HashMap<String, Vec<i64>>>,
    #[serde(default)]
    authorships: Vec<Authorship>,
    #[serde(default)]
    publication_year: Option<i64>,
    #[serde(default, rename = "type")]
    work_type: Option<String>,
    #[serde(default)]
    open_access: Option<OpenAccess>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    primary_topic: Option<Named>,
    #[serde(default)]
    concepts: Vec<Named>,
    #[serde(default)]
    cited_by_count: Option<i64>,
    #[serde(default)]
    primary_location: Option<PrimaryLocation>,
}

#[derive(Deserialize)]
struct Authorship {
    #[serde(default)]
    author: Option<Named>,
}
#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    display_name: Option<String>,
}
#[derive(Deserialize)]
struct OpenAccess {
    #[serde(default)]
    oa_status: Option<String>,
}
#[derive(Deserialize)]
struct PrimaryLocation {
    #[serde(default)]
    source: Option<Named>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let in_glob = arg(&args, "-in", "/tmp/openalex/works/*/*.gz");
    let rrs_path = arg(&args, "-rrs", "/tmp/openalex.rrs");
    let facets_path = arg(&args, "-facets", "/tmp/openalex.rrf");
    let bin_path = arg(&args, "-bin", "/tmp/openalex-records.bin");
    let idx_path = arg(&args, "-idx", "/tmp/openalex-records.idx");
    let limit: usize = arg(&args, "-limit", "0").parse().unwrap_or(0);

    let mut files: Vec<PathBuf> = glob::glob(&in_glob)
        .expect("invalid -in glob")
        .filter_map(Result::ok)
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("no input files matched {in_glob}");
        std::process::exit(1);
    }
    eprintln!("matched {} input files", files.len());
    let t0 = Instant::now();

    // Pass 1: rank by citations to assign doc IDs.
    let mut rows: Vec<(u64, i64)> = files.par_iter().flat_map_iter(|p| rank_file(p)).collect();
    eprintln!(
        "pass1: {} ranked rows in {:.1}s",
        rows.len(),
        t0.elapsed().as_secs_f64()
    );
    rows.par_sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
    let n = rows.len();
    if n == 0 {
        eprintln!("no works ranked");
        std::process::exit(1);
    }
    eprintln!("ranked {} works; top cited_by_count={}", n, rows[0].1);
    let id_to_doc: HashMap<u64, u32> = rows
        .iter()
        .enumerate()
        .map(|(i, (wid, _))| (*wid, i as u32))
        .collect();
    drop(rows);

    // Pass 2: tokenize + index + facets + records, fanned out across files.
    let t1 = Instant::now();
    let shards: Vec<Mutex<HashMap<u64, RoaringBitmap>>> = (0..KEY_SHARDS)
        .map(|_| Mutex::new(HashMap::new()))
        .collect();
    let facets: Vec<Mutex<HashMap<String, RoaringBitmap>>> = (0..FACET_FIELDS.len())
        .map(|_| Mutex::new(HashMap::new()))
        .collect();

    let per_file: Vec<Vec<(u32, Vec<u8>)>> = files
        .par_iter()
        .map(|p| build_file(p, &id_to_doc, &shards, &facets))
        .collect();
    let indexed: usize = per_file.iter().map(|v| v.len()).sum();
    eprintln!(
        "pass2: indexed {} works in {:.1}s",
        indexed,
        t1.elapsed().as_secs_f64()
    );

    // Place records into doc-ID order, then write the record store.
    let t2 = Instant::now();
    let mut records: Vec<Vec<u8>> = vec![Vec::new(); n];
    for fr in per_file {
        for (d, rec) in fr {
            records[d as usize] = rec;
        }
    }
    {
        let bin = BufWriter::with_capacity(1 << 20, File::create(&bin_path).expect("create bin"));
        let idx = BufWriter::with_capacity(1 << 20, File::create(&idx_path).expect("create idx"));
        write_records(bin, idx, &records).expect("write records");
    }
    eprintln!(
        "wrote record store {} (+{}) in {:.1}s",
        bin_path,
        idx_path,
        t2.elapsed().as_secs_f64()
    );
    drop(records);

    // Split each posting head/tail (parallel across shards) and write the RRS.
    let t3 = Instant::now();
    let entries: Vec<(u64, Vec<u8>, Vec<u8>)> = shards
        .into_par_iter()
        .flat_map_iter(|m| {
            let map = m.into_inner().unwrap();
            map.into_iter()
                .map(|(k, bm)| {
                    let (h, t) = split_posting(&bm);
                    (k, h, t)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let ngrams = entries.len();
    {
        let out = BufWriter::with_capacity(1 << 20, File::create(&rrs_path).expect("create rrs"));
        write_rrs(out, GRAM as u16, DEFAULT_STRIDE, entries).expect("write rrs");
    }
    eprintln!(
        "wrote RRS {} ({} ngrams, {} bytes) in {:.1}s",
        rrs_path,
        ngrams,
        file_len(&rrs_path),
        t3.elapsed().as_secs_f64()
    );

    // Facets.
    let t4 = Instant::now();
    let fields_out: Vec<FacetFieldOut> = facets
        .into_iter()
        .enumerate()
        .map(|(fi, m)| {
            let map = m.into_inner().unwrap();
            let cats = map
                .into_iter()
                .map(|(val, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm);
                    FacetCatOut {
                        name: val,
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            FacetFieldOut {
                name: FACET_FIELDS[fi].to_string(),
                cats,
            }
        })
        .collect();
    {
        let out =
            BufWriter::with_capacity(1 << 20, File::create(&facets_path).expect("create facets"));
        write_rrsf(out, fields_out).expect("write facets");
    }
    eprintln!(
        "wrote facets {} ({} bytes) in {:.1}s",
        facets_path,
        file_len(&facets_path),
        t4.elapsed().as_secs_f64()
    );

    eprintln!(
        "DONE: {} docs in {:.1}s total",
        n,
        t0.elapsed().as_secs_f64()
    );
}

/// Streams one file for pass 1, returning `(wid, cited_by_count)` per indexable
/// work (titled, with a parseable id).
fn rank_file(path: &Path) -> Vec<(u64, i64)> {
    let mut out = Vec::new();
    let reader = match open_gz(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip {}: {e}", path.display());
            return out;
        }
    };
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<RankRow>(&line) {
            if r.display_name.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            if let Some(wid) = parse_wid(&r.id) {
                out.push((wid, r.cited_by_count.unwrap_or(0)));
            }
        }
    }
    out
}

/// Streams one file for pass 2: tokenizes each work and inserts its doc ID into
/// the shared sharded bitmaps + facet bitmaps (one lock per touched shard/field),
/// returning the file's `(docID, record bytes)` pairs.
fn build_file(
    path: &Path,
    id_to_doc: &HashMap<u64, u32>,
    shards: &[Mutex<HashMap<u64, RoaringBitmap>>],
    facets: &[Mutex<HashMap<String, RoaringBitmap>>],
) -> Vec<(u32, Vec<u8>)> {
    let mut recs = Vec::new();
    let reader = match open_gz(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip {}: {e}", path.display());
            return recs;
        }
    };
    // Per-shard key buckets, reused across this file's works to batch lock acquisitions.
    let mut buckets: Vec<Vec<u64>> = (0..KEY_SHARDS).map(|_| Vec::new()).collect();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        let w: Work = match serde_json::from_str(&line) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let title = w.display_name.as_deref().unwrap_or("");
        if title.is_empty() {
            continue;
        }
        let docid = match parse_wid(&w.id).and_then(|wid| id_to_doc.get(&wid)) {
            Some(d) => *d,
            None => continue,
        };

        let abstract_ = reconstruct_abstract(&w.abstract_inverted_index);
        let authors = author_names(&w);
        let venue = w
            .primary_location
            .as_ref()
            .and_then(|p| p.source.as_ref())
            .and_then(|s| s.display_name.as_deref())
            .unwrap_or("");
        let topic = topic_name(&w);
        let text = build_text(title, &abstract_, &authors, venue);

        // Tokenize, bucket keys by shard, then insert each shard's keys under one lock.
        let keys = ngram_keys(&text, GRAM);
        for b in buckets.iter_mut() {
            b.clear();
        }
        for &k in &keys {
            buckets[(k as usize) % KEY_SHARDS].push(k);
        }
        for (si, ks) in buckets.iter().enumerate() {
            if ks.is_empty() {
                continue;
            }
            let mut m = shards[si].lock().unwrap();
            for &k in ks {
                m.entry(k).or_default().insert(docid);
            }
        }

        // Facets: one bitmap per (field, value).
        for (fi, map) in facets.iter().enumerate() {
            let v = facet_value(&w, fi, &topic);
            if v.is_empty() {
                continue;
            }
            map.lock().unwrap().entry(v).or_default().insert(docid);
        }

        let id_trim = trim_openalex_id(&w.id);
        recs.push((
            docid,
            build_record(
                id_trim,
                title,
                &authors,
                w.publication_year.unwrap_or(0),
                venue,
                w.cited_by_count.unwrap_or(0),
            ),
        ));
    }
    recs
}

/// Opens a gzipped JSON-Lines file as a buffered line reader (multi-member gzip,
/// matching Go's default multistream behavior).
fn open_gz(path: &Path) -> std::io::Result<BufReader<MultiGzDecoder<BufReader<File>>>> {
    let f = File::open(path)?;
    let gz = MultiGzDecoder::new(BufReader::with_capacity(1 << 20, f));
    Ok(BufReader::with_capacity(1 << 20, gz))
}

/// Parses the numeric tail of an OpenAlex work id
/// ("https://openalex.org/W2741809807" -> 2741809807), a compact, stable key
/// across the two passes.
fn parse_wid(id: &str) -> Option<u64> {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let bytes = tail.as_bytes();
    if bytes.len() < 2 || (bytes[0] != b'W' && bytes[0] != b'w') {
        return None;
    }
    tail[1..].parse::<u64>().ok()
}

/// Drops the URL prefix, keeping the "W..." id stored in the record.
fn trim_openalex_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Joins authorship display names with "; ".
fn author_names(w: &Work) -> String {
    let names: Vec<&str> = w
        .authorships
        .iter()
        .filter_map(|a| a.author.as_ref().and_then(|x| x.display_name.as_deref()))
        .filter(|n| !n.is_empty())
        .collect();
    names.join("; ")
}

/// Primary topic display name, falling back to the first concept's.
fn topic_name(w: &Work) -> String {
    if let Some(n) = w
        .primary_topic
        .as_ref()
        .and_then(|t| t.display_name.as_deref())
    {
        if !n.is_empty() {
            return n.to_string();
        }
    }
    w.concepts
        .first()
        .and_then(|c| c.display_name.as_deref())
        .unwrap_or("")
        .to_string()
}

/// Rebuilds abstract text from OpenAlex's inverted index (word -> positions),
/// capped at `ABSTRACT_CHAR_CAP` bytes on a char boundary.
fn reconstruct_abstract(idx: &Option<HashMap<String, Vec<i64>>>) -> String {
    let idx = match idx {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };
    let max_pos = idx
        .values()
        .flat_map(|ps| ps.iter().copied())
        .max()
        .unwrap_or(-1);
    if max_pos < 0 {
        return String::new();
    }
    let mut words: Vec<&str> = vec![""; (max_pos as usize) + 1];
    for (word, ps) in idx {
        for &p in ps {
            if p >= 0 && (p as usize) < words.len() {
                words[p as usize] = word.as_str();
            }
        }
    }
    let mut s = words.join(" ");
    if s.len() > ABSTRACT_CHAR_CAP {
        let mut end = ABSTRACT_CHAR_CAP;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Indexed text: title, then any non-empty abstract/authors/venue (space-joined).
fn build_text(title: &str, abstract_: &str, authors: &str, venue: &str) -> String {
    let mut s =
        String::with_capacity(title.len() + abstract_.len() + authors.len() + venue.len() + 3);
    s.push_str(title);
    for f in [abstract_, authors, venue] {
        if !f.is_empty() {
            s.push(' ');
            s.push_str(f);
        }
    }
    s
}

/// The work's value for facet field `fi` ("" = omit).
fn facet_value(w: &Work, fi: usize, topic: &str) -> String {
    match fi {
        0 => match w.publication_year {
            Some(y) if y != 0 => y.to_string(),
            _ => String::new(),
        },
        1 => w.work_type.clone().unwrap_or_default(),
        2 => w
            .open_access
            .as_ref()
            .and_then(|o| o.oa_status.clone())
            .unwrap_or_default(),
        3 => w.language.clone().unwrap_or_default(),
        _ => topic.to_string(),
    }
}

/// Marshals the stored record JSON (compact keys: id, t, a, y, v, c) with the
/// same omit-empty rules as the Go loader.
fn build_record(
    id: &str,
    title: &str,
    authors: &str,
    year: i64,
    venue: &str,
    cited: i64,
) -> Vec<u8> {
    let mut s = String::with_capacity(160);
    s.push_str("{\"id\":");
    s.push_str(&json_str(id));
    s.push_str(",\"t\":");
    s.push_str(&json_str(title));
    if !authors.is_empty() {
        s.push_str(",\"a\":");
        s.push_str(&json_str(authors));
    }
    if year != 0 {
        s.push_str(",\"y\":");
        s.push_str(&year.to_string());
    }
    if !venue.is_empty() {
        s.push_str(",\"v\":");
        s.push_str(&json_str(venue));
    }
    s.push_str(",\"c\":");
    s.push_str(&cited.to_string());
    s.push('}');
    s.into_bytes()
}

/// JSON-encodes a string (quoted + escaped) via serde_json.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Returns the value following `flag` in `args`, or `default`.
fn arg(args: &[String], flag: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// File size in bytes, or 0 if it can't be stat'd.
fn file_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
