//! Head-size tuning analysis (`-analyze-head`).
//!
//! Given a finished primary `.rrs`, sweeps candidate head boundaries `B` and
//! reports, per `B`, the fraction of a query workload whose first page (top
//! `-limit` results) is served from the head postings alone, plus the modeled
//! request latency on a slow-mobile profile — so the head/tail split point can be
//! chosen from data instead of a guessed constant.
//!
//! The analysis reads only the finished index: because doc IDs equal rank and the
//! stored head∪tail reconstructs each trigram's full posting, every candidate `B`
//! is evaluated from the one built index with no rebuild (see [`crate::secondary`]
//! for the matching transcode that actually emits a new-`B` index).
//!
//! Cost model — it mirrors the reader's [`crate::index::Index::search`] waves:
//!   * **dict wave** — one ranged read per distinct trigram for its dictionary
//!     block (`stride` entries × 24 B); always paid.
//!   * **head wave** — every trigram's head posting (`full ∩ [0,B)`), intersected.
//!     If at least `limit` matches fall below `B` the query is *head-sufficient*
//!     and stops here (2 round-trip waves).
//!   * **tail wave** — only when the head under-fills: the rarest tail in full,
//!     then the others at container granularity (mirrors
//!     [`crate::posting::tail_intersect_and`]). Adds a 3rd wave and a large
//!     transfer.
//!
//! `E[latency](B) ≈ waves(B)·RTT + bytes(B)/bandwidth`, aggregated over the
//! workload; the optimum `B` is the minimum.

use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::ngram_keys;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::time::Instant;
use tracing::{info, info_span, warn};

/// Dictionary entry: key(8) + headOffset(8) + headSize(4) + tailSize(4).
const DICT_ENTRY: usize = 24;
/// Bytes read up front per non-seed tail posting for its header (mirrors `posting.rs`).
const HEADER_PREFIX: usize = 4096;

/// Candidate head boundaries to sweep — multiples of the container size, 64K…16M.
const CANDIDATES: [u32; 9] = [
    65536,   // 64K (current)
    131072,  // 128K
    262144,  // 256K
    524288,  // 512K
    1 << 20, // 1M
    1 << 21, // 2M
    1 << 22, // 4M
    1 << 23, // 8M
    1 << 24, // 16M
];

/// Slow-mobile latency profile and page depth, overridable from the CLI.
struct Profile {
    /// Round-trip time per request wave (ms).
    rtt_ms: f64,
    /// Downlink bandwidth (bits/s).
    bw_bps: f64,
    /// First-page depth the head must satisfy.
    limit: usize,
}

impl Default for Profile {
    fn default() -> Self {
        // Slow-mobile: ~200 ms RTT, ~2 Mbps down, one page of 25.
        Profile {
            rtt_ms: 200.0,
            bw_bps: 2_000_000.0,
            limit: 25,
        }
    }
}

/// Sync, range-reading reader for a finished `.rrs`. Holds the header fields and
/// the in-memory sparse index; postings are read on demand. Mirrors the byte
/// layout written by [`roaringrange::build::chunk::merge_partials_to_rrs`].
struct RrsReader {
    file: File,
    gram_size: usize,
    ngrams: usize,
    stride: usize,
    dict_start: u64,
    /// `sparse[i]` is the key of dictionary entry `i·stride`.
    sparse: Vec<u64>,
}

/// A located dictionary entry: where the head/tail bytes live and how big the
/// dict block was that we had to read to find it.
struct PostingLoc {
    head_offset: u64,
    head_size: u32,
    tail_size: u32,
    /// Bytes the reader fetches for this key's dict block (block entries × 24).
    dict_block_bytes: usize,
}

