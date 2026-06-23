# Task 046 — Go RRIL (`.rril`) lookup builder

Port the Rust `write_lookup` / `write_lookup_streaming` (`rust/src/lookup.rs`) to
`go/` — the external-key → doc-IDs reverse lookup (e.g. DOI → docs the OpenAlex demo
uses at `lookup.lookup(doi)`). Go currently has a reader-side `lookup` reference but
no writer. Byte-for-byte vs Rust, with a shared `go/testdata/rril_*_golden.txt`
asserted by both sides (the `rrss_build_golden.txt` pattern). Part of the Go
build-side gap set (see 045).
