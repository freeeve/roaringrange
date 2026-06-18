//! Known-item sanity check: strict-AND [`search_bm25`] vs min-should-match
//! [`search_bm25_min_match`] (min=2) over a local `.rrt` + `.rrb` and an
//! `id<TAB>title[<TAB>...]` set (e.g. `/tmp/bm25ab/{terms.rrt,terms.rrb,titles.tsv}`).
//!
//! Each title is a navigational query whose relevant doc is its own `id`. Two
//! regimes: the exact title (both rankers should find it — min-match must not
//! regress), and a one-word **typo** (a query term goes out-of-vocab, so strict AND
//! resolves to an empty result while min-match drops the bad term and recovers on
//! the rest). Reports recall@k per regime plus how often a typo'd AND returns 0 and
//! min-match still finds the target. Absolute numbers are corpus-specific.
//!
//!   cargo run --release --features terms --example bm25_minmatch_eval -- \
//!     terms.rrt terms.rrb titles.tsv [N=3000] [M=2000] [K=10]

use futures::executor::block_on;
use roaringrange::bm25::{search_bm25, search_bm25_min_match, ImpactIndex};
use roaringrange::{FileFetch, TermIndex};
use std::io::{BufRead, BufReader};

/// Swaps the two middle characters of the longest word — a realistic typo that is
/// almost always out-of-vocabulary, so a strict-AND query containing it resolves
/// to empty while min-should-match simply drops it.
fn typo(words: &[String]) -> Vec<String> {
    let mut out: Vec<String> = words.to_vec();
    if let Some(i) = (0..out.len()).max_by_key(|&i| out[i].chars().count()) {
        let mut cs: Vec<char> = out[i].chars().collect();
        if cs.len() >= 4 {
            let mid = cs.len() / 2;
            cs.swap(mid - 1, mid);
            out[i] = cs.into_iter().collect();
        }
    }
    out
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: bm25_minmatch_eval RRT RRB TITLES.tsv [N=3000] [M=2000] [K=10]");
        std::process::exit(2);
    }
    let n: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);
    let m: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let k: usize = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(10);

    let terms = block_on(TermIndex::open(FileFetch::open(&a[0]).expect("rrt"))).expect("open rrt");
    let impacts =
        block_on(ImpactIndex::open(FileFetch::open(&a[1]).expect("rrb"))).expect("open rrb");

    let f = BufReader::new(std::fs::File::open(&a[2]).expect("titles"));
    let mut pairs: Vec<(u32, String)> = Vec::new();
    for line in f.lines().map_while(Result::ok) {
        let mut it = line.splitn(3, '\t');
        if let (Some(id), Some(title)) = (it.next(), it.next()) {
            if let Ok(id) = id.parse::<u32>() {
                pairs.push((id, title.to_string()));
            }
        }
    }
    // Deterministic stride sample (no RNG), only titles with >= 3 words (so a typo
    // leaves >= 2 terms for min-should-match).
    let stride = (pairs.len() / n).max(1);
    let sample: Vec<(u32, Vec<String>)> = pairs
        .iter()
        .step_by(stride)
        .filter_map(|(id, t)| {
            let w: Vec<String> = t.split_whitespace().map(str::to_string).collect();
            (w.len() >= 3).then_some((*id, w))
        })
        .take(n)
        .collect();

    println!(
        "{} multi-word known-item queries  (k={k}, m={m}, min_match=2)\n",
        sample.len()
    );
    for (label, noise) in [("exact", false), ("typo", true)] {
        let (mut and_hit, mut mm_hit, mut and_empty, mut recovered) = (0usize, 0, 0, 0);
        for (id, words) in &sample {
            let q = if noise { typo(words) } else { words.clone() }.join(" ");
            let and = block_on(search_bm25(&terms, &impacts, &q, m, k)).expect("bm25");
            let mm =
                block_on(search_bm25_min_match(&terms, &impacts, &q, m, k, 2)).expect("min_match");
            let in_and = and.iter().any(|s| s.doc_id == *id);
            let in_mm = mm.iter().any(|s| s.doc_id == *id);
            and_hit += in_and as usize;
            mm_hit += in_mm as usize;
            if and.is_empty() {
                and_empty += 1;
                recovered += in_mm as usize;
            }
        }
        let den = sample.len().max(1) as f64;
        let pct = |x: usize| 100.0 * x as f64 / den;
        println!(
            "[{label:>5}] recall@{k}:  AND {:5.1}%   min2 {:5.1}%   (Δ {:+.1} pp)",
            pct(and_hit),
            pct(mm_hit),
            pct(mm_hit) - pct(and_hit)
        );
        println!(
            "         AND returned 0 for {and_empty}; min2 recovered the target in {recovered}\n"
        );
    }
}
