//! Generates the sample artifacts the standalone split-set demo serves: a tiered `RRSS` split
//! set (with term Bloom filters + per-split `RRSF` facet sidecars) plus a matching `RRSR`
//! record store, written into `examples/splitset-demo/data/`.
//!
//!   cargo run --release --features "splits hotcache" --example splitset_demo_data [OUT_DIR]
//!
//! The corpus is a deterministic set of synthetic "papers" (title words + a `year` and `field`
//! facet), fed in rank order so global doc id == rank. A small byte cap forces several tiers so
//! the demo can show the tiered short-circuit, Bloom pruning, and facet-presence pruning. With
//! the `hotcache` feature on it also emits `index.rrhc`, the boot bundle the demo opens to
//! collapse the per-split header GETs into one (`RrssIndex.openBundle`).

use roaringrange::build::write_records;
use roaringrange::{Policy, SplitBuildConfig, SplitSetBuilder};
use std::fs;
use std::path::Path;

/// A deterministic xorshift so the sample is reproducible without an RNG crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() as usize) % xs.len()]
    }
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../examples/splitset-demo/data".to_string());
    let dir = Path::new(&out);
    fs::create_dir_all(dir).expect("create out dir");

    // Vocabulary: a few "topic" words (common) + per-paper distinctive tokens, plus facets.
    let topics = [
        "neural",
        "quantum",
        "genome",
        "climate",
        "protein",
        "galaxy",
        "vaccine",
        "graphene",
        "blackhole",
        "enzyme",
        "robotics",
        "plasma",
    ];
    let fields = ["physics", "biology", "cs", "medicine", "astronomy"];
    let years = ["2019", "2020", "2021", "2022", "2023"];

    let n = 400usize;
    let mut rng = Rng(0x1234_5678_9abc_def0);
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(n);

    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap: 16 * 1024, // small -> several tiers
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "demo".to_string(),
        sortcol: None,
        bloom_bits_per_key: 10,
    });

    for i in 0..n {
        // Two topic words + a unique token; rarer papers (higher rank id) get rarer topics.
        let t1 = topics[i % topics.len()];
        let t2 = rng.pick(&topics);
        let title = format!("{t1} {t2} study tok{i:04}");
        let field = rng.pick(&fields).to_string();
        let year = rng.pick(&years).to_string();

        let facets = vec![
            ("field".to_string(), field.clone()),
            ("year".to_string(), year.clone()),
        ];
        let id = b.add_faceted(&title, &facets).expect("add");
        assert_eq!(id as usize, i);

        let rec = format!(r#"{{"id":{i},"title":{title:?},"field":{field:?},"year":{year:?}}}"#);
        records.push(rec.into_bytes());
    }

    let built = b.finish().expect("finish");

    // Write the manifest, each split .rrs, and each per-split .rrf facet sidecar.
    fs::write(dir.join("index.rrss"), &built.manifest).expect("write manifest");
    for (name, bytes) in &built.splits {
        fs::write(dir.join(name), bytes).expect("write split");
    }
    for (name, bytes) in &built.facets {
        fs::write(dir.join(name), bytes).expect("write facet sidecar");
    }

    // An RRHC boot bundle (behind `hotcache`): inlines every split's boot region so the demo
    // boots the split set with the per-split header GETs collapsed into the single `.rrhc` GET
    // (the 2-round-trip cold boot). Optional — with the feature off the demo cold-opens splits.
    #[cfg(feature = "hotcache")]
    {
        let mut rrhc = Vec::new();
        roaringrange::write_splitset_bundle(&mut rrhc, &built, 0, 1 << 20).expect("write bundle");
        fs::write(dir.join("index.rrhc"), &rrhc).expect("write rrhc");
    }

    // The record store (raw JSON), keyed by the same global doc ids.
    let mut idx = Vec::new();
    let mut bin = Vec::new();
    write_records(&mut bin, &mut idx, &records).expect("write records");
    fs::write(dir.join("records.idx"), &idx).expect("write idx");
    fs::write(dir.join("records.bin"), &bin).expect("write bin");

    let total: u64 = built.splits.iter().map(|(_, b)| b.len() as u64).sum();
    println!(
        "wrote {} docs -> {} splits ({} facet sidecars), {} KB of splits, into {}",
        n,
        built.splits.len(),
        built.facets.len(),
        total / 1024,
        dir.display()
    );
}
