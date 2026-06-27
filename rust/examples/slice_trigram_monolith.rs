//! Slices a monolithic v3 `.rrs` (trigram) into an `RRSS` **trigram** split set by doc-ID
//! range — the trigram sibling of `slice_term_monolith`, and the fast path to a geometric
//! split set when the monolith already exists. The shared rank-ordered doc-ID space means a
//! split's posting for a trigram is exactly the monolith posting restricted to `[lo, hi)`,
//! rebased to local IDs; and the monolith's dictionary + postings regions are laid out in
//! trigram-key order, so the whole slice is ONE sequential read of the `.rrs` fanned into N
//! streaming split writers. ~Disk speed, versus days re-tokenizing 484M records (the
//! `SplitSetBuilder`'s open-split map degrades badly at multi-GiB caps).
//!
//! Ranges are geometric in doc count: `base_docs`, doubling per tier, capped at `max_docs` —
//! the doc-count mirror of the builders' `byte_cap_max` doubling (bytes per doc is roughly
//! uniform, so split sizes follow). The last split takes the remainder. Trigram postings run
//! ~2× the bytes/doc of the term index, so the doc cap is half the term slicer's to keep each
//! split under the same ~8 GiB ceiling.
//!
//!   cargo run --release --features "splits" --example slice_trigram_monolith -- \
//!     MONO.rrs OUT_DIR PREFIX [base_docs=2000000] [max_docs=32000000] [n_docs=484369476]
//!   cargo run --release --features "splits" --example slice_trigram_monolith -- --selftest
//!
//! `--selftest` builds a synthetic corpus through `SplitSetBuilder` AND through
//! monolith-build → slice (using the builder's own doc ranges), and asserts the two split
//! sets are byte-identical — manifest and every split.

use roaring::RoaringBitmap;
use roaringrange::build::serialize_posting;
use roaringrange::{write_splitset, BodyKind, Policy, SplitSetConfig, SplitSpec};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// v3 `RRS` header size and version — hardcoded to match `build::write_index` (the inline
/// `RrsStreamWriter` reproduces its byte layout; the selftest pins the two together).
const RRS_HEADER_SIZE: usize = 16;
const RRS_FORMAT_VERSION: u16 = 3;
/// Default sparse-index stride (matches `build::DEFAULT_STRIDE`).
const DEFAULT_STRIDE: u32 = 512;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--selftest") {
        selftest();
        return;
    }
    if args.len() < 3 {
        eprintln!(
            "usage: slice_trigram_monolith MONO.rrs OUT_DIR PREFIX [base_docs=2000000] [max_docs=32000000] [n_docs=484369476]\n       slice_trigram_monolith --selftest"
        );
        std::process::exit(2);
    }
    let mono = &args[0];
    let out_dir = PathBuf::from(&args[1]);
    let prefix = args[2].clone();
    let base_docs: u32 = args
        .get(3)
        .map(|s| s.parse().expect("base_docs"))
        .unwrap_or(2_000_000);
    let max_docs: u32 = args
        .get(4)
        .map(|s| s.parse().expect("max_docs"))
        .unwrap_or(32_000_000);
    // The monolith records no doc count (only its trigram count), and the ranges must be
    // FIXED up front, so the corpus size is passed in. Default to the OpenAlex full corpus.
    let n_docs: u32 = args
        .get(5)
        .map(|s| s.parse().expect("n_docs"))
        .unwrap_or(484_369_476);
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let ranges = geometric_ranges(n_docs, base_docs, max_docs);
    eprintln!(
        "slicing {mono} into {} splits: {:?}",
        ranges.len(),
        ranges.iter().map(|&(lo, hi)| hi - lo).collect::<Vec<_>>()
    );
    let t0 = Instant::now();
    slice(Path::new(mono), &out_dir, &prefix, &ranges, 0).expect("slice");
    eprintln!("done in {:.0}s", t0.elapsed().as_secs_f64());
}