impl RrsReader {
    fn open(path: &str) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hdr = [0u8; 16];
        file.read_exact(&mut hdr)?;
        if &hdr[0..4] != b"RRSI" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "rrs: bad magic"));
        }
        let version = u16::from_le_bytes(hdr[4..6].try_into().unwrap());
        let gram_size = u16::from_le_bytes(hdr[6..8].try_into().unwrap()) as usize;
        let ngrams = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
        let stride = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        // Version 2 adds a 4-byte head_boundary field; consume it before the sparse
        // index so both formats read correctly.
        let header_size: u64 = if version >= 2 {
            let mut hb = [0u8; 4];
            file.read_exact(&mut hb)?;
            20
        } else {
            16
        };
        let sparse_count = if ngrams == 0 {
            0
        } else {
            ngrams.div_ceil(stride)
        };
        let mut sbuf = vec![0u8; sparse_count * 8];
        file.read_exact(&mut sbuf)?;
        let sparse = sbuf
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let dict_start = header_size + (sparse_count as u64) * 8;
        Ok(RrsReader {
            file,
            gram_size,
            ngrams,
            stride,
            dict_start,
            sparse,
        })
    }

    /// Locates a key's dictionary entry by reading the one sparse block that may
    /// contain it (≤ `stride` entries) and scanning for an exact match.
    fn locate(&self, key: u64) -> io::Result<Option<PostingLoc>> {
        if self.ngrams == 0 || key < self.sparse[0] {
            return Ok(None);
        }
        // Largest block whose first key is ≤ `key`.
        let blk = match self.sparse.binary_search(&key) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = blk * self.stride;
        let entries = self.stride.min(self.ngrams - start);
        let mut buf = vec![0u8; entries * DICT_ENTRY];
        // Positioned read (no shared cursor) so locate/full_posting are callable
        // from parallel threads on a shared &RrsReader.
        self.file
            .read_exact_at(&mut buf, self.dict_start + (start * DICT_ENTRY) as u64)?;
        let dict_block_bytes = buf.len();
        for e in buf.chunks_exact(DICT_ENTRY) {
            let k = u64::from_le_bytes(e[0..8].try_into().unwrap());
            if k == key {
                return Ok(Some(PostingLoc {
                    head_offset: u64::from_le_bytes(e[8..16].try_into().unwrap()),
                    head_size: u32::from_le_bytes(e[16..20].try_into().unwrap()),
                    tail_size: u32::from_le_bytes(e[20..24].try_into().unwrap()),
                    dict_block_bytes,
                }));
            }
            if k > key {
                break;
            }
        }
        Ok(None)
    }

    /// Reads and unions a key's head+tail into the full posting.
    fn full_posting(&self, loc: &PostingLoc) -> io::Result<RoaringBitmap> {
        let mut buf = vec![0u8; (loc.head_size + loc.tail_size) as usize];
        self.file.read_exact_at(&mut buf, loc.head_offset)?;
        let hs = loc.head_size as usize;
        let mut bm = RoaringBitmap::deserialize_from(&buf[..hs])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        bm |= RoaringBitmap::deserialize_from(&buf[hs..])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(bm)
    }
}

/// One distinct trigram's loaded data: the full posting (for the per-query
/// intersection and selective-tail model) plus the dict-block bytes the reader
/// pays to find it and the precomputed serialized head/tail sizes at every
/// candidate boundary. Built once per trigram in a parallel pre-pass, then shared
/// read-only across the eval threads.
struct Entry {
    full: RoaringBitmap,
    dict_block_bytes: usize,
    /// `head_sizes[i]` = serialized bytes of `full ∩ [0, CANDIDATES[i])`.
    head_sizes: Vec<usize>,
    /// `tail_sizes[i]` = serialized bytes of `full ∩ [CANDIDATES[i], ∞)`.
    tail_sizes: Vec<usize>,
}

/// Serialized size of `full ∩ [0, b)` for every candidate `b`: one clone, then a
/// descending progressive trim (each step removes only the slice above the
/// next-smaller boundary).
fn prefix_sizes(full: &RoaringBitmap) -> Vec<usize> {
    let mut h = full.clone();
    let mut sizes = vec![0usize; CANDIDATES.len()];
    for i in (0..CANDIDATES.len()).rev() {
        h.remove_range(CANDIDATES[i]..);
        sizes[i] = h.serialized_size();
    }
    sizes
}

/// Serialized size of `full ∩ [b, ∞)` for every candidate `b`: one clone with an
/// ascending progressive trim of the low end.
fn suffix_sizes(full: &RoaringBitmap) -> Vec<usize> {
    let mut t = full.clone();
    let mut sizes = vec![0usize; CANDIDATES.len()];
    for i in 0..CANDIDATES.len() {
        t.remove_range(0..CANDIDATES[i]);
        sizes[i] = t.serialized_size();
    }
    sizes
}

