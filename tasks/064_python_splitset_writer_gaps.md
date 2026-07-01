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
