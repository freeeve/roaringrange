//! Generates the RRIL (`.rril`) conformance golden for the Go lookup builder (task
//! 046). Builds the index over a fixed set of (identifier, doc) pairs that exercise
//! normalization (dashes/colons/case dropped or upcased), empty-drop, and
//! same-hash/different-doc sorting, then prints `rril <hex>` — saved to
//! `go/testdata/rril_build_golden.txt` and asserted by both `go/lookup_test.go` and
//! the Rust `build::tests` golden test.
//!
//!   cargo run --release --example gen_rril_golden

use roaringrange::build::write_lookup;

/// Shared fixed entries — kept byte-identical to `go/lookup_test.go`.
pub const ENTRIES: &[(&str, u32)] = &[
    ("978-0-13-468599-1", 5),
    ("B07XYZ1234", 2),
    ("978-0-13-468599-1", 9), // dup id, later doc — same hash, sorts after doc 5
    ("isbn:0262033844", 7),
    ("", 3),    // normalizes to empty — dropped
    ("!!!", 4), // all punctuation — dropped
    ("AbC123", 1),
    ("b07xyz1234", 8), // normalizes to B07XYZ1234 — same hash as doc 2
];

/// Builds the `.rril` bytes for [`ENTRIES`].
pub fn build_golden() -> Vec<u8> {
    let entries: Vec<(String, u32)> = ENTRIES.iter().map(|(s, d)| (s.to_string(), *d)).collect();
    let mut out = Vec::new();
    write_lookup(&mut out, &entries).expect("write_lookup");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rril {hex}");
}