/// Distinct container high keys (`doc >> 16`) in `bm`, ascending. `bm` iterates
/// ascending, so adjacent dedup yields a sorted, unique list.
fn distinct_high_keys(bm: &RoaringBitmap) -> Vec<u16> {
    let mut keys = Vec::new();
    let mut last: Option<u16> = None;
    for d in bm {
        let k = (d >> 16) as u16;
        if last != Some(k) {
            keys.push(k);
            last = Some(k);
        }
    }
    keys
}

/// Serialized body bytes of one roaring container of cardinality `card`: an array
/// container packs 2 B/value below 4096, a bitmap container is a fixed 8 KB above;
/// plus a few bytes of descriptive/offset overhead.
fn container_bytes(card: u32) -> usize {
    let body = if card >= 4096 {
        8192
    } else {
        2 * card as usize
    };
    body + 4
}

/// Bytes the reader fetches for the tail AND at boundary `b`, mirroring
/// [`crate`]'s `posting::tail_intersect_and`: read the smallest (rarest) tail in
/// full, then each remaining tail only at the containers whose high key still
/// survives among the candidates. `tail_sizes[j][i]` is key `j`'s precomputed
/// serialized tail size at boundary index `i`; per-block cardinality of the larger
/// tails comes from `rank` so no large bitmap is iterated.
fn selective_tail(fulls: &[&RoaringBitmap], tail_sizes: &[&Vec<usize>], i: usize, b: u32) -> usize {
    if fulls.is_empty() {
        return 0;
    }
    let mut order: Vec<usize> = (0..fulls.len()).collect();
    order.sort_by_key(|&j| tail_sizes[j][i]);
    let seed = order[0];
    // If even the rarest tail is large, the selective walk would iterate a huge
    // bitmap to no useful end — the tail is expensive regardless, so fall back to the
    // (upper-bound) "fetch every tail" sum. This bounds per-query cost.
    if tail_sizes[seed][i] > 2_000_000 {
        return order.iter().map(|&j| tail_sizes[j][i]).sum();
    }
    let mut running = fulls[seed].clone();
    running.remove_range(0..b);
    let mut bytes = tail_sizes[seed][i]; // == running.serialized_size(), already known
    for &j in &order[1..] {
        if running.is_empty() {
            break;
        }
        bytes += HEADER_PREFIX;
        for hk in distinct_high_keys(&running) {
            let lo = (hk as u32) << 16;
            let hi = lo | 0xFFFF;
            let below_lo = if lo == 0 { 0 } else { fulls[j].rank(lo - 1) };
            let card = (fulls[j].rank(hi) - below_lo) as u32;
            if card > 0 {
                bytes += container_bytes(card);
            }
        }
        running &= fulls[j];
    }
    bytes
}

/// Per-(query, B) outcome.
#[derive(Clone)]
struct Cell {
    sufficient: bool,
    bytes: usize,
    waves: u32,
    latency_ms: f64,
}