/// Doubling doc-count ranges: `base`, `2·base`, …, capped at `max`, last takes the remainder.
fn geometric_ranges(n_docs: u32, base: u32, max: u32) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut lo: u32 = 0;
    let mut size: u64 = base as u64;
    while lo < n_docs {
        let hi = (lo as u64 + size).min(n_docs as u64) as u32;
        ranges.push((lo, hi));
        lo = hi;
        size = (size * 2).min(max as u64);
    }
    ranges
}

/// Streaming writer for a v3 `RRS` index: posting bytes go to the wrapped writer (a region
/// temp file) as they arrive in key order, while the dictionary `(key, offset, size)` accrues
/// in memory. [`finish_meta`](Self::finish_meta) emits the header + sparse index + dictionary
/// once the dict is complete; the caller concatenates `meta || region` into the final `.rrs`.
/// Byte-identical to `build::write_index` given the same key-sorted entries — the trigram
/// analog of `TermIndexStreamWriter`.
struct RrsStreamWriter<W: Write> {
    w: W,
    gram_size: u16,
    stride: u32,
    region_len: u64,
    /// `(key, region-relative posting offset, posting size)`, in push (ascending-key) order.
    dict: Vec<(u64, u64, u32)>,
}

impl<W: Write> RrsStreamWriter<W> {
    fn new(w: W, gram_size: u16, stride: u32) -> Self {
        RrsStreamWriter {
            w,
            gram_size,
            stride: if stride == 0 { DEFAULT_STRIDE } else { stride },
            region_len: 0,
            dict: Vec::new(),
        }
    }

    /// Appends one trigram key's posting; must be called in strictly ascending key order so the
    /// dictionary and sparse index come out sorted (the monolith dict is already key-sorted).
    fn push(&mut self, key: u64, posting: &[u8]) -> io::Result<()> {
        self.w.write_all(posting)?;
        self.dict.push((key, self.region_len, posting.len() as u32));
        self.region_len += posting.len() as u64;
        Ok(())
    }

    /// Region (posting) bytes streamed so far — the final split's posting-region length.
    fn region_len(&self) -> u64 {
        self.region_len
    }

    /// Flushes the region writer and returns the header + sparse index + dictionary bytes that
    /// precede the posting region in the final `.rrs`. Dictionary offsets are made absolute by
    /// adding the now-known `postings_start`.
    fn finish_meta(mut self) -> io::Result<Vec<u8>> {
        self.w.flush()?;
        let n = self.dict.len();
        let sparse_count = if n == 0 {
            0
        } else {
            n.div_ceil(self.stride as usize)
        };
        let dict_start = RRS_HEADER_SIZE + sparse_count * 8;
        let postings_start = (dict_start + n * 20) as u64;

        let mut meta = Vec::with_capacity(postings_start as usize);
        meta.extend_from_slice(b"RRSI");
        meta.extend_from_slice(&RRS_FORMAT_VERSION.to_le_bytes());
        meta.extend_from_slice(&self.gram_size.to_le_bytes());
        meta.extend_from_slice(&(n as u32).to_le_bytes());
        meta.extend_from_slice(&self.stride.to_le_bytes());
        for i in 0..sparse_count {
            meta.extend_from_slice(&self.dict[i * self.stride as usize].0.to_le_bytes());
        }
        for (key, rel_off, size) in &self.dict {
            meta.extend_from_slice(&key.to_le_bytes());
            meta.extend_from_slice(&(postings_start + rel_off).to_le_bytes());
            meta.extend_from_slice(&size.to_le_bytes());
        }
        Ok(meta)
    }
}

/// One split under construction: its streaming `RRS` writer over a region temp file.
struct SplitSink {
    writer: RrsStreamWriter<BufWriter<File>>,
    region_path: PathBuf,
}

