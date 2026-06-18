//! Derives full per-split `RRSF` facet sidecars by slicing the monolith's `.rrf`
//! along the split set's doc-ID ranges — no corpus re-stream. The monolith and the
//! split set share the rank-ordered doc-ID space, so a category's per-split posting
//! is exactly the monolith posting restricted to `[docIdLo, docIdHi]`, rebased to
//! local IDs.
//!
//! Unbounded-cardinality fields (OpenAlex `topic`: ~55k categories) are excluded by
//! default: per-split filtered search reads each visited split's whole meta region
//! (the `open_meta` path), and carrying tens of thousands of category entries would
//! turn that KB-scale read into MBs. Filters on excluded fields fall back to the
//! shared-ID-space post-filter against the monolith `.rrf`, like term mode.
//!
//!   cargo run --release --features splits --example derive_split_facets -- \
//!     /tmp/oa-out/openalex-full.rrf /tmp/oa-out/splitset-trigram-484m/openalex.rrss \
//!     /tmp/oa-out/split-facets-v2 [year,type,oa,language]

use futures::executor::block_on;
use roaring::RoaringBitmap;
use roaringrange::build::{write_facets, FacetCategory, FacetField};
use roaringrange::{FacetIndex, FileFetch, SplitSet};
use std::io::BufWriter;

const HEAD_BOUNDARY: u32 = 65536;

fn main() {
    let mut args = std::env::args().skip(1);
    let (rrf, manifest, out_dir) = (
        args.next()
            .expect("usage: derive_split_facets MONO_RRF MANIFEST OUT_DIR [fields]"),
        args.next().expect("MANIFEST arg"),
        args.next().expect("OUT_DIR arg"),
    );
    let fields: Vec<String> = args
        .next()
        .unwrap_or_else(|| "year,type,oa,language".to_string())
        .split(',')
        .map(str::to_string)
        .collect();
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let ss = SplitSet::from_bytes(&std::fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    let mono = block_on(FacetIndex::open_meta(
        FileFetch::open(&rrf).expect("open .rrf"),
    ))
    .expect("open mono facets");

    // Pull every selected category's full posting once (the postings live on local
    // disk; resident roaring across the bounded fields is a few GB), then slice all
    // splits from memory.
    let mut cats: Vec<(String, String, RoaringBitmap)> = Vec::new();
    for f in mono.fields() {
        if !fields.contains(&f.name) {
            eprintln!("skipping field {:?} ({} cats)", f.name, f.categories.len());
            continue;
        }
        for c in &f.categories {
            let sel = vec![(f.name.clone(), c.name.clone())];
            let bm = block_on(mono.resolve(&sel).full_bitmap()).expect("category posting");
            cats.push((f.name.clone(), c.name.clone(), bm));
        }
        eprintln!("loaded field {:?} ({} cats)", f.name, f.categories.len());
    }

    let mut written = 0usize;
    for split in ss.splits() {
        if split.doc_count == 0 {
            continue;
        }
        let (lo, hi) = (split.doc_id_lo, split.doc_id_hi);
        // Group the sliced postings back into write_facets' field shape, preserving
        // the cats vec's field-major order.
        let mut out_fields: Vec<FacetField> = Vec::new();
        for (field, cat, bm) in &cats {
            let mut local = RoaringBitmap::new();
            for g in bm.range(lo..=hi) {
                local.insert(g - lo);
            }
            if local.is_empty() {
                continue;
            }
            let card = local.len() as u32;
            let (head, tail) = roaringrange::build::split_posting(&local, HEAD_BOUNDARY);
            let fc = FacetCategory {
                name: cat.clone(),
                card,
                head,
                tail,
            };
            match out_fields.last_mut() {
                Some(f) if f.name == *field => f.cats.push(fc),
                _ => out_fields.push(FacetField {
                    name: field.clone(),
                    cats: vec![fc],
                }),
            }
        }
        let stem = split
            .data_file
            .strip_suffix(".rrs")
            .unwrap_or(&split.data_file);
        let path = format!("{out_dir}/{stem}.rrf");
        let w = BufWriter::new(std::fs::File::create(&path).expect("create sidecar"));
        write_facets(w, out_fields).expect("write sidecar");
        written += 1;
        if written.is_multiple_of(50) {
            eprintln!("  {written} sidecars written");
        }
    }
    println!("wrote {written} sidecars to {out_dir}");
}
