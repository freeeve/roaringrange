# Task 049 — Go records zstd encode (`write_records_zstd`)

Go `WriteRecords` writes only **raw** (tag-0) records; the Rust
`write_records_zstd` (`rust/src/build.rs`) writes tag-1 zstd frames against a shared
trained dictionary (the chunked-zstd full-corpus format — why the 115 GB store is
Rust-built today). Add the zstd encode + dictionary-train path to `go/`. Needs a
zstd encoder + dict trainer (cgo libzstd, or a pure-Go encoder if frame/dict bytes
can be made to match). Conformance is harder here (zstd frame bytes must match);
scope the matchability before committing. Lower priority than 045–048.
