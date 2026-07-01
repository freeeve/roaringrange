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
