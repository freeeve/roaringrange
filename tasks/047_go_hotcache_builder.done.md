# Task 047 — Go RRHC (`.rrhc`) hotcache boot-bundle builder

Port the Rust `write_hotcache` (`rust/src/hotcache_build.rs`, `build_catalog_rrhc`
example) to `go/` — the boot bundle that inlines member indices' boot regions for a
1-RTT open. Byte-for-byte vs Rust, shared `go/testdata/rrhc_*_golden.txt` asserted by
both sides. Part of the Go build-side gap set (see 045).
