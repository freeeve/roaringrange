//! Build-side emitter for an `RRHC` boot bundle over a split set — the reader-path application
//! of the catalog hotcache (`tasks/006_catalog_hotcache.md` §1).
//!
//! A split set names N immutable `RRS` splits; cold-booting it opens each queried split with a
//! separate header GET (one round trip per split, over a CDN). This emitter writes one `.rrhc`
//! that **inlines each split's boot region** (the `RRS` header + sparse index, `[0, boot_len)`)
//! as an `RRS` member keyed by the split's data-file name. A reader opens the manifest as usual
//! and then hands each split its inlined boot via [`crate::splitset::SplitFetcher::boot`], so the
//! split opens with [`crate::index::Index::from_boot`] (no header fetch). The N per-split opens
//! collapse into the single GET of this bundle — the 2-round-trip cold boot (manifest + bundle,
//! issued in one parallel wave).
//!
//! Native-only; behind `splits` + `hotcache`. It inlines split boots only (not the manifest),
//! matching the 2-round-trip reader path where the manifest keeps its own GET; a member whose
//! boot exceeds `inline_threshold` is referenced rather than inlined and simply cold-opens, so
//! the bundle degrades gracefully.

use crate::hotcache::MemberTag;
use crate::hotcache_build::{write_hotcache, MemberSpec};
use crate::index::rrs_boot_len;
use crate::splitset_build::BuiltSplitSet;
use std::io::{self, Write};

/// Writes an `.rrhc` boot bundle over `built` to `w`: one inlined `RRS` member per split (in
/// seal/rank order, so the top tiers come first), each carrying its split's boot region
/// `[0, boot_len)`.
///
/// `max_splits` caps how many splits are inlined (`0` = all); a corpus with a large top tier
/// inlines only the splits a top-K query is likely to open, keeping the first GET small.
/// `inline_threshold` is forwarded to [`write_hotcache`]: a split whose boot exceeds it is
/// referenced (not inlined), so its `SplitFetcher::boot` lookup returns `None` and it cold-opens
/// — the bundle never forces a fat first GET. Errors only on a malformed split header or an I/O
/// failure writing `w`.
pub fn write_splitset_bundle<W: Write>(
    w: W,
    built: &BuiltSplitSet,
    max_splits: usize,
    inline_threshold: u32,
) -> io::Result<()> {
    let take = if max_splits == 0 {
        built.splits.len()
    } else {
        max_splits.min(built.splits.len())
    };
    let mut specs = Vec::with_capacity(take);
    for (name, bytes) in built.splits.iter().take(take) {
        let boot_len = rrs_boot_len(bytes).map_err(|e| io::Error::other(e.to_string()))?;
        if boot_len > bytes.len() {
            return Err(io::Error::other(format!(
                "split {name}: boot region {boot_len} B exceeds the split's {} B",
                bytes.len()
            )));
        }
        specs.push(MemberSpec {
            tag: MemberTag::Rrs,
            data_file: name.clone(),
            boot_off: 0,
            boot_len: boot_len as u32,
            boot_bytes: bytes[..boot_len].to_vec(),
        });
    }
    write_hotcache(w, &specs, inline_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::hotcache::Hotcache;
    use crate::splitset::Policy;
    use crate::splitset::{Split, SplitFetcher, SplitSet};
    use crate::splitset_build::{SplitBuildConfig, SplitSetBuilder};
    use futures::executor::block_on;
    use std::collections::HashMap;

    /// A resolver whose per-query reads hit resident split bytes, but whose split *boots* come
    /// from an `RRHC` bundle's inlined blob — the browser's `SplitFetcher::boot` shape.
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
            self.hc
                .inlined_by_name(&split.data_file)
                .map(<[u8]>::to_vec)
        }
    }

    /// Builds a tiered split set over 30 "abc"-bearing docs; the small byte cap forces several
    /// splits so the bundle has more than one member.
    fn built_corpus() -> crate::splitset_build::BuiltSplitSet {
        let mut b = SplitSetBuilder::new(SplitBuildConfig {
            policy: Policy::Tiered,
            byte_cap: 2048,
            gram_size: 3,
            head_boundary: 0,
            stride: 0,
            name_prefix: "corpus".to_string(),
            sortcol: None,
            bloom_bits_per_key: 10,
        });
        for i in 0..30u32 {
            b.add_text(&format!("abc tok{i:04}")).unwrap();
        }
        let built = b.finish().unwrap();
        assert!(built.splits.len() > 1, "byte cap should force >1 split");
        built
    }

    #[test]
    fn bundle_inlines_every_split_boot_and_query_matches_cold() {
        let built = built_corpus();
        let files: HashMap<String, Vec<u8>> = built.splits.iter().cloned().collect();

        let mut rrhc = Vec::new();
        write_splitset_bundle(&mut rrhc, &built, 0, 1 << 20).unwrap();
        let hc = block_on(Hotcache::open(MemoryFetch::new(rrhc))).unwrap();

        // One inlined member per split (no manifest member — this is the 2-RT bundle), and every
        // split's boot resolves by name.
        assert_eq!(hc.members().len(), built.splits.len());
        assert!(hc.members().iter().all(|m| m.inlined));
        assert!(built
            .splits
            .iter()
            .all(|(name, _)| hc.inlined_by_name(name).is_some()));

        // Querying through the bundle (splits opened via their inlined boots, no header fetch)
        // returns exactly the cold-boot result: top-3 are the highest-ranked docs.
        let ss = block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap();
        let resolver = BundleResolver { files, hc };
        assert_eq!(
            block_on(ss.search(&resolver, "abc", 3)).unwrap(),
            vec![0, 1, 2]
        );
        let all = block_on(ss.search(&resolver, "abc", 1000)).unwrap();
        assert_eq!(all, (0..30).collect::<Vec<u32>>());
    }

    #[test]
    fn max_splits_caps_inlined_members_and_rest_cold_open() {
        let built = built_corpus();
        let files: HashMap<String, Vec<u8>> = built.splits.iter().cloned().collect();

        // Inline all but the last split; the last is absent from the bundle (and must cold-open).
        let n = built.splits.len();
        let mut rrhc = Vec::new();
        write_splitset_bundle(&mut rrhc, &built, n - 1, 1 << 20).unwrap();
        let hc = block_on(Hotcache::open(MemoryFetch::new(rrhc))).unwrap();
        assert_eq!(hc.members().len(), n - 1);
        assert!(hc.inlined_by_name(&built.splits[0].0).is_some());
        assert!(hc.inlined_by_name(&built.splits[n - 1].0).is_none());

        // A full-page query still returns every doc: the inlined splits boot for free, the rest
        // cold-open (their `boot` lookup returns `None`) — identical results either way.
        let ss = block_on(SplitSet::open(MemoryFetch::new(built.manifest.clone()))).unwrap();
        let resolver = BundleResolver { files, hc };
        let all = block_on(ss.search(&resolver, "abc", 1000)).unwrap();
        assert_eq!(all, (0..30).collect::<Vec<u32>>());
    }

    #[test]
    fn empty_split_set_writes_an_empty_bundle() {
        let built = BuiltSplitSet {
            manifest: Vec::new(),
            splits: Vec::new(),
            facets: Vec::new(),
        };
        let mut rrhc = Vec::new();
        write_splitset_bundle(&mut rrhc, &built, 0, 1 << 20).unwrap();
        let hc = block_on(Hotcache::open(MemoryFetch::new(rrhc))).unwrap();
        assert!(hc.members().is_empty());
    }
}
