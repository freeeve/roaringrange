//! Local reproduction of the faceted Lambda handler against the byte-identical
//! local index files, to surface the production 500 with a real backtrace.
//!
//!   cd examples/search-lambda
//!   cargo run --example repro -- /tmp/oarust.rrs /tmp/oarust.rrf "machine learning"
//!
//! Mirrors handler(): open Catalog + facets, search_cursor_filtered, load_tail,
//! loaded(), facet counts over the head bitmap, page(0, limit).
use roaringrange::{Catalog, FetchError, RangeFetch};
use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// A `RangeFetch` over a local file (stand-in for S3 range reads).
#[derive(Clone)]
struct FileFetch {
    path: String,
}

impl RangeFetch for FileFetch {
    async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError> {
        thread_local! { static FILES: RefCell<Vec<(String, File)>> = const { RefCell::new(Vec::new()) }; }
        let mut buf = vec![0u8; len];
        FILES.with(|cell| {
            let mut v = cell.borrow_mut();
            if !v.iter().any(|(p, _)| p == &self.path) {
                let f = File::open(&self.path)
                    .map_err(|e| FetchError::Transport(format!("open {}: {e}", self.path)))?;
                v.push((self.path.clone(), f));
            }
            let f = &mut v.iter_mut().find(|(p, _)| p == &self.path).unwrap().1;
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| FetchError::Transport(format!("seek: {e}")))?;
            f.read_exact(&mut buf)
                .map_err(|e| FetchError::Transport(format!("read: {e}")))?;
            Ok::<(), FetchError>(())
        })?;
        Ok(buf)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let rrs = a
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/oarust.rrs".into());
    let rrf = a
        .get(2)
        .cloned()
        .unwrap_or_else(|| "/tmp/oarust.rrf".into());
    let query = a
        .get(3)
        .cloned()
        .unwrap_or_else(|| "machine learning".into());
    let offset: usize = 0;
    let limit: usize = 3;
    let max_missing: usize = 0;
    eprintln!("opening rrs={rrs} rrf={rrf} query={query:?}");

    let cat = Catalog::open(FileFetch { path: rrs })
        .await
        .expect("open index")
        .load_facets(FileFetch { path: rrf })
        .await
        .expect("open facets");
    eprintln!("catalog open; fields={}", cat.fields().len());

    let resolved = None;
    let mut cur = cat
        .index()
        .search_cursor_filtered(&query, max_missing, resolved)
        .await
        .expect("search");
    eprintln!("cursor open; head_count={}", cur.head_count());

    cur.load_tail().await.expect("load_tail");
    let total = cur.loaded();
    eprintln!("loaded (total) = {total}");

    if let Some(f) = cat.facets() {
        let counts = f.counts(cur.head_bitmap());
        eprintln!("facet count groups = {}", counts.len());
        for (field, fc) in cat.fields().iter().zip(&counts) {
            let nz = fc.iter().filter(|&&n| n > 0).count();
            eprintln!(
                "  field {:?}: {} categories, {} non-zero",
                field.name,
                fc.len(),
                nz
            );
        }
    }

    let ids = cur.page(offset, limit).await.expect("page");
    eprintln!("page ids = {ids:?}");
    println!("OK total={total} ids={ids:?}");
}
