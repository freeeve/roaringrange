//! Phased, resumable chunked builder.
//!
//! Splits the chunked build into independent **phases**, each writing one output
//! and skipping itself when that output already exists — so a crash or stop is
//! resumed by re-running with the same arguments, picking up where it left off
//! instead of rebuilding from scratch. Resumability is two-level:
//!
//!   * **Phase A** (the index pass) streams the sources once per chunk and writes
//!     four *per-chunk* artifacts to a stable work directory: a key-sorted text
//!     partial, a record temp, a facet partial, and a DOI temp. Each artifact is
//!     written to a `.tmp` sibling and atomically renamed, so a chunk counts as
//!     done only when all four finals exist. A re-run skips finished chunks and
//!     redoes at most the one chunk that was in flight.
//!   * The **finalizers** then merge those artifacts into the four outputs — text
//!     index (`.rrs`), record store (`.bin`/`.idx`, optionally zstd), facet sidecar
//!     (`.rrf`), and DOI lookup (`.rril`) — and each is skipped if its output is
//!     already present.
//!
//! The memory win over the previous chunked path is that the cross-chunk DOI and
//! facet accumulators no longer live in RAM: each chunk flushes its DOIs and facet
//! postings to disk, and the finalizers stream them back. The doc-id map
//! (`id_to_doc`) is the only large structure, it is needed only by Phase A, and it
//! is dropped before the finalizers run — so a finalizer-only resume needs no large
//! allocation and no source re-stream at all. The DOI lookup is built by streaming
//! the DOI temps through [`write_lookup_streaming`], which retains only the
//! fixed-width hash triples rather than every identifier string.
//!
//! All four outputs are byte-for-byte identical to the single-pass build on the
//! same inputs: the text merge unions the same disjoint per-chunk postings, the
//! record concat feeds the same records in doc-id order through the same writer,
//! the facet merge unions the same disjoint postings, and the lookup hashes/sorts
//! the same `(DOI, doc)` pairs. The dictionary sample is re-derived from the record
//! temps as exactly the single-pass selection (every stride-th non-empty record by
//! global doc id), so a compressed store matches too.

