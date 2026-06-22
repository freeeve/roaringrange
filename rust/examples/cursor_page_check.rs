//! Reproduces the stateless-pagination bug and its fix over a local `.rrs`.
//!
//! The search Lambda builds a FRESH cursor per request and pages it. The cursor's
//! tail scan is bounded per call (first-paint bias), so a single `page(offset,limit)`
//! returns only the first sliver and an `offset>0` page comes back empty. The fix:
//! loop `page` until the slice is full or the tail is exhausted.
//!
//!   cargo run --release --example cursor_page_check -- <index.rrs> "<query>" [limit=25]

use futures::executor::block_on;
use roaringrange::{FileFetch, Index};
use std::collections::HashSet;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let path = &a[0];
    let query = &a[1];
    let limit: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(25);
    let good: Vec<u32> = a
        .get(3)
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_default();

    let idx = block_on(Index::open(FileFetch::open(path).expect("open"))).expect("index");

    // BROKEN: one page() call on a fresh cursor, like the Lambda does today.
    let broken = |offset: usize| -> Vec<u32> {
        let mut cur = block_on(idx.search_cursor(query, 0)).unwrap();
        block_on(cur.page(offset, limit)).unwrap()
    };
    // FIXED: loop page() on the fresh cursor until the slice fills or tail ends.
    let fixed = |offset: usize| -> Vec<u32> {
        let mut cur = block_on(idx.search_cursor(query, 0)).unwrap();
        let mut ids = block_on(cur.page(offset, limit)).unwrap();
        while ids.len() < limit && cur.pending_tail() {
            ids = block_on(cur.page(offset, limit)).unwrap();
        }
        ids
    };

    println!("query {query:?}, limit {limit}");
    println!(
        "  BROKEN page(0): {} ids   page({limit}): {} ids",
        broken(0).len(),
        broken(limit).len()
    );
    println!(
        "  FIXED  page(0): {} ids   page({limit}): {} ids",
        fixed(0).len(),
        fixed(limit).len()
    );

    // Page through with the fix; report total reachable and where the good docs land.
    let goodset: HashSet<u32> = good.iter().copied().collect();
    let mut total = 0usize;
    let mut found: Vec<(usize, u32)> = Vec::new();
    for page in 0..200 {
        let offset = page * limit;
        let ids = fixed(offset);
        if ids.is_empty() {
            break;
        }
        for id in &ids {
            if goodset.contains(id) {
                found.push((page, *id));
            }
        }
        total += ids.len();
        if ids.len() < limit {
            break;
        }
    }
    println!("  FIXED reachable total: {total}");
    if !good.is_empty() {
        println!(
            "  good docs found at (page, id): {found:?}  ({} of {})",
            found.len(),
            good.len()
        );
    }
}
