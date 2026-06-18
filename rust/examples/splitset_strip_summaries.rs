//! Re-emit an `RRSS` manifest with its per-split **summary blob dropped**, over the SAME split
//! bodies. The trigram split set's per-split term-Bloom summaries bloated the manifest to ~727 MB
//! — and the reader makes the whole manifest resident at boot, so the split set was un-bootable in
//! a browser. Dropping the summaries shrinks the manifest to tens of KB (header + 56 B/entry +
//! the split-name string blob) so it boots in one small ranged read.
//!
//! What changes: only the manifest. The split `.rrs` data files, their doc-ID ranges, tiers, and
//! sizes are copied through unchanged, so every existing split keeps working and results stay
//! identical. What's lost: cross-split **Bloom/facet pruning** — a rare/absent-term query no
//! longer skips splits it can't match, so it may descend more tiers (still correct, just more
//! reads). The tier short-circuit (read tier 0 first) is unaffected, so common queries are fine.
//!
//! An optional `max_tier` additionally keeps only the splits whose rank tier is `<= max_tier`
//! — because doc ID == citation rank, a tier-0-only manifest IS a search over the top-cited
//! slice of the corpus, using the very same split bodies and record store already on S3
//! (a "lite" dataset for the cost of one small manifest).
//!
//!   cargo run --release --features splits --example splitset_strip_summaries -- <in.rrss> <out.rrss> [max_tier]

use roaringrange::{write_splitset, SplitSet, SplitSetConfig, SplitSpec};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: splitset_strip_summaries <in.rrss> <out.rrss> [max_tier]");
        std::process::exit(2);
    }
    let max_tier: Option<u16> = args.get(3).map(|s| s.parse().expect("max_tier (u16)"));
    let buf = std::fs::read(&args[1]).expect("read manifest");
    let ss = SplitSet::from_bytes(&buf).expect("parse manifest");
    assert!(
        ss.sortcol().is_none(),
        "sortcol present — stable-key sets aren't handled by this stripper"
    );
    assert!(
        max_tier.is_none() || ss.delta_splits().is_empty(),
        "tier filtering over a set with delta splits is not supported"
    );
    eprintln!(
        "in : {} splits, {:.1} MB manifest, {} tiers, bodyKind {:?}, gram {}",
        ss.splits().len(),
        buf.len() as f64 / (1u64 << 20) as f64,
        ss.tier_count(),
        ss.body_kind(),
        ss.gram_size(),
    );

    let splits: Vec<SplitSpec> = ss
        .splits()
        .iter()
        .filter(|s| max_tier.is_none_or(|mt| s.tier <= mt))
        .map(|s| {
            assert!(
                !s.has_tombstone() && !s.absolute_ids(),
                "split {} carries tombstone/absolute-id state tied to a summary — not strippable",
                s.data_file
            );
            SplitSpec {
                data_file: s.data_file.clone(),
                tier: s.tier,
                doc_count: s.doc_count,
                doc_id_lo: s.doc_id_lo,
                doc_id_hi: s.doc_id_hi,
                epoch: s.epoch,
                byte_size: s.byte_size,
                flags: 0,
                summary: Vec::new(),
            }
        })
        .collect();

    let docs: u64 = splits.iter().map(|s| s.doc_count as u64).sum();
    let config = SplitSetConfig {
        policy: ss.policy(),
        tier_count: max_tier.map_or(ss.tier_count(), |mt| mt + 1),
        base_count: splits.len() as u32,
        byte_cap: ss.byte_cap(),
        gram_size: ss.gram_size(),
        body_kind: ss.body_kind(),
        sortcol: None,
        flags: 0, // no summary-presence flags now that the blob is empty
    };

    let mut out = Vec::new();
    write_splitset(&mut out, &splits, &config).expect("write manifest");
    std::fs::write(&args[2], &out).expect("write out");
    eprintln!(
        "out: {} splits ({docs} docs{}), {:.1} KB manifest (summaries dropped) -> {}",
        splits.len(),
        max_tier.map_or(String::new(), |mt| format!(", tiers ≤ {mt}")),
        out.len() as f64 / 1024.0,
        args[2]
    );
}
