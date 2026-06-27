//! End-to-end: an `RRHC` boot bundle over a split set (needs both the `splits` and `hotcache`
//! features). The bundle inlines the `RRSS` manifest plus the top tier's split boots, so a
//! reader opens those splits with `Index::from_boot` (no per-split header fetch) — the
//! 1–2 round-trip cold boot. Run with `cargo test --features "splits hotcache"`.
#![cfg(all(feature = "splits", feature = "hotcache"))]

use futures::executor::block_on;
use roaringrange::{
    write_hotcache, Hotcache, Index, MemberSpec, MemberTag, MemoryFetch, Policy, Split,
    SplitBuildConfig, SplitFetcher, SplitSet, SplitSetBuilder,
};
use std::collections::HashMap;

/// A reader resolver backed by an `RRHC` bundle: per-query reads still go to the split's data
/// file (here resident bytes), but a split's *boot* comes free from the bundle's inlined blob.
struct BundleResolver {
    files: HashMap<String, Vec<u8>>,
    hc: Hotcache,
}

impl SplitFetcher for BundleResolver {
    type Fetch = MemoryFetch;
    fn fetch_named(&self, name: &str) -> MemoryFetch {
        MemoryFetch::new(self.files.get(name).cloned().unwrap_or_default())
    }
    fn boot(&self, split: &Split) -> Option<Vec<u8>> {
        // One line: hand a split its inlined boot from the single GET that fetched the .rrhc.
        self.hc
            .inlined_by_name(&split.data_file)
            .map(<[u8]>::to_vec)
    }
}

#[test]
fn rrhc_bundle_boots_a_split_set() {
    // A tiered split set over 30 "abc"-bearing docs, small cap -> several tier-0 splits.
    let mut b = SplitSetBuilder::new(SplitBuildConfig {
        byte_cap_max: 0,
        policy: Policy::Tiered,
        byte_cap: 2048,
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        name_prefix: "corpus".to_string(),
        sortcol: None,
        bloom_bits_per_key: 10,
        case_sensitive: false,
    });
    for i in 0..30u32 {
        b.add_text(&format!("abc tok{i:04}")).unwrap();
    }
    let built = b.finish().unwrap();
    let files: HashMap<String, Vec<u8>> = built.splits.iter().cloned().collect();

    // Compose the boot bundle with the existing hotcache writer: the manifest (tag RRSS) plus
    // one member per split carrying its boot region ([0, dictStart), sized via Index::boot_len).
    let mut specs = vec![MemberSpec {
        tag: MemberTag::Rrss,
        data_file: "index.rrss".to_string(),
        boot_off: 0,
        boot_len: built.manifest.len() as u32,
        boot_bytes: built.manifest.clone(),
    }];
    for (name, bytes) in &built.splits {
        let boot_len = block_on(Index::open(MemoryFetch::new(bytes.clone())))
            .unwrap()
            .boot_len() as usize;
        specs.push(MemberSpec {
            tag: MemberTag::Rrs,
            data_file: name.clone(),
            boot_off: 0,
            boot_len: boot_len as u32,
            boot_bytes: bytes[..boot_len].to_vec(),
        });
    }
    let mut rrhc = Vec::new();
    write_hotcache(&mut rrhc, &specs, 1 << 20).unwrap();
    let hc = block_on(Hotcache::open(MemoryFetch::new(rrhc))).unwrap();

    // The manifest and every split boot came back inlined in the one GET.
    assert_eq!(hc.members().len(), built.splits.len() + 1);
    assert!(hc.inlined_by_name("index.rrss").is_some());
    assert!(hc.inlined_by_name(&built.splits[0].0).is_some());

    // Query through the bundle-backed resolver: top-3 are the highest-ranked docs (0,1,2), and
    // the splits opened via their inlined boots (no header fetch) — results must be identical.
    let ss = block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap();
    let resolver = BundleResolver { files, hc };
    assert_eq!(
        block_on(ss.search(&resolver, "abc", 3)).unwrap(),
        vec![0, 1, 2]
    );
    // A Bloom-absent term still prunes (and never consults a boot/fetch).
    assert!(block_on(ss.search(&resolver, "zzzqqq", 10))
        .unwrap()
        .is_empty());
}
