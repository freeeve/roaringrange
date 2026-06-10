# 028 — Right-sized split-set Bloom summaries

**Status:** pending

`splitset_strip_summaries` dropped ALL summaries to get the manifest from 727 MB → 29 KB,
losing absent-term pruning entirely (a rare/absent-term split query now opens every tier-0
split). The 727 MB came from 10 bits/key over every split's full trigram vocabulary;
the right size is much smaller.

## Options (pick by measurement)

1. **Lower bits/key per split** (~3–4 bits/key ≈ 15–25% FP — pruning is advisory, an FP
   just costs one split open): manifest ≈ 727 MB × 0.35 ≈ 250 MB — still too big resident.
2. **Blooms in the `.rrhc`, not the manifest**: keep the 29 KB manifest; move per-split
   Blooms into the boot bundle as a lazily-fetched member, or as per-split sidecar files
   ranged on demand (only consulted for terms that survive tier-0 under-fill).
3. **One global Bloom over tier-0 vocabulary** (absent-from-the-whole-corpus check only):
   a single ~50–100 MB filter answers "no split can match" — but that's most of the win
   (genuinely absent/typo terms), at one ranged structure that itself could be probed by
   hash position (a Bloom probe needs k byte positions — ~k ranged byte reads! A Bloom is
   range-probeable by construction: fetch k single bytes, not the whole filter).

Option 3's range-probe property is the elegant one: absent-term checks for ~k × 1-byte
ranged reads against a filter that never loads.

## Acceptance

- Absent-term query in split mode opens 0 splits again (perf bar).
- Manifest stays ~29 KB resident; added boot cost ≤ a few KB.
- `splitset_strip_summaries` docs updated to describe what replaces the dropped Blooms.