/// Evaluates one query across all candidate boundaries. Returns `None` if any
/// trigram is absent (the strict AND is then empty — no matches, excluded from
/// the head-serve rate). Reuses `cache` so recurring trigrams are read once.
fn eval_text(
    text: &str,
    map: &HashMap<u64, Option<Entry>>,
    prof: &Profile,
    gram: usize,
) -> Option<Vec<Cell>> {
    let keys = ngram_keys(text, gram);
    if keys.is_empty() {
        return None;
    }
    // Resolve every trigram in the prebuilt read-only map; an absent one makes the
    // strict AND empty, so the query has no matches.
    let mut entries: Vec<&Entry> = Vec::with_capacity(keys.len());
    for k in &keys {
        match map.get(k) {
            Some(Some(e)) => entries.push(e),
            _ => return None,
        }
    }

    let nb = CANDIDATES.len();
    let mut dict_bytes = 0usize;
    let mut head_sum = vec![0usize; nb];
    let mut tail_sizes_refs: Vec<&Vec<usize>> = Vec::with_capacity(entries.len());
    let mut refs: Vec<&RoaringBitmap> = Vec::with_capacity(entries.len());
    for e in &entries {
        dict_bytes += e.dict_block_bytes;
        for i in 0..nb {
            head_sum[i] += e.head_sizes[i];
        }
        tail_sizes_refs.push(&e.tail_sizes);
        refs.push(&e.full);
    }

    // Full strict-AND intersection (smallest first); doc IDs are rank order, so
    // `rank(b-1)` counts matches in the head window `[0, b)`.
    let mut order: Vec<usize> = (0..refs.len()).collect();
    order.sort_by_key(|&i| refs[i].len());
    let mut acc = refs[order[0]].clone();
    for &i in &order[1..] {
        acc &= refs[i];
        if acc.is_empty() {
            break;
        }
    }

    let mut cells = Vec::with_capacity(nb);
    for (i, &b) in CANDIDATES.iter().enumerate() {
        let below = if b == 0 { 0 } else { acc.rank(b - 1) } as usize;
        let sufficient = below >= prof.limit;
        let (bytes, waves) = if sufficient {
            (dict_bytes + head_sum[i], 2)
        } else {
            let tail = selective_tail(&refs, &tail_sizes_refs, i, b);
            (dict_bytes + head_sum[i] + tail, 3)
        };
        let latency_ms = waves as f64 * prof.rtt_ms + (bytes as f64 * 8.0) / prof.bw_bps * 1000.0;
        cells.push(Cell {
            sufficient,
            bytes,
            waves,
            latency_ms,
        });
    }
    Some(cells)
}

/// Percentile (nearest-rank) of a slice; `xs` is sorted in place.
fn pct(xs: &mut [f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((p / 100.0) * (xs.len() as f64 - 1.0)).round() as usize;
    xs[idx]
}

/// A labeled query.
struct Query {
    text: String,
    class: String,
}

/// Loads queries from a `query<TAB>class` file, or returns a small curated set
/// spanning the popularity classes when no file is given.
fn load_queries(path: Option<&str>) -> io::Result<Vec<Query>> {
    if let Some(p) = path {
        let mut out = Vec::new();
        for line in BufReader::new(File::open(p)?).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (text, class) = match line.split_once('\t') {
                Some((t, c)) => (t.to_string(), c.to_string()),
                None => (line.to_string(), "unlabeled".to_string()),
            };
            out.push(Query { text, class });
        }
        return Ok(out);
    }
    let curated = [
        ("machine learning", "common"),
        ("cancer", "common"),
        ("climate change", "common"),
        ("covid", "common"),
        ("deep learning", "common"),
        ("graph neural network", "mid"),
        ("crispr cas9", "mid"),
        ("quantum computing", "mid"),
        ("transformer attention", "mid"),
        ("yoshua bengio", "author"),
        ("jennifer doudna", "author"),
        ("geoffrey hinton", "author"),
        ("attention is all you need", "title"),
        ("a survey of reinforcement learning", "title"),
        ("photocatalytic hydrogen evolution mxene", "rare"),
        ("enantioselective organocatalysis morita baylis", "rare"),
    ];
    Ok(curated
        .iter()
        .map(|(t, c)| Query {
            text: t.to_string(),
            class: c.to_string(),
        })
        .collect())
}

