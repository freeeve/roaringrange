//! Emit the **catalog boot bundle** (`RRHC`) for a monolith composition — the one-GET boot
//! for the OpenAlex demo's 4–5 cold opens (task 006 step 2). Slices each member's boot
//! region from the built artifacts and inlines them all:
//!
//!   * `RRS`  trigram index — header + sparse index (`rrs_boot_len`)
//!   * `RRSF` facet sidecar — header + field/category tables + string blob (`rrsf_boot_len`)
//!   * `RRSR` record offset index — its 16-byte header (the frozen `RECORDS.md` header)
//!   * `RRSR` zstd dictionary — the whole `.dict` sidecar
//!   * `RRIL` DOI lookup — its 16-byte header (the frozen `LOOKUP.md` header)
//!
//! Inputs may be the full artifacts **or prefix files** holding at least the boot region
//! (e.g. a ranged `curl` of a deployed object) — only `[0, bootLen)` is read. Member names
//! in the bundle are each input's basename, which must match the names the demo fetches.
//!
//!   cargo run --release --features hotcache --example build_catalog_rrhc -- \
//!     <openalex-full.rrs> <openalex-full.rrf> <records-full.idx> <records-full.dict> \
//!     <openalex-full.rril> <out.rrhc>

use roaringrange::facet::rrsf_boot_len;
use roaringrange::hotcache::MemberTag;
use roaringrange::hotcache_build::{write_hotcache, MemberSpec};
use roaringrange::index::rrs_boot_len;
use std::os::unix::fs::FileExt;

/// Reads `[0, len)` of `path` (which may be a prefix file at least that long).
fn read_prefix(path: &str, len: usize) -> Vec<u8> {
    let f = std::fs::File::open(path).expect("open input");
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, 0)
        .unwrap_or_else(|e| panic!("{path}: need at least {len} boot bytes: {e}"));
    buf
}

fn base_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .expect("input has a file name")
        .to_string_lossy()
        .into_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        eprintln!(
            "usage: build_catalog_rrhc <rrs> <rrf> <idx> <dict> <rril> <out.rrhc>\n\
             (inputs may be prefix files covering at least each boot region)"
        );
        std::process::exit(2);
    }
    let (rrs, rrf, idx, dict, rril, out) =
        (&args[1], &args[2], &args[3], &args[4], &args[5], &args[6]);

    let rrs_len = rrs_boot_len(&read_prefix(rrs, 16)).expect("rrs header");
    let rrf_len = rrsf_boot_len(&read_prefix(rrf, 24)).expect("rrf header");
    let dict_len = std::fs::metadata(dict).expect("dict metadata").len() as usize;

    // The 16-byte RRSR / RRIL headers are frozen in RECORDS.md / LOOKUP.md.
    let members = vec![
        member(MemberTag::Rrs, rrs, rrs_len),
        member(MemberTag::Rrsf, rrf, rrf_len),
        member(MemberTag::RrsrIdx, idx, 16),
        member(MemberTag::RrsrDict, dict, dict_len),
        member(MemberTag::Rril, rril, 16),
    ];
    for m in &members {
        eprintln!(
            "  {:>10}  {:>12} B  {}",
            format!("{:?}", m.tag),
            m.boot_bytes.len(),
            m.data_file
        );
    }

    let mut buf = Vec::new();
    // Inline everything: the largest member (the rrs sparse / rrf meta, MBs) is
    // exactly what the one-GET boot exists to carry.
    write_hotcache(&mut buf, &members, u32::MAX).expect("write rrhc");
    std::fs::write(out, &buf).expect("write out");
    eprintln!(
        "wrote {out}: {} members, {:.2} MB — upload content-hashed + immutable next to the artifacts",
        members.len(),
        buf.len() as f64 / (1u64 << 20) as f64
    );
}

fn member(tag: MemberTag, path: &str, boot_len: usize) -> MemberSpec {
    MemberSpec {
        tag,
        data_file: base_name(path),
        boot_off: 0,
        boot_len: boot_len as u32,
        boot_bytes: read_prefix(path, boot_len),
    }
}
