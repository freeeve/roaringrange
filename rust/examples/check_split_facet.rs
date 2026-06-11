//! Local end-to-end check of a derived per-split facet sidecar: opens one split's
//! `.rrs` body and its (possibly re-derived) `.rrf`, resolves a facet filter from
//! meta alone, and runs the filtered cursor — what `search_split_filtered` does,
//! against local files.
//!
//!   cargo run --release --features splits --example check_split_facet -- \
//!     SPLIT.rrs SIDECAR.rrf "machine learning" type article

use futures::executor::block_on;
use roaringrange::{FacetIndex, FileFetch, Index};

fn main() {
    let mut a = std::env::args().skip(1);
    let (rrs, rrf, q, field, cat) = (
        a.next()
            .expect("usage: check_split_facet SPLIT.rrs SIDECAR.rrf QUERY FIELD CAT"),
        a.next().expect("SIDECAR.rrf"),
        a.next().expect("QUERY"),
        a.next().expect("FIELD"),
        a.next().expect("CAT"),
    );
    let idx = block_on(Index::open(FileFetch::open(&rrs).expect("open .rrs"))).expect("rrs");
    let facets = block_on(FacetIndex::open_meta(
        FileFetch::open(&rrf).expect("open .rrf"),
    ))
    .expect("rrf");
    let sel = vec![(field.clone(), cat.clone())];
    let resolved = facets.resolve(&sel);
    println!(
        "filter {field}={cat}: empty_arm={} bound={:?}",
        resolved.has_empty_arm(),
        facets.filter_count_bound(&sel)
    );
    let mut cur =
        block_on(idx.search_cursor_filtered(&q, 0, Some(resolved))).expect("filtered cursor");
    block_on(cur.load_tail()).expect("tail");
    let hits = block_on(cur.page(0, 10)).expect("page");
    println!(
        "query {q:?}: {} filtered hits (first 10 local ids: {hits:?})",
        cur.loaded()
    );
}