/// Runs the `-analyze-head` analysis. Flags: `-rrs <path>` (required),
/// `-queries <file>`, `-limit N`, `-rtt-ms F`, `-bw-kbps F`, `-out <csv>`.
pub fn run(args: &[String]) {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let rrs = match flag("-rrs") {
        Some(p) => p,
        None => {
            warn!("usage: -analyze-head -rrs <path> [-queries f] [-limit N] [-rtt-ms F] [-bw-kbps F] [-out csv]");
            std::process::exit(2);
        }
    };
    let mut prof = Profile::default();
    if let Some(v) = flag("-limit") {
        prof.limit = v.parse().expect("-limit");
    }
    if let Some(v) = flag("-rtt-ms") {
        prof.rtt_ms = v.parse().expect("-rtt-ms");
    }
    if let Some(v) = flag("-bw-kbps") {
        prof.bw_bps = v.parse::<f64>().expect("-bw-kbps") * 1000.0;
    }

    let span = info_span!("analyze-head").entered();
    let t0 = Instant::now();
    let reader = RrsReader::open(&rrs).expect("open rrs");
    info!(
        ngrams = reader.ngrams,
        gram = reader.gram_size,
        stride = reader.stride,
        "opened index"
    );
    let queries = load_queries(flag("-queries").as_deref()).expect("load queries");
    info!(
        queries = queries.len(),
        limit = prof.limit,
        rtt_ms = prof.rtt_ms,
        bw_kbps = prof.bw_bps / 1000.0,
        "workload loaded"
    );

    let gram = reader.gram_size;
    // Distinct trigrams across the whole workload.
    let distinct: Vec<u64> = {
        let mut s: HashSet<u64> = HashSet::new();
        for q in &queries {
            for k in ngram_keys(&q.text, gram) {
                s.insert(k);
            }
        }
        s.into_iter().collect()
    };
    // Parallel pre-pass: read + deserialize each distinct posting once and
    // precompute its per-boundary sizes. Built once, then read-only during eval (no
    // locks). `None` marks a trigram absent from the index.
    let t_pre = Instant::now();
    let map: HashMap<u64, Option<Entry>> = distinct
        .par_iter()
        .map(|&k| {
            let entry = match reader.locate(k) {
                Ok(Some(loc)) => reader.full_posting(&loc).ok().map(|full| Entry {
                    dict_block_bytes: loc.dict_block_bytes,
                    head_sizes: prefix_sizes(&full),
                    tail_sizes: suffix_sizes(&full),
                    full,
                }),
                _ => None,
            };
            (k, entry)
        })
        .collect();
    info!(
        distinct_trigrams = map.len(),
        present = map.values().filter(|v| v.is_some()).count(),
        prepass_s = t_pre.elapsed().as_secs_f64(),
        "postings loaded"
    );

    // Evaluate each DISTINCT query once (popular subjects repeat heavily), in
    // parallel, then expand back over the original lines so repeats frequency-weight
    // the aggregate.
    let nb = CANDIDATES.len();
    let unique: Vec<String> = {
        let mut s: HashSet<&str> = HashSet::new();
        for q in &queries {
            s.insert(q.text.as_str());
        }
        s.into_iter().map(str::to_string).collect()
    };
    let memo: HashMap<String, Option<Vec<Cell>>> = unique
        .par_iter()
        .map(|t| (t.clone(), eval_text(t, &map, &prof, gram)))
        .collect();
    let mut all_cells: Vec<(String, Vec<Cell>)> = Vec::with_capacity(queries.len());
    let mut empty = 0usize;
    for q in &queries {
        match memo.get(&q.text).and_then(|o| o.as_ref()) {
            Some(cells) => all_cells.push((q.class.clone(), cells.clone())),
            None => empty += 1,
        }
    }
    info!(
        matched = all_cells.len(),
        empty,
        unique_queries = unique.len(),
        elapsed_s = t0.elapsed().as_secs_f64(),
        "evaluated"
    );

    // Aggregate per boundary.
    println!("\nB\thead_serve%\tmean_kB\tp95_kB\tmean_ms\tp95_ms\tmean_waves");
    let mut csv = String::from("B,head_serve_pct,mean_kb,p95_kb,mean_ms,p95_ms,mean_waves\n");
    for (bi, &b) in CANDIDATES.iter().enumerate() {
        let n = all_cells.len().max(1);
        let served = all_cells.iter().filter(|(_, c)| c[bi].sufficient).count();
        let serve_pct = 100.0 * served as f64 / n as f64;
        let mut kbs: Vec<f64> = all_cells
            .iter()
            .map(|(_, c)| c[bi].bytes as f64 / 1024.0)
            .collect();
        let mut mss: Vec<f64> = all_cells.iter().map(|(_, c)| c[bi].latency_ms).collect();
        let mean_kb = kbs.iter().sum::<f64>() / n as f64;
        let mean_ms = mss.iter().sum::<f64>() / n as f64;
        let mean_waves = all_cells
            .iter()
            .map(|(_, c)| c[bi].waves as f64)
            .sum::<f64>()
            / n as f64;
        let p95_kb = pct(&mut kbs, 95.0);
        let p95_ms = pct(&mut mss, 95.0);
        println!(
            "{}\t{:.1}\t{:.1}\t{:.1}\t{:.0}\t{:.0}\t{:.2}",
            fmt_b(b),
            serve_pct,
            mean_kb,
            p95_kb,
            mean_ms,
            p95_ms,
            mean_waves
        );
        csv.push_str(&format!(
            "{},{:.2},{:.1},{:.1},{:.1},{:.1},{:.3}\n",
            b, serve_pct, mean_kb, p95_kb, mean_ms, p95_ms, mean_waves
        ));
    }

    // Knee: the boundary minimizing mean modeled latency.
    let best = (0..nb)
        .min_by(|&a, &b| {
            let la = all_cells.iter().map(|(_, c)| c[a].latency_ms).sum::<f64>();
            let lb = all_cells.iter().map(|(_, c)| c[b].latency_ms).sum::<f64>();
            la.partial_cmp(&lb).unwrap()
        })
        .unwrap_or(0);
    info!(
        best_boundary = fmt_b(CANDIDATES[best]),
        "min mean-latency boundary"
    );

    // Per-class head-serve at each boundary.
    println!("\nper-class head_serve% by B:");
    let mut classes: Vec<String> = all_cells.iter().map(|(c, _)| c.clone()).collect();
    classes.sort();
    classes.dedup();
    print!("class");
    for &b in &CANDIDATES {
        print!("\t{}", fmt_b(b));
    }
    println!();
    for cl in &classes {
        let rows: Vec<&Vec<Cell>> = all_cells
            .iter()
            .filter(|(c, _)| c == cl)
            .map(|(_, c)| c)
            .collect();
        print!("{}", cl);
        for bi in 0..nb {
            let served = rows.iter().filter(|c| c[bi].sufficient).count();
            print!("\t{:.0}%", 100.0 * served as f64 / rows.len().max(1) as f64);
        }
        println!();
    }

    if let Some(out) = flag("-out") {
        std::fs::write(&out, csv).expect("write csv");
        info!(path = %out, "wrote csv");
    }
    drop(span);
}

