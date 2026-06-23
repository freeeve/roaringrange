# Task 048 — Go RRSC sortcols + secondary (date-desc) builders

Port the Rust `write_sortcols` / `write_perm` (`rust/src/sortcols.rs`) and the
secondary date-descending index (`rust/src/secondary.rs`) to `go/`. Go has the
`SortColSpec` *manifest reference* (`splitset.go`) but no standalone column writer,
and no secondary index. Byte-for-byte vs Rust, shared
`go/testdata/rrsc_*_golden.txt` asserted by both sides. Part of the Go build-side
gap set (see 045).