use rayon::prelude::*;
use roaring::RoaringBitmap;
use roaringrange::build::chunk::{merge_partials_to_rrs, write_partial};
use roaringrange::build::{
    split_posting, train_record_dict, write_facets, write_lookup_streaming, FacetCategory,
    FacetField, DEFAULT_HEAD_BOUNDARY, DEFAULT_STRIDE,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tracing::{info, info_span, warn};

use crate::{
    build_source_range, concat_chunk_records, file_len, rank_rows, write_chunk_records, Source,
    FACET_FIELDS, GRAM, KEY_SHARDS, ZSTD_DICT_SAMPLE_CAP,
};

/// Per-chunk artifact kinds written by Phase A (one file each, per chunk).
const ARTIFACTS: [&str; 4] = ["partial", "recs", "facets", "dois"];

/// Output paths and build knobs for [`build`]. Mirrors the chunked-build arguments
/// but bundled so the orchestration reads cleanly.
pub struct Opts<'a> {
    pub rrs_path: &'a str,
    pub facets_path: &'a str,
    pub bin_path: &'a str,
    pub idx_path: &'a str,
    pub lookup_path: &'a str,
    pub limit: usize,
    pub chunks: usize,
    pub abstract_cap: usize,
    /// `Some((dict_path, dict_size, level))` enables a zstd-compressed record store.
    pub zstd: Option<(&'a str, usize, i32)>,
    /// Override for the work directory (defaults to `<rrs_path>.rrwork`).
    pub work_dir: Option<&'a str>,
    /// Keep the work directory after a successful build (default: remove it).
    pub keep_work: bool,
}

/// Runs the phased, resumable build. Ranks works (persisting the ranking so a
/// resume skips it), runs Phase A for any incomplete chunk, then runs each
/// finalizer whose output is missing, and finally removes the work directory.
pub fn build(sources: &[Source], opts: Opts, t0: Instant) {
    let work = work_dir(&opts);
    std::fs::create_dir_all(&work).expect("create work dir");
    let chunks = opts.chunks;

    // Ranking: reuse a persisted ranking on resume; otherwise rank now and persist
    // it. Only the work count is needed up front (to size chunks); the full doc-id
    // map is built lazily, and only if Phase A actually has work to do.
    let ranking_path = work.join("ranking.bin");
    let (n, ranked_rows) = if ranking_path.exists() {
        let n = read_ranking_count(&ranking_path).expect("read ranking count");
        info!(works = n, "resume: ranking present");
        (n, None)
    } else {
        let _span = info_span!("rank").entered();
        let rows = rank_rows(sources, opts.limit, t0);
        if rows.is_empty() {
            warn!("no works ranked");
            std::process::exit(1);
        }
        info!(works = rows.len(), top_cited = rows[0].1, "ranked");
        persist_ranking(&ranking_path, &rows).expect("persist ranking");
        (rows.len(), Some(rows))
    };

    let chunk_size = n.div_ceil(chunks);
    let active: Vec<usize> = (0..chunks).filter(|&c| c * chunk_size < n).collect();
    info!(active_chunks = active.len(), chunk_size, work_dir = %work.display(), "phased build");

    // Phase A: index pass. Only run when some chunk is incomplete; only then is the
    // (large) doc-id map needed.
    if active.iter().any(|&c| !chunk_done(&work, c)) {
        let _span = info_span!("index").entered();
        let id_to_doc: HashMap<u64, u32> = match ranked_rows {
            Some(ref rows) => rows
                .iter()
                .enumerate()
                .map(|(i, (wid, _))| (*wid, i as u32))
                .collect(),
            None => {
                info!("resume: loading doc-id map from ranking");
                load_ranking_map(&ranking_path).expect("load ranking map")
            }
        };
        run_phase_a(
            sources,
            &id_to_doc,
            n,
            chunk_size,
            &active,
            &work,
            opts.abstract_cap,
        );
        drop(id_to_doc);
    } else {
        info!(
            chunks = active.len(),
            "all chunks already indexed, skipping"
        );
    }
    drop(ranked_rows);

    // Finalizers — each skipped if its output already exists. No large RAM, no
    // source re-stream: they merge/stream the on-disk per-chunk artifacts.
    finalize_text(&active, &work, opts.rrs_path);
    finalize_records(&active, &work, n, opts.bin_path, opts.idx_path, opts.zstd);
    finalize_facets(&active, &work, opts.facets_path);
    finalize_lookup(&active, &work, opts.lookup_path);

    if opts.keep_work {
        info!(work_dir = %work.display(), "kept work dir");
    } else {
        match std::fs::remove_dir_all(&work) {
            Ok(()) => info!(work_dir = %work.display(), "removed work dir"),
            Err(e) => warn!(work_dir = %work.display(), error = %e, "could not remove work dir"),
        }
    }
    info!(
        docs = n,
        elapsed_s = t0.elapsed().as_secs_f64(),
        "build complete"
    );
}

/// Phase A: builds each incomplete chunk's four artifacts. For a chunk, fans the
/// sources across rayon (indexing only works whose doc id is in the chunk's range),
/// places the chunk's records, and writes the record temp, DOI temp, key-sorted
/// text partial, and facet partial — each atomically (`.tmp` then rename), so a
/// chunk is "done" only once all four exist.
#[allow(clippy::too_many_arguments)]
fn run_phase_a(
    sources: &[Source],
    id_to_doc: &HashMap<u64, u32>,
    n: usize,
    chunk_size: usize,
    active: &[usize],
    work: &Path,
    abstract_cap: usize,
) {
    for &c in active {
        let lo = (c * chunk_size) as u32;
        let hi = ((c + 1) * chunk_size).min(n) as u32;
        if chunk_done(work, c) {
            info!(chunk = c, lo, hi, "chunk cached, skip");
            continue;
        }
        let _span = info_span!("chunk", chunk = c).entered();
        let tc = Instant::now();

        let shards: Vec<Mutex<HashMap<u64, RoaringBitmap>>> = (0..KEY_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();

        let per_file: Vec<_> = sources
            .par_iter()
            .map(|s| build_source_range(s, id_to_doc, lo, hi, &shards, abstract_cap))
            .collect();
        let indexed: usize = per_file.iter().map(|v| v.recs.len()).sum();

        // Place this chunk's records at their chunk-local offset; gather its DOIs;
        // union the per-source facet postings (order-independent).
        let mut chunk_recs: Vec<Vec<u8>> = vec![Vec::new(); (hi - lo) as usize];
        let mut dois: Vec<(String, u32)> = Vec::new();
        let mut facet_maps: Vec<HashMap<String, RoaringBitmap>> =
            (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
        for fr in per_file {
            for (d, rec) in fr.recs {
                chunk_recs[(d - lo) as usize] = rec;
            }
            dois.extend(fr.dois);
            for (fi, m) in fr.facets.into_iter().enumerate() {
                for (val, bm) in m {
                    match facet_maps[fi].get_mut(&val) {
                        Some(acc) => *acc |= bm,
                        None => {
                            facet_maps[fi].insert(val, bm);
                        }
                    }
                }
            }
        }

        // Record temp (framed [len][bytes]); written atomically.
        let recs_final = chunk_path(work, c, "recs");
        let recs_tmp = tmp_path(&recs_final);
        write_chunk_records(&recs_tmp, &chunk_recs).expect("write chunk records");
        std::fs::rename(&recs_tmp, &recs_final).expect("rename recs");
        drop(chunk_recs);

        // DOI temp.
        let ndois = dois.len();
        write_atomic(&chunk_path(work, c, "dois"), |w| write_doi_temp(w, &dois))
            .expect("write doi temp");
        drop(dois);

        // Text partial: each key's whole (unsplit) chunk bitmap, serialized.
        let entries: Vec<(u64, Vec<u8>)> = shards
            .into_par_iter()
            .flat_map_iter(|m| {
                m.into_inner()
                    .unwrap()
                    .into_iter()
                    .map(|(k, bm)| {
                        let mut b = Vec::with_capacity(bm.serialized_size());
                        bm.serialize_into(&mut b).expect("serialize posting");
                        (k, b)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let ngrams = entries.len();
        write_atomic(&chunk_path(work, c, "partial"), |w| {
            write_partial(w, entries)
        })
        .expect("write partial");

        // Facet partial: per-field (value -> serialized chunk bitmap), from the
        // unioned per-source maps gathered above.
        write_atomic(&chunk_path(work, c, "facets"), |w| {
            write_facet_partial(w, &facet_maps)
        })
        .expect("write facet partial");

        info!(
            lo,
            hi,
            works = indexed,
            ngrams,
            dois = ndois,
            elapsed_s = tc.elapsed().as_secs_f64(),
            "chunk indexed"
        );
    }
}

/// Finalizer: merges the per-chunk text partials into the standard `.rrs`. Skipped
/// if the output exists. Streams by key (merge peak is one key's postings), writing
/// to a `.tmp` that is renamed on success.
fn finalize_text(active: &[usize], work: &Path, rrs_path: &str) {
    let _span = info_span!("merge-text").entered();
    if Path::new(rrs_path).exists() {
        info!("output present, skip ({rrs_path})");
        return;
    }
    let t = Instant::now();
    let partials: Vec<PathBuf> = active
        .iter()
        .map(|&c| chunk_path(work, c, "partial"))
        .collect();
    let tmp = tmp_path(Path::new(rrs_path));
    {
        let mut f = File::create(&tmp).expect("create rrs tmp");
        merge_partials_to_rrs(&partials, GRAM as u16, DEFAULT_STRIDE, &mut f).expect("merge partials");
        f.flush().expect("flush rrs");
    }
    std::fs::rename(&tmp, rrs_path).expect("rename rrs");
    info!(
        partials = partials.len(),
        bytes = file_len(rrs_path),
        elapsed_s = t.elapsed().as_secs_f64(),
        "merged text index -> {rrs_path}"
    );
}

/// Finalizer: concatenates the per-chunk record temps (doc-id order) into the final
/// store, optionally zstd-compressing against a dictionary trained from a sample
/// re-derived from the temps. Skipped if both `.bin`/`.idx` exist (and the `.dict`
/// when compressing). Writes `.tmp` outputs that are renamed on success.
fn finalize_records(
    active: &[usize],
    work: &Path,
    n: usize,
    bin_path: &str,
    idx_path: &str,
    zstd: Option<(&str, usize, i32)>,
) {
    let _span = info_span!("compress-records").entered();
    let dict_missing = zstd
        .map(|(dp, _, _)| !Path::new(dp).exists())
        .unwrap_or(false);
    if Path::new(bin_path).exists() && Path::new(idx_path).exists() && !dict_missing {
        info!("output present, skip ({bin_path})");
        return;
    }
    let t = Instant::now();
    let temps: Vec<PathBuf> = active
        .iter()
        .map(|&c| chunk_path(work, c, "recs"))
        .collect();

    let dict_and_level: Option<(Vec<u8>, i32)> = match zstd {
        Some((dict_path, dict_size, level)) => {
            let dict = train_dict_from_temps(&temps, n, dict_size).expect("train record dict");
            std::fs::write(dict_path, &dict).expect("write dict");
            info!(bytes = dict.len(), "wrote zstd dict {dict_path}");
            Some((dict, level))
        }
        None => None,
    };
    let zstd_cfg = dict_and_level.as_ref().map(|(d, l)| (d.as_slice(), *l));

    let bin_tmp = tmp_path(Path::new(bin_path));
    let idx_tmp = tmp_path(Path::new(idx_path));
    concat_chunk_records(
        &temps,
        n,
        bin_tmp.to_str().expect("bin tmp utf8"),
        idx_tmp.to_str().expect("idx tmp utf8"),
        zstd_cfg,
    )
    .expect("concat records");
    std::fs::rename(&bin_tmp, bin_path).expect("rename bin");
    std::fs::rename(&idx_tmp, idx_path).expect("rename idx");
    info!(
        bytes = file_len(bin_path),
        elapsed_s = t.elapsed().as_secs_f64(),
        "wrote record store {bin_path} (+{idx_path})"
    );
}

/// Finalizer: merges the per-chunk facet partials (disjoint postings) into the
/// `.rrf` sidecar. Skipped if the output exists. The merge fold is the only sizable
/// allocation (the unioned facet postings), well under the index's footprint.
fn finalize_facets(active: &[usize], work: &Path, facets_path: &str) {
    let _span = info_span!("merge-facets").entered();
    if Path::new(facets_path).exists() {
        info!("output present, skip ({facets_path})");
        return;
    }
    let t = Instant::now();
    let temps: Vec<PathBuf> = active
        .iter()
        .map(|&c| chunk_path(work, c, "facets"))
        .collect();
    let acc = merge_facet_partials(&temps).expect("merge facet partials");
    let fields_out: Vec<FacetField> = acc
        .into_iter()
        .enumerate()
        .map(|(fi, map)| {
            let mut cats: Vec<FacetCategory> = map
                .into_iter()
                .map(|(val, bm)| {
                    let card = bm.len() as u32;
                    let (head, tail) = split_posting(&bm, DEFAULT_HEAD_BOUNDARY);
                    FacetCategory {
                        name: val,
                        card,
                        head,
                        tail,
                    }
                })
                .collect();
            // Sort by name for a reproducible string-blob layout (the reader resolves
            // by name and re-sorts the category table by key).
            cats.sort_by(|a, b| a.name.cmp(&b.name));
            FacetField {
                name: FACET_FIELDS[fi].to_string(),
                cats,
            }
        })
        .collect();
    write_atomic(Path::new(facets_path), |w| write_facets(w, fields_out)).expect("write facets");
    info!(
        bytes = file_len(facets_path),
        elapsed_s = t.elapsed().as_secs_f64(),
        "merged facets -> {facets_path}"
    );
}

/// Finalizer: streams the per-chunk DOI temps through [`write_lookup_streaming`]
/// into the `.rril` sidecar, retaining only the hash triples (not the identifier
/// strings). Skipped if the output exists.
fn finalize_lookup(active: &[usize], work: &Path, lookup_path: &str) {
    let _span = info_span!("build-lookup").entered();
    if Path::new(lookup_path).exists() {
        info!("output present, skip ({lookup_path})");
        return;
    }
    let t = Instant::now();
    let temps: Vec<PathBuf> = active
        .iter()
        .map(|&c| chunk_path(work, c, "dois"))
        .collect();
    let reader = DoiTempReader::new(temps);
    write_atomic(Path::new(lookup_path), |w| {
        write_lookup_streaming(w, reader)
    })
    .expect("write lookup");
    info!(
        bytes = file_len(lookup_path),
        elapsed_s = t.elapsed().as_secs_f64(),
        "wrote DOI lookup {lookup_path}"
    );
}

/// The work directory: `-work` override, else `<rrs_path>.rrwork`. Derived from the
/// output path so it is stable across runs (resume finds the same artifacts) yet
/// distinct between different builds.
fn work_dir(opts: &Opts) -> PathBuf {
    match opts.work_dir {
        Some(d) => PathBuf::from(d),
        None => {
            let mut s = std::ffi::OsString::from(opts.rrs_path);
            s.push(".rrwork");
            PathBuf::from(s)
        }
    }
}

/// Path of a chunk's artifact of the given kind within the work directory.
fn chunk_path(work: &Path, c: usize, kind: &str) -> PathBuf {
    work.join(format!("chunk_{c}.{kind}"))
}

/// A chunk is complete iff all four of its artifacts exist (each is renamed into
/// place only after being fully written, so a half-written `.tmp` never counts).
fn chunk_done(work: &Path, c: usize) -> bool {
    ARTIFACTS
        .iter()
        .all(|kind| chunk_path(work, c, kind).exists())
}

/// `<path>.tmp` sibling used for atomic writes.
fn tmp_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Writes `path` atomically: the body writes to a `.tmp` sibling that is renamed
/// over `path` only after a successful flush, so an interrupted write leaves the
/// final path absent (or untouched) rather than truncated.
fn write_atomic<F>(path: &Path, body: F) -> io::Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> io::Result<()>,
{
    let tmp = tmp_path(path);
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        body(&mut w)?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// Writes a chunk's DOI temp: a stream of `[doc u32][len u32][doi bytes]` frames.
fn write_doi_temp<W: Write>(mut w: W, dois: &[(String, u32)]) -> io::Result<()> {
    for (doi, doc) in dois {
        w.write_all(&doc.to_le_bytes())?;
        w.write_all(&(doi.len() as u32).to_le_bytes())?;
        w.write_all(doi.as_bytes())?;
    }
    Ok(())
}

/// Writes a chunk's facet partial: `[field_count u32]`, then per field
/// `[cat_count u32]` and per category `[val_len u32][val][bm_len u32][bitmap]`.
/// Fields are positional (aligned with [`FACET_FIELDS`]); categories are written in
/// map order (the merge accumulates by value, so order is irrelevant to the union).
fn write_facet_partial<W: Write>(
    mut w: W,
    fields: &[HashMap<String, RoaringBitmap>],
) -> io::Result<()> {
    w.write_all(&(fields.len() as u32).to_le_bytes())?;
    for map in fields {
        w.write_all(&(map.len() as u32).to_le_bytes())?;
        for (val, bm) in map {
            w.write_all(&(val.len() as u32).to_le_bytes())?;
            w.write_all(val.as_bytes())?;
            let mut bb = Vec::with_capacity(bm.serialized_size());
            bm.serialize_into(&mut bb).expect("serialize facet posting");
            w.write_all(&(bb.len() as u32).to_le_bytes())?;
            w.write_all(&bb)?;
        }
    }
    Ok(())
}

/// Reads a little-endian `u32` from `r`.
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Merges the per-chunk facet partials into one `(value -> posting)` map per field
/// by OR-ing the postings. Chunk doc ranges are disjoint, so OR is the union — the
/// same postings the single-pass path accumulates, regardless of merge order.
fn merge_facet_partials(paths: &[PathBuf]) -> io::Result<Vec<HashMap<String, RoaringBitmap>>> {
    let mut acc: Vec<HashMap<String, RoaringBitmap>> =
        (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
    for p in paths {
        let mut r = BufReader::with_capacity(1 << 20, File::open(p)?);
        let nfields = read_u32(&mut r)? as usize;
        for fi in 0..nfields {
            let ncats = read_u32(&mut r)? as usize;
            for _ in 0..ncats {
                let vlen = read_u32(&mut r)? as usize;
                let mut vb = vec![0u8; vlen];
                r.read_exact(&mut vb)?;
                let val = String::from_utf8(vb)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let blen = read_u32(&mut r)? as usize;
                let mut bb = vec![0u8; blen];
                r.read_exact(&mut bb)?;
                let bm = RoaringBitmap::deserialize_from(&bb[..])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if fi < acc.len() {
                    match acc[fi].get_mut(&val) {
                        Some(a) => *a |= bm,
                        None => {
                            acc[fi].insert(val, bm);
                        }
                    }
                }
            }
        }
    }
    Ok(acc)
}

/// Re-derives the dictionary-training sample from the record temps (read in doc-id
/// order): every `stride`-th non-empty record by global doc id, exactly the
/// single-pass selection (`records.iter().step_by(stride).filter(non-empty)`).
/// Non-sampled frames are skipped without allocating their bytes.
fn sample_from_temps(paths: &[PathBuf], stride: usize) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut gi: usize = 0;
    for p in paths {
        let mut r = BufReader::with_capacity(1 << 20, File::open(p)?);
        loop {
            let mut lb = [0u8; 4];
            match r.read_exact(&mut lb) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let len = u32::from_le_bytes(lb) as usize;
            if gi.is_multiple_of(stride) {
                let mut bytes = vec![0u8; len];
                r.read_exact(&mut bytes)?;
                if !bytes.is_empty() {
                    out.push(bytes);
                }
            } else {
                // Skip with seek_relative, NOT seek: a plain seek discards the
                // BufReader buffer every frame, which thrashes catastrophically when
                // stepping past ~every record of a multi-hundred-GB stream.
                // seek_relative advances within the buffer (only refilling at buffer
                // boundaries), keeping the skip a sequential scan.
                r.seek_relative(len as i64)?;
            }
            gi += 1;
        }
    }
    Ok(out)
}

/// Trains the shared zstd dictionary from the temp-derived sample, matching the
/// single-pass stride so the dictionary (and thus the compressed store) is
/// identical regardless of chunk count.
fn train_dict_from_temps(paths: &[PathBuf], n: usize, dict_size: usize) -> io::Result<Vec<u8>> {
    let stride = n.div_ceil(ZSTD_DICT_SAMPLE_CAP).max(1);
    let samples = sample_from_temps(paths, stride)?;
    let count = samples.len();
    let refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();
    let dict = train_record_dict(&refs, dict_size)?;
    info!(bytes = dict.len(), sampled = count, "trained zstd dict");
    Ok(dict)
}

/// Persists the ranking as `[count u64]` then the work ids in doc-id (rank) order,
/// so a resume can rebuild the identical `id_to_doc` map (or read just the count).
fn persist_ranking(path: &Path, rows: &[(u64, i64)]) -> io::Result<()> {
    write_atomic(path, |w| {
        w.write_all(&(rows.len() as u64).to_le_bytes())?;
        for (wid, _) in rows {
            w.write_all(&wid.to_le_bytes())?;
        }
        Ok(())
    })
}

/// Reads just the work count from a persisted ranking (its 8-byte header).
fn read_ranking_count(path: &Path) -> io::Result<usize> {
    let mut r = BufReader::new(File::open(path)?);
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b) as usize)
}

/// Rebuilds `id_to_doc` (work id -> doc id) from a persisted ranking: doc id is the
/// work's position in rank order, identical to the freshly-ranked map.
fn load_ranking_map(path: &Path) -> io::Result<HashMap<u64, u32>> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    let n = u64::from_le_bytes(b) as usize;
    let mut map = HashMap::with_capacity(n);
    for i in 0..n {
        r.read_exact(&mut b)?;
        map.insert(u64::from_le_bytes(b), i as u32);
    }
    Ok(map)
}

/// Iterator over the `(DOI, doc)` pairs in a sequence of DOI temps, opening each
/// file in turn. Feeds [`write_lookup_streaming`] so the lookup is built without
/// holding every identifier string in memory.
struct DoiTempReader {
    paths: std::vec::IntoIter<PathBuf>,
    cur: Option<BufReader<File>>,
}

impl DoiTempReader {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths: paths.into_iter(),
            cur: None,
        }
    }
}

impl Iterator for DoiTempReader {
    type Item = (String, u32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.cur.is_none() {
                let p = self.paths.next()?;
                let f =
                    File::open(&p).unwrap_or_else(|e| panic!("open doi temp {}: {e}", p.display()));
                self.cur = Some(BufReader::with_capacity(1 << 20, f));
            }
            let r = self.cur.as_mut().unwrap();
            let mut db = [0u8; 4];
            match r.read_exact(&mut db) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.cur = None;
                    continue;
                }
                Err(e) => panic!("doi temp read: {e}"),
            }
            let doc = u32::from_le_bytes(db);
            let mut lb = [0u8; 4];
            r.read_exact(&mut lb).expect("doi len");
            let mut bytes = vec![0u8; u32::from_le_bytes(lb) as usize];
            r.read_exact(&mut bytes).expect("doi bytes");
            let doi = String::from_utf8(bytes).expect("doi utf8");
            return Some((doi, doc));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(docs: &[u32]) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &d in docs {
            b.insert(d);
        }
        b
    }

    /// The temp-derived dictionary sample is exactly the single-pass selection
    /// (`step_by(stride)` then drop-empty), split across two record temps.
    #[test]
    fn sample_from_temps_matches_single_pass_selection() {
        let recs: Vec<Vec<u8>> = (0..10)
            .map(|i| {
                if i == 6 {
                    Vec::new()
                } else {
                    format!("rec-{i}").into_bytes()
                }
            })
            .collect();
        let stride = 3;
        let want: Vec<Vec<u8>> = recs
            .iter()
            .step_by(stride)
            .filter(|r| !r.is_empty())
            .cloned()
            .collect();
        let dir = std::env::temp_dir();
        let p0 = dir.join("rr_phased_sample_0.recs");
        let p1 = dir.join("rr_phased_sample_1.recs");
        write_chunk_records(&p0, &recs[0..4]).unwrap();
        write_chunk_records(&p1, &recs[4..10]).unwrap();
        let got = sample_from_temps(&[p0.clone(), p1.clone()], stride).unwrap();
        assert_eq!(got, want);
        for p in [p0, p1] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Per-chunk facet partials, written and then merged, produce a facet sidecar
    /// byte-identical to writing the single-pass union directly — the disjoint
    /// chunk postings OR back to the same per-(field, value) bitmaps.
    #[test]
    fn merged_facets_byte_identical_to_single_pass() {
        // Two disjoint doc ranges; some values appear in both chunks, some in one.
        let chunk_a: Vec<HashMap<String, RoaringBitmap>> = {
            let mut v: Vec<HashMap<String, RoaringBitmap>> =
                (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
            v[0].insert("2020".into(), bm(&[1, 3]));
            v[1].insert("article".into(), bm(&[1, 3, 70_000]));
            v
        };
        let chunk_b: Vec<HashMap<String, RoaringBitmap>> = {
            let mut v: Vec<HashMap<String, RoaringBitmap>> =
                (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
            v[0].insert("2020".into(), bm(&[100_000]));
            v[0].insert("2021".into(), bm(&[100_001]));
            v[1].insert("article".into(), bm(&[100_002]));
            v
        };
        // The single-pass accumulator: the union of the two chunks.
        let single: Vec<HashMap<String, RoaringBitmap>> = {
            let mut v: Vec<HashMap<String, RoaringBitmap>> =
                (0..FACET_FIELDS.len()).map(|_| HashMap::new()).collect();
            v[0].insert("2020".into(), bm(&[1, 3, 100_000]));
            v[0].insert("2021".into(), bm(&[100_001]));
            v[1].insert("article".into(), bm(&[1, 3, 70_000, 100_002]));
            v
        };

        let to_fields = |acc: Vec<HashMap<String, RoaringBitmap>>| -> Vec<FacetField> {
            acc.into_iter()
                .enumerate()
                .map(|(fi, map)| {
                    let mut cats: Vec<FacetCategory> = map
                        .into_iter()
                        .map(|(val, b)| {
                            let card = b.len() as u32;
                            let (head, tail) = split_posting(&b, DEFAULT_HEAD_BOUNDARY);
                            FacetCategory {
                                name: val,
                                card,
                                head,
                                tail,
                            }
                        })
                        .collect();
                    cats.sort_by(|a, b| a.name.cmp(&b.name));
                    FacetField {
                        name: FACET_FIELDS[fi].to_string(),
                        cats,
                    }
                })
                .collect()
        };

        let mut want = Vec::new();
        write_facets(&mut want, to_fields(single)).unwrap();

        let dir = std::env::temp_dir();
        let pa = dir.join("rr_phased_facet_a.facets");
        let pb = dir.join("rr_phased_facet_b.facets");
        write_atomic(&pa, |w| write_facet_partial(w, &chunk_a)).unwrap();
        write_atomic(&pb, |w| write_facet_partial(w, &chunk_b)).unwrap();
        let merged = merge_facet_partials(&[pa.clone(), pb.clone()]).unwrap();
        let mut got = Vec::new();
        write_facets(&mut got, to_fields(merged)).unwrap();

        assert_eq!(got, want, "merged facets differ from single-pass union");
        for p in [pa, pb] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// A DOI temp round-trips through [`DoiTempReader`] in write order, across files.
    #[test]
    fn doi_temp_reader_round_trips() {
        let dir = std::env::temp_dir();
        let p0 = dir.join("rr_phased_doi_0.dois");
        let p1 = dir.join("rr_phased_doi_1.dois");
        let a: Vec<(String, u32)> = vec![("10.1/x".into(), 5), ("10.2/y".into(), 9)];
        let b: Vec<(String, u32)> = vec![("10.3/z".into(), 1)];
        write_atomic(&p0, |w| write_doi_temp(w, &a)).unwrap();
        write_atomic(&p1, |w| write_doi_temp(w, &b)).unwrap();
        let got: Vec<(String, u32)> = DoiTempReader::new(vec![p0.clone(), p1.clone()]).collect();
        assert_eq!(got, vec![a[0].clone(), a[1].clone(), b[0].clone()]);
        for p in [p0, p1] {
            let _ = std::fs::remove_file(p);
        }
    }
}
