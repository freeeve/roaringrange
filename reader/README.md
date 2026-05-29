# roaringrange_reader

Browser-side reader for the RRS v2 (`RRS2`) range-fetchable static search index.
See [`../FORMAT.md`](../FORMAT.md) for the frozen byte-layout contract.

The reader boots by downloading the 16-byte header plus the sparse index once
(tens of KB), then answers each query with a few small ranged reads:
in-memory sparse binary search → one ranged dictionary-block read → binary
search within the block → one ranged posting read per n-gram key. Postings are
standard **portable** RoaringBitmaps (`roaring` crate
`RoaringBitmap::deserialize_from`), byte-identical to the Go builder's output.

## Design: transport behind a trait

All byte access goes through the [`RangeFetch`](src/fetch.rs) trait:

```rust
pub trait RangeFetch {
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, FetchError>;
}
```

The core [`Index`](src/index.rs) is generic over `RangeFetch`, so it is
transport-agnostic. Native code and tests use the in-memory
[`MemoryFetch`](src/fetch.rs). A future WASM build supplies an HTTP-Range
implementation **without changing the core**.

## Building and testing (native / host target)

`wasm-bindgen` is an optional dependency only; the crate builds and tests on the
host target as-is:

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy -- -D warnings
```

## Deferred: WASM build

The WASM path is intentionally deferred (the `wasm32-unknown-unknown` target and
`wasm-pack` are not installed here). To enable it later:

1. Install the target and tooling:
   ```sh
   rustup target add wasm32-unknown-unknown
   cargo install wasm-pack
   ```
2. Add a `fetch()`-backed `RangeFetch` implementation behind the `wasm` feature
   (this crate already declares `wasm = ["dep:wasm-bindgen"]` and the optional
   `wasm-bindgen` dependency). The impl issues HTTP `Range: bytes=offset-end`
   requests against the index URL and returns the response bytes; it plugs into
   the same `Index<F>` core unchanged.
3. Build:
   ```sh
   wasm-pack build --target web --features wasm
   ```

No core code (header parsing, sparse lookup, head/tail reads, search) needs to
change for the WASM build — only a new `RangeFetch` impl and thin `wasm-bindgen`
exports.
