//! Generates the RRSC (`.rrsc`) conformance golden for the Go sortcols builder:
//! one column of each value type (u16/u32/i32/f32, incl. negative and
//! zero), printed as `rrsc <hex>`. Saved to go/testdata/rrsc_build_golden.txt and
//! asserted by both go/sortcols_test.go and Rust build_tests::rrsc_golden_matches.
//!
//!   cargo run --release --example gen_rrsc_golden

use roaringrange::build::{write_sortcols, ColumnValues, SortColumn};

pub fn build_golden() -> Vec<u8> {
    let cols = vec![
        SortColumn {
            name: "year".to_string(),
            values: ColumnValues::U16(vec![2020, 2019, 2021, 2018]),
        },
        SortColumn {
            name: "citations".to_string(),
            values: ColumnValues::U32(vec![100, 5, 9999, 0]),
        },
        SortColumn {
            name: "delta".to_string(),
            values: ColumnValues::I32(vec![-5, 10, -100, 42]),
        },
        SortColumn {
            name: "score".to_string(),
            values: ColumnValues::F32(vec![1.5, -2.25, 0.0, 3.5]),
        },
    ];
    let mut out = Vec::new();
    write_sortcols(&mut out, cols).expect("write_sortcols");
    out
}

fn main() {
    let hex: String = build_golden().iter().map(|b| format!("{b:02x}")).collect();
    println!("rrsc {hex}");
}