/// Formats a boundary as 64K / 1M etc.
fn fmt_b(b: u32) -> String {
    if b >= 1 << 20 {
        format!("{}M", b >> 20)
    } else {
        format!("{}K", b >> 10)
    }
}

// ----------------------------------------------------------------------------
// Workload generation (`-gen-workload`): derive a query set from the corpus's own
// text, since real searches are substrings of real documents.

/// Named entity with a display name (topic, concept, author, venue source).
#[derive(Deserialize)]
struct NamedLite {
    #[serde(default)]
    display_name: Option<String>,
}
#[derive(Deserialize)]
struct AuthorshipLite {
    #[serde(default)]
    author: Option<NamedLite>,
}
#[derive(Deserialize)]
struct LocationLite {
    #[serde(default)]
    source: Option<NamedLite>,
}

/// The fields a query can be sampled from, plus the citation count used to weight
/// sampling toward the papers people actually search for.
#[derive(Deserialize)]
struct WorkLite {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    authorships: Vec<AuthorshipLite>,
    #[serde(default)]
    primary_topic: Option<NamedLite>,
    #[serde(default)]
    concepts: Vec<NamedLite>,
    #[serde(default)]
    primary_location: Option<LocationLite>,
    #[serde(default)]
    cited_by_count: Option<i64>,
}

/// A deterministic contiguous word slice (≤ `want` words) of `title` for a
/// "partial title" query — the short fragment a user actually remembers rather
/// than the whole title. Start is chosen from a cheap hash so it varies per title
/// without an RNG. `None` if the title is too short.
fn partial_title(title: &str, want: usize) -> Option<String> {
    let words: Vec<&str> = title.split_whitespace().collect();
    if words.len() < 2 {
        return None;
    }
    let want = want.min(words.len());
    let h = title
        .bytes()
        .fold(0usize, |a, b| a.wrapping_mul(131).wrapping_add(b as usize));
    let start = if words.len() > want {
        h % (words.len() - want + 1)
    } else {
        0
    };
    Some(words[start..start + want].join(" "))
}

