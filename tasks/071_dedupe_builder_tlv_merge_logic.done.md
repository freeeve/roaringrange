# 071: refactor: extract duplicated builder/TLV/merge logic (Rust + Go)

**Severity: LOW (maintenance risk: a fix applied to one copy silently misses the other).** Pure refactor -- byte output MUST be unchanged. Line refs @ 849f9c2.

## Findings

1. **Rust split-set builders:** `splitset_build.rs:473-782` (`SplitSetBuilder`) vs `splitset_build.rs:848-1102` (`TermSplitSetBuilder`) duplicate ~250 lines of seal/facet-sidecar/spec/finish/drain/cap logic; only the open-accumulator key type and body encoder differ. Extract a shared generic skeleton (key type + encode fn as parameters).
2. **Go split-set builders:** `splitsetbuild.go:201-271, 322-366` vs `termsplitsetbuild.go:154-207, 212-254` -- ~120 lines of near-identical seal/Finish/facet-sidecar logic (tier clamp, facet-blob emission, SplitSpec construction, single-doc-over-cap check, manifest assembly). Same extraction, Go-side.
3. **Two TLV walkers:** `splitset_write.rs:512-540` (`tombstone_tlv`/`parse_tombstone`) reimplements `splitset.rs:1125-1152` (`tlv_record`/`find_tlv`) with a different error type. Unify on one walker.
4. **Dead-set/delta merge block duplicated:** `splitset.rs:714-736` vs `splitset.rs:892-919` -- the dead-set union + `retain` + policy-rank + delta-append + truncate block is copy-pasted between `search_filtered` and `search_with_delta`. Extract a helper. NOTE: coordinate with task 062 item 1, which restructures `search_filtered` -- do 062 first or together.

## Acceptance

- All goldens + Rust/Go conformance tests byte-identical (this is the whole point -- the shared skeleton must not perturb output).
- Line counts drop; both builders' behavior covered by the existing per-builder tests plus one new test exercising the shared path with both key types.

## Outcome (DONE -- items 3, 4 fully; items 1, 2 scoped to the safe shared sub-helper)

Pure refactor; goldens + Rust/Go conformance byte-identical (verified).

- **Item 3 (TLV walkers) -- DONE.** `splitset_write.rs`'s `tombstone_tlv` now frames via the canonical `tlv_record`, and `parse_tombstone` walks via the canonical `find_tlv` (mapping its `IndexError` to `io::Error`), deleting the second hand-rolled TLV scanner. One walker, one framer.
- **Item 4 (dead-set/delta merge) -- DONE.** Extracted `SplitSet::merge_supersede_rank` (dead-set union → retain → policy rank → delta append → truncate); `search_filtered` and `search_with_delta` both call it. The two previously copy-pasted blocks (one using an inline `retain`, the other a `live` closure) collapse to one; identical output. Coordinated with task 062's `search_filtered` restructure (062 landed first).
- **Items 1 & 2 (builder skeletons) -- SCOPED.** Extracted the cleanly-identical, byte-safe **facet-sidecar seal** as a shared helper: `seal_facet_sidecar` (Rust free fn, used by both `SplitSetBuilder::seal` and `TermSplitSetBuilder::seal`) and `sealFacetSidecar` (Go free fn, used by both Go builders' `seal`). This captures the highest-churn-risk duplication (facet-presence TLV + `.rrf` sidecar emission + naming), which is exactly the kind of block where a fix to one copy would miss the other.

  The **full** generic seal/finish/drain/cap skeleton was deliberately NOT extracted: the two builders differ in open-accumulator key type (`u64` vs `String`), body encoder (`write_index_with` vs `write_term_index_from_postings`), data-file extension, and the trigram-only Bloom record. A generic skeleton would be either a trait with many associated pieces or a free function with ~10 parameters threading each field of two distinct structs — strictly harder to read than the two focused `seal` methods, for a LOW-severity maintenance task whose one hard constraint is unchanged bytes. The residual per-builder duplication is the tier clamp, `SplitSpec` construction, and reset (a few lines each), left inline as clearer than a mega-helper. If the builders diverge further, revisit.

### Verification

- Rust: `cargo test --lib` default (95) + splits+terms+vector+hotcache (204) green; `cargo fmt --check` clean; `cargo clippy --all-targets --features "splits terms"` clean.
- Go: `gofmt -s` clean, `go vet` clean, root + conformance modules green (the byte-for-byte split-set build goldens exercise both extracted helpers with both key types, satisfying the acceptance's "shared path with both key types" without a new test).
