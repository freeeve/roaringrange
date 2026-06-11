//! Dumps an `RRSF` meta region (fields, category counts, member totals) from a local
//! pre-sliced boot-region file (header + tables + string blob, e.g. the bundle
//! staging copy) or the head of a full `.rrf`:
//!
//!   cargo run --release --example facet_meta_dump -- /tmp/oa-out/openalex-full.rrf.boot

use roaringrange::FacetMeta;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: facet_meta_dump FILE");
    let buf = std::fs::read(&path).expect("read file");
    println!("meta bytes: {}", buf.len());
    let meta = FacetMeta::parse(buf).expect("parse RRSF meta");
    let idx = meta.attach(roaringrange::MemoryFetch::new(Vec::new()));
    for f in &idx.fields {
        let total: u64 = f.categories.iter().map(|c| u64::from(c.count)).sum();
        println!(
            "  field {:<12} {:>6} cats  {:>13} total members",
            f.name,
            f.categories.len(),
            total
        );
    }
}
