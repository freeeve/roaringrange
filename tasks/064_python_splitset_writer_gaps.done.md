# 064: fix(python): SplitSetWriter case_sensitive + resume; head_boundary validation; build peak memory

**Severity: MED (silent case-mode divergence).** `python/src/lib.rs` + core `rust/src/splitset_write.rs`. Line refs @ 849f9c2.

## Findings

1. **MED -- `python/src/lib.rs:963`: `SplitSetWriter` hardcodes `case_sensitive: false`** in `WriterConfig`, and `resume` (:974-995) doesn't accept it either -- core `SplitSetWriter::resume` (`rust/src/splitset_write.rs:157`) has no parameter for it. Meanwhile `SplitSetBuilder`/`TermSplitSetBuilder` (:704, :819) DO expose it (task 054 added the config knob). Resuming a case-sensitive split set from Python silently builds case-FOLDED delta splits -> delta queries disagree with the base, no error, no warning. Fix: plumb `case_sensitive` through the Python writer ctor AND core `resume`; better, have `resume` read the case flag from the existing manifest (RRSS manifest bit4 per task 054) and reject/ignore a contradicting caller value.
2. **LOW -- `python/src/lib.rs:95-99`: `Builder` head_boundary contract unvalidated.** Docstring says it "must be a multiple of 65536" but `new` only applies `.max(DEFAULT_HEAD_BOUNDARY)`; `split_posting` (`rust/src/build.rs:48`) accepts anything -> a non-multiple silently produces a head straddling a roaring container. Validate (error) in the Python ctor and/or core.
3. **LOW -- `python/src/lib.rs:177`: `Builder::build` doubles peak memory.** `records[doc_id] = doc.record.clone()` copies every staged record while the originals stay alive in `self.docs` (build takes `&self`) -- an in-memory build peaks at ~2x record bytes. Make it `build(&mut self)` + `mem::take` (breaking for repeat-build callers -- check none exist; if some do, document single-shot).
4. Related core cleanup (fold in or note): `splitset_write.rs:48-49` `WriterConfig.head_boundary` is DEAD -- documented ("0 -> DEFAULT_HEAD_BOUNDARY") but never stored in `new` and explicitly ignored (`_head_boundary`) in `resume` (v3 RRS has no head/tail split). Remove the field or make the doc say it's ignored.

## Acceptance

- Python test: build a case-sensitive split set, resume + add a delta from Python, query for a case-distinct term -- delta results must respect case sensitivity (and a wrong caller value must error).
- head_boundary non-multiple raises.
- Goldens/conformance unchanged for default (case-folded) paths.

## Outcome (DONE)

Correction to the review's premise: the **core** `SplitSetWriter::resume` already
inherits case sensitivity from the manifest (`prev.flags() & FLAG_CASE_SENSITIVE`,
`splitset_write.rs:224`), and `SplitSet::flags()` reads it from the header -- so the
Python `resume` path was already correct. The only real bug was that Python
`SplitSetWriter.new` hardcoded `case_sensitive: false`, making a case-sensitive
split set impossible to create from Python at all.

- **item 1** `python/src/lib.rs`: `SplitSetWriter.new` gained a `case_sensitive=False`
  parameter threaded into `WriterConfig`. `resume` needs no parameter (inherits from
  the manifest); its docstring now says so.
- **item 2** `Builder.new` now returns `PyResult` and raises `ValueError` when
  `head_boundary` is not a multiple of 65536 (it splits the facet head/tail, so an
  off-container value would straddle a roaring container).
- **item 3** `Builder.build` streams records via the core `RecordWriter` straight
  from the staged docs in doc-ID order -- no intermediate `records` Vec of clones, so
  peak memory is ~1x the record bytes not 2x, and `self.docs` is left intact for a
  repeat build (no `build(&mut self)` breakage).
- **item 4** the dead `WriterConfig.head_boundary` (v3 delta splits have no head/tail
  split) is now documented as accepted-but-ignored in the core and the Python
  `new`/`resume` signatures, rather than removed (non-breaking).

Tests (`python/tests/test_roaringrange.py`, run via `maturin develop` + pytest, all
22 pass): `test_splitset_writer_case_sensitive_flag_set_and_inherited` (default =
manifest flag clear + RRSI v3; `case_sensitive=True` = flag set + v4; resume inherits
both) and `test_builder_rejects_non_container_head_boundary`. No byte-output changes
on the default paths; core (splits) fmt/tests unaffected (doc-only core change).
