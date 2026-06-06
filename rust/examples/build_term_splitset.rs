//! Builds a **term split-set** (`RRSS`, `bodyKind = 1`) over an `RRSR` record store —
//! fully native Rust, parallel, and bounded-RAM. Each sealed split is an `RRTI` term index
//! over a byte-capped band of top-ranked documents; the `.rrss` manifest names them all.
//!
//!   cargo run --release --features "splits terms" --example build_term_splitset -- \
//!       records.idx records.bin records.dict <N> <out_dir> [byte_cap_mb=256] [language=english]
//!
//! Records are JSON in descending rank, so doc id == rank and `0..n` is the top-`n` corpus.
//! For each doc the title (`"t"`) and abstract (`"ab"`) are concatenated into one text field,
//! tokenized by the term builder, and fed in rank order. The JSON parse — the throughput
//! bottleneck a single-threaded pipeline wastes cores on — is fanned out across a `rayon`
//! pool per chunk; the builder's seal/insert stays sequential (rank order is load-bearing).
//! Splits are streamed to disk via [`TermSplitSetBuilder::drain_sealed`] after every chunk, so
//! peak memory is one open split rather than the whole split set.

use rayon::prelude::*;
use roaringrange::fetch::{FetchError, RangeFetch};
use roaringrange::records::RecordStore;
use roaringrange::{Language, Policy, TermSplitBuildConfig, TermSplitSetBuilder};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// A [`RangeFetch`] over a local file using positional reads (`pread`), so a 100+ GB store is
/// range-read without loading it into memory. Cloneable (shared fd) so each `rayon` worker reads
/// its own records concurrently.
#[derive(Clone)]
struct FileFetch {
    file: Arc<File>,
}

impl FileFetch {
    /// Opens `path` read-only for positional reads.
    fn open(path: &str) -> std::io::Result<Self> {
        Ok(Self {
            file: Arc::new(File::open(path)?),
        })
    }
}

impl RangeFetch for FileFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            match self
                .file
                .read_at(&mut buf[filled..], offset + filled as u64)
            {
                Ok(0) => {
                    return Err(FetchError::Transport(format!(
                        "unexpected EOF at offset {offset} (+{filled})"
                    )))
                }
                Ok(nr) => filled += nr,
                Err(e) => return Err(FetchError::Transport(e.to_string())),
            }
        }
        Ok(buf)
    }
}

/// Parses one record's JSON bytes into the indexed text: `"<title> <abstract>"`, trimmed. A
/// missing field is treated as empty; a record that fails to parse contributes empty text (it
/// still consumes a doc id, keeping the doc-id space dense and aligned with records/facets).
fn record_text(bytes: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
    let ab = v.get("ab").and_then(|x| x.as_str()).unwrap_or("");
    format!("{t} {ab}").trim().to_string()
}

/// Writes each `(filename, bytes)` blob into `dir`, dropping the bytes as it goes, and returns
/// the number of files and total bytes written. Freeing the drained blobs here keeps peak RAM at
/// one open split.
fn flush_blobs(dir: &Path, blobs: Vec<(String, Vec<u8>)>) -> (usize, u64) {
    let mut count = 0;
    let mut total = 0u64;
    for (name, bytes) in blobs {
        total += bytes.len() as u64;
        let mut f = File::create(dir.join(&name)).expect("create split file");
        f.write_all(&bytes).expect("write split file");
        count += 1;
    }
    (count, total)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 || args.len() > 8 {
        eprintln!(
            "usage: build_term_splitset <records.idx> <records.bin> <records.dict> <N> <out_dir> \
             [byte_cap_mb=256] [language=english]"
        );
        std::process::exit(2);
    }
    let idx_path = &args[1];
    let bin_path = &args[2];
    let dict_path = &args[3];
    let want_n: u32 = args[4].parse().expect("N");
    let out_dir = Path::new(&args[5]);
    let byte_cap_mb: u64 = args
        .get(6)
        .map(|s| s.parse().expect("byte_cap_mb"))
        .unwrap_or(256);
    let language_arg = args.get(7).map(String::as_str).unwrap_or("english");
    let language = match language_arg {
        "english" => Some(Language::English),
        "none" | "" => None,
        other => {
            eprintln!("unknown language {other:?}; use `english` or `none`");
            std::process::exit(2);
        }
    };

    std::fs::create_dir_all(out_dir).expect("create out dir");

    let dict = std::fs::read(dict_path).expect("read dict");
    let idx = FileFetch::open(idx_path).expect("open idx");
    let bin = FileFetch::open(bin_path).expect("open bin");
    let store = futures::executor::block_on(RecordStore::open_with_dict(idx, bin, dict))
        .expect("open record store");

    let n = want_n.min(store.len());
    let byte_cap = byte_cap_mb * 1024 * 1024;

    let mut b = TermSplitSetBuilder::new(TermSplitBuildConfig {
        policy: Policy::Tiered,
        byte_cap,
        head_boundary: 0,
        name_prefix: "openalex-484m-terms".to_string(),
        sortcol: None,
        language,
        stopwords: false,
    });

    // Chunk size trades parallelism against the rayon fan-out's transient memory (one batch of
    // parsed texts resident at a time); 50k docs is a few MB of text, well under the byte cap.
    const CHUNK: u32 = 50_000;
    let started = Instant::now();
    let mut files_written = 0usize;
    let mut bytes_written = 0u64;

    let mut start = 0u32;
    while start < n {
        let end = (start + CHUNK).min(n);
        // Parse the chunk's records in parallel: each rayon worker pread's its own record and
        // JSON-parses it, returning (global_id, text). Sorting back into rank order keeps the
        // sequential builder feed in strict rank order regardless of completion order.
        let mut batch: Vec<(u32, String)> = (start..end)
            .into_par_iter()
            .map(|id| {
                let bytes = futures::executor::block_on(store.get(id))
                    .expect("get record")
                    .expect("record present");
                (id, record_text(&bytes))
            })
            .collect();
        batch.sort_unstable_by_key(|(id, _)| *id);

        for (id, text) in &batch {
            let assigned = b.add_text(text).expect("add doc");
            debug_assert_eq!(assigned, *id);
        }
        drop(batch);

        // Stream every split sealed during this chunk to disk and free its bytes.
        let (splits, facets) = b.drain_sealed();
        let (sc, sb) = flush_blobs(out_dir, splits);
        let (fc, fb) = flush_blobs(out_dir, facets);
        files_written += sc + fc;
        bytes_written += sb + fb;

        eprintln!(
            "  {} / {} docs | {} files written ({:.1} MB) | {:.1}s",
            end,
            n,
            files_written,
            bytes_written as f64 / (1024.0 * 1024.0),
            started.elapsed().as_secs_f64(),
        );
        start = end;
    }

    // Seal the final open split, write the remaining blobs, and write the manifest last.
    let built = b.finish().expect("finish");
    let (sc, sb) = flush_blobs(out_dir, built.splits);
    let (fc, fb) = flush_blobs(out_dir, built.facets);
    files_written += sc + fc;
    bytes_written += sb + fb;

    let manifest_name = "openalex-484m-terms.rrss";
    std::fs::write(out_dir.join(manifest_name), &built.manifest).expect("write manifest");

    eprintln!(
        "done: {} docs -> {} split/facet files + {} ({:.1} MB total) in {:.1}s into {}",
        n,
        files_written,
        manifest_name,
        bytes_written as f64 / (1024.0 * 1024.0),
        started.elapsed().as_secs_f64(),
        out_dir.display(),
    );
}