/// Pushes `(query, class)` candidates for one work into `out`. Mirrors the search
/// modes a scholarly UI exposes: full and partial titles, subject/topic labels,
/// the lead author, and the venue.
fn extract_queries(w: &WorkLite, out: &mut Vec<(String, String)>) {
    if let Some(t) = w.display_name.as_deref() {
        let t = t.trim();
        if t.split_whitespace().count() >= 2 {
            out.push((t.to_string(), "title".into()));
        }
        if let Some(p) = partial_title(t, 3) {
            out.push((p, "partial".into()));
        }
    }
    if let Some(topic) = w
        .primary_topic
        .as_ref()
        .and_then(|n| n.display_name.clone())
    {
        out.push((topic, "subject".into()));
    }
    for c in w.concepts.iter().take(2) {
        if let Some(name) = c.display_name.clone() {
            out.push((name, "subject".into()));
        }
    }
    if let Some(a) = w
        .authorships
        .first()
        .and_then(|a| a.author.as_ref())
        .and_then(|n| n.display_name.clone())
    {
        out.push((a, "author".into()));
    }
    if let Some(v) = w
        .primary_location
        .as_ref()
        .and_then(|l| l.source.as_ref())
        .and_then(|n| n.display_name.clone())
    {
        out.push((v, "venue".into()));
    }
}

/// Generates a labeled query workload from the source shards. Flags: `-in <glob>`
/// (required), `-out <file>` (required), `-docs N` (docs to sample, default
/// 20000), `-weight uniform|cited` (default `cited`), `-min-cites T` (cited mode
/// threshold, default 10).
///
/// `cited` mode keeps only works at/above the citation threshold, biasing the
/// sample toward papers people actually search; `uniform` keeps every work and so
/// skews toward the (uncited) long tail. Running both brackets the result —
/// uniform is the pessimistic (deep-result) end, cited the realistic end. Repeated
/// subjects across works naturally frequency-weight popular topics.
pub fn gen_workload(args: &[String]) {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let glob_pat = flag("-in").expect("-in <shards glob> required");
    let out_path = flag("-out").expect("-out <file> required");
    let docs_target: usize = flag("-docs").map(|s| s.parse().unwrap()).unwrap_or(20_000);
    let cited = flag("-weight").as_deref() != Some("uniform");
    let min_cites: i64 = flag("-min-cites").map(|s| s.parse().unwrap()).unwrap_or(10);

    let _span = info_span!("gen-workload").entered();
    let t0 = Instant::now();
    let mut queries: Vec<(String, String)> = Vec::new();
    let mut sampled = 0usize;
    let mut scanned = 0usize;
    'outer: for entry in glob::glob(&glob_pat).expect("bad glob") {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let rdr = BufReader::new(MultiGzDecoder::new(f));
        for line in rdr.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.is_empty() {
                continue;
            }
            scanned += 1;
            let w: WorkLite = match serde_json::from_str(&line) {
                Ok(w) => w,
                Err(_) => continue,
            };
            if cited && w.cited_by_count.unwrap_or(0) < min_cites {
                continue;
            }
            extract_queries(&w, &mut queries);
            sampled += 1;
            if sampled >= docs_target {
                break 'outer;
            }
        }
    }

    let mut by_class: HashMap<&str, usize> = HashMap::new();
    let mut buf = String::new();
    for (q, c) in &queries {
        let q = q.replace(['\t', '\n'], " ");
        let q = q.trim();
        if q.is_empty() {
            continue;
        }
        *by_class.entry(c.as_str()).or_default() += 1;
        buf.push_str(q);
        buf.push('\t');
        buf.push_str(c);
        buf.push('\n');
    }
    File::create(&out_path)
        .and_then(|mut f| f.write_all(buf.as_bytes()))
        .expect("write workload");
    let mut classes: Vec<(&&str, &usize)> = by_class.iter().collect();
    classes.sort();
    info!(
        weight = if cited { "cited" } else { "uniform" },
        docs_sampled = sampled,
        docs_scanned = scanned,
        queries = queries.len(),
        per_class = ?classes,
        out = %out_path,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "workload generated"
    );
}