/// Slices the monolith at `mono` into one split per `ranges` entry plus the manifest, writing
/// `‹prefix›-sNNNNN.rrs` files and `‹prefix›.rrss` into `out_dir`. `byte_cap` is recorded in
/// the manifest (informational — the slicer ranges by doc count, not bytes).
fn slice(
    mono: &Path,
    out_dir: &Path,
    prefix: &str,
    ranges: &[(u32, u32)],
    byte_cap: u64,
) -> io::Result<()> {
    // v3 RRS header: magic, version, gram, ngrams, stride (16 B).
    let mut f = File::open(mono)?;
    let mut header = [0u8; RRS_HEADER_SIZE];
    f.read_exact(&mut header)?;
    if &header[0..4] != b"RRSI" {
        return Err(io::Error::other("not an RRS index (bad magic)"));
    }
    let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
    if version != RRS_FORMAT_VERSION {
        return Err(io::Error::other(format!(
            "expected RRS v{RRS_FORMAT_VERSION}, found v{version}"
        )));
    }
    let gram = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let ngrams = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let stride = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let sparse_count = if ngrams == 0 {
        0
    } else {
        (ngrams as usize).div_ceil(stride as usize)
    };
    let dict_start = (RRS_HEADER_SIZE + sparse_count * 8) as u64;
    let postings_start = dict_start + ngrams as u64 * 20;

    // Two sequential readers in lockstep: the dictionary (20 B entries) and the posting region.
    let mut dict_r = BufReader::with_capacity(4 << 20, File::open(mono)?);
    dict_r.seek(SeekFrom::Start(dict_start))?;
    let mut post_r = BufReader::with_capacity(32 << 20, File::open(mono)?);
    post_r.seek(SeekFrom::Start(postings_start))?;
    let mut post_pos: u64 = 0; // region-relative; cross-checked against each dict offset

    let mut sinks: Vec<SplitSink> = ranges
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let region_path = out_dir.join(format!("{prefix}-s{i:05}.region.tmp"));
            let w = BufWriter::with_capacity(8 << 20, File::create(&region_path)?);
            Ok(SplitSink {
                writer: RrsStreamWriter::new(w, gram, stride),
                region_path,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    let t0 = Instant::now();
    let mut entry = [0u8; 20];
    for n in 0..ngrams {
        dict_r.read_exact(&mut entry)?;
        let key = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let abs_off = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        let size = u32::from_le_bytes(entry[16..20].try_into().unwrap()) as usize;
        if abs_off != postings_start + post_pos {
            return Err(io::Error::other(format!(
                "postings lockstep diverged at key {key} (#{n}): dict says {abs_off}, stream is at {}",
                postings_start + post_pos
            )));
        }
        let mut posting = vec![0u8; size];
        post_r.read_exact(&mut posting)?;
        post_pos += size as u64;

        let full = RoaringBitmap::deserialize_from(&posting[..])
            .map_err(|e| io::Error::other(format!("posting #{n}: {e}")))?;
        for (si, &(lo, hi)) in ranges.iter().enumerate() {
            // Cheap intersect check before materializing the slice.
            if full.range_cardinality(lo..hi) == 0 {
                continue;
            }
            let local = RoaringBitmap::from_sorted_iter(full.range(lo..hi).map(|d| d - lo))
                .expect("sorted local ids");
            sinks[si].writer.push(key, &serialize_posting(&local))?;
        }
        if (n as u64 + 1).is_multiple_of(20_000_000) {
            eprintln!(
                "  {}/{ngrams} trigrams in {:.0}s",
                n + 1,
                t0.elapsed().as_secs_f64()
            );
        }
    }

    // Assemble each split (meta header + streamed posting region) and collect manifest entries.
    let mut specs = Vec::with_capacity(ranges.len());
    for (i, (sink, &(lo, hi))) in sinks.into_iter().zip(ranges).enumerate() {
        let region_len = sink.writer.region_len();
        let meta = sink.writer.finish_meta()?;
        let actual = std::fs::metadata(&sink.region_path)?.len();
        if actual != region_len {
            return Err(io::Error::other(format!(
                "split {i}: region temp file is {actual} B, writer streamed {region_len} B"
            )));
        }
        let name = format!("{prefix}-s{i:05}.rrs");
        let mut out = BufWriter::new(File::create(out_dir.join(&name))?);
        out.write_all(&meta)?;
        io::copy(&mut File::open(&sink.region_path)?, &mut out)?;
        out.flush()?;
        std::fs::remove_file(&sink.region_path)?;
        let byte_size = std::fs::metadata(out_dir.join(&name))?.len();
        eprintln!(
            "  split {i}: docs [{lo}, {hi}) -> {name} ({:.1} MB)",
            byte_size as f64 / (1024.0 * 1024.0)
        );
        specs.push(SplitSpec {
            data_file: name,
            tier: i.min(u16::MAX as usize) as u16,
            doc_count: hi - lo,
            doc_id_lo: lo,
            doc_id_hi: hi - 1,
            epoch: 0,
            byte_size,
            flags: 0,
            summary: Vec::new(),
        });
    }

    let config = SplitSetConfig {
        policy: Policy::Tiered,
        tier_count: specs.len().min(u16::MAX as usize) as u16,
        base_count: specs.len() as u32,
        byte_cap,
        gram_size: gram,
        body_kind: BodyKind::Trigram,
        sortcol: None,
        flags: 0,
    };
    let mut manifest = BufWriter::new(File::create(out_dir.join(format!("{prefix}.rrss")))?);
    write_splitset(&mut manifest, &specs, &config)?;
    manifest.flush()
}

/// Builds a synthetic corpus through `SplitSetBuilder` AND through monolith-build → slice with
/// the builder's own doc ranges, asserting the two split sets are byte-identical (manifest +
/// every split). Bloom is disabled and no facets are added so the builder's manifest carries
/// empty per-split summaries — exactly what the slicer emits.
fn selftest() {
    use roaringrange::build::write_index;
    use roaringrange::{ngram_keys, SplitBuildConfig, SplitSet, SplitSetBuilder};
    let dir = std::env::temp_dir().join(format!("slice-tri-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("selftest dir");

    let docs: Vec<String> = (0..5_000)
        .map(|i| {
            let mut s = String::new();
            for j in 0..(i % 7 + 1) {
                s.push_str(&format!("word{} ", (i * 3 + j) % 211));
            }
            s.push_str("common shared text");
            s
        })
        .collect();

    // Path A: the greedy builder (no bloom, no facets, flat cap → several splits).
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap: 20_000,
        byte_cap_max: 0,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "tg".to_string(),
        sortcol: None,
        bloom_bits_per_key: 0,
        case_sensitive: false,
    });
    for d in &docs {
        b.add_text(d).unwrap();
    }
    let built = b.finish().unwrap();

    // The builder's ranges, recovered from its own manifest.
    let ss = SplitSet::from_bytes(&built.manifest).expect("parse manifest");
    let ranges: Vec<(u32, u32)> = ss
        .splits()
        .iter()
        .map(|s| (s.doc_id_lo, s.doc_id_hi + 1))
        .collect();

    // Path B: a v3 RRS monolith over the whole corpus, then slice with those exact ranges.
    let mut postings: std::collections::BTreeMap<u64, RoaringBitmap> =
        std::collections::BTreeMap::new();
    for (i, d) in docs.iter().enumerate() {
        for k in ngram_keys(d, 3) {
            postings.entry(k).or_default().insert(i as u32);
        }
    }
    let entries: Vec<(u64, Vec<u8>)> = postings
        .iter()
        .map(|(k, bm)| (*k, serialize_posting(bm)))
        .collect();
    let mono_path = dir.join("mono.rrs");
    write_index(
        BufWriter::new(File::create(&mono_path).unwrap()),
        3,
        0,
        entries,
    )
    .unwrap();
    slice(&mono_path, &dir, "tg", &ranges, 20_000).expect("slice");

    let manifest_b = std::fs::read(dir.join("tg.rrss")).unwrap();
    assert_eq!(
        built.manifest, manifest_b,
        "sliced manifest differs from the builder's"
    );
    for (name, bytes) in &built.splits {
        let sliced = std::fs::read(dir.join(name)).unwrap();
        assert_eq!(bytes, &sliced, "{name} differs between builder and slicer");
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "selftest OK: {} splits + manifest byte-identical across builder and slicer",
        built.splits.len()
    );
}
