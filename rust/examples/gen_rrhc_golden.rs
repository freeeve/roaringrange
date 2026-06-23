//! Generates the RRHC (`.rrhc`) conformance golden for the Go hotcache builder (task
//! 047): four members with a mix of inlined (boot ≤ threshold) and range-referenced
//! (boot > threshold) boots, printed as `rrhc <hex>`. Saved to
//! go/testdata/rrhc_build_golden.txt, asserted by both go/hotcache_test.go and the
//! Rust build_tests::rrhc_golden_matches (feature `hotcache`).
//!
//!   cargo run --release --features hotcache --example gen_rrhc_golden

use roaringrange::hotcache::MemberTag;
use roaringrange::hotcache_build::{write_hotcache, MemberSpec};

pub fn build_golden() -> Vec<u8> {
    let members = vec![
        MemberSpec {
            tag: MemberTag::Rrs,
            data_file: "a.rrs".to_string(),
            boot_off: 16,
            boot_len: 8,
            boot_bytes: vec![0xA0; 8],
        },
        MemberSpec {
            tag: MemberTag::Rrti,
            data_file: "terms.rrt".to_string(),
            boot_off: 16,
            boot_len: 16,
            boot_bytes: vec![0xB1; 16],
        },
        MemberSpec {
            tag: MemberTag::Rrvi,
            data_file: "vec.rrvi".to_string(),
            boot_off: 48,
            boot_len: 40,
            boot_bytes: vec![0xC2; 40],
        },
        MemberSpec {
            tag: MemberTag::RrsrIdx,
            data_file: "records.idx".to_string(),
            boot_off: 0,
            boot_len: 4,
            boot_bytes: vec![0xD3; 4],
        },
    ];
    let mut out = Vec::new();
    write_hotcache(&mut out, &members, 16).expect("write_hotcache");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rrhc {hex}");
}
