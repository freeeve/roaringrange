package roaringrange

// The Go build-side emitter for an RRHC boot bundle over a split set — the mirror of the
// Rust splitset_bundle::write_splitset_bundle, byte-for-byte. A split set names N immutable
// RRS splits; cold-booting opens each queried split with its own dependent header +
// sparse-index reads (two round trips per split over a CDN). The bundle inlines each split's
// boot region ([0, bootLen) = header + sparse index) as an RRS member keyed by the split's
// data-file name, so a reader booting via the bundle (RrssIndex.openBundle) opens every
// split with no header fetch: the N per-split opens collapse into the bundle's single GET.
// Split boots only (not the manifest), matching the 2-round-trip reader path where the
// manifest keeps its own GET. See rust/src/splitset_bundle.rs and HOTCACHE.md.

import (
	"encoding/binary"
	"fmt"
	"io"
)

// rrsBootLen returns the byte length of a split's RRS boot region — the header plus the
// sparse index, the [0, dictStart) prefix a from-boot open consumes — parsed from the
// split's leading bytes with the same checks as the Rust index::rrs_boot_len: short header,
// bad magic, unexpected version, zero stride.
func rrsBootLen(split []byte) (uint64, error) {
	if len(split) < headerSize {
		return 0, ErrTruncated
	}
	if string(split[0:4]) != Magic {
		return 0, ErrMagic
	}
	hdrSize := uint64(headerSize)
	switch binary.LittleEndian.Uint16(split[4:6]) {
	case Version:
	case VersionV4:
		hdrSize = headerSizeV4
	default:
		return 0, ErrVersion
	}
	ngrams := binary.LittleEndian.Uint32(split[8:12])
	stride := binary.LittleEndian.Uint32(split[12:16])
	if ngrams == 0 {
		return hdrSize, nil
	}
	if stride == 0 {
		return 0, ErrTruncated
	}
	sparseCount := (uint64(ngrams) + uint64(stride) - 1) / uint64(stride)
	return hdrSize + sparseCount*8, nil
}

// WriteSplitsetBundle writes an .rrhc boot bundle over built to dst — byte-for-byte with
// the Rust write_splitset_bundle: one inlined RRS member per split (in seal/rank order, so
// the top tiers come first), each carrying its split's boot region.
//
// maxSplits caps how many splits are inlined (0 = all): a corpus with a large top tier
// inlines only the splits a top-K query is likely to open, keeping the first GET small.
// inlineThreshold is forwarded to WriteHotcache: a split whose boot exceeds it is referenced
// rather than inlined, so its boot lookup misses and it simply cold-opens — the bundle never
// forces a fat first GET. Errors only on a malformed split header or an I/O failure.
func WriteSplitsetBundle(dst io.Writer, built *BuiltSplitSet, maxSplits int, inlineThreshold uint32) error {
	take := len(built.Splits)
	if maxSplits != 0 && maxSplits < take {
		take = maxSplits
	}
	specs := make([]MemberSpec, 0, take)
	for _, s := range built.Splits[:take] {
		bootLen, err := rrsBootLen(s.Bytes)
		if err != nil {
			return fmt.Errorf("split %q: %w", s.Name, err)
		}
		if bootLen > uint64(len(s.Bytes)) {
			return fmt.Errorf("split %q: boot region %d B exceeds the split's %d B",
				s.Name, bootLen, len(s.Bytes))
		}
		specs = append(specs, MemberSpec{
			Tag:       MemberRrs,
			DataFile:  s.Name,
			BootOff:   0,
			BootLen:   uint32(bootLen),
			BootBytes: s.Bytes[:bootLen],
		})
	}
	return WriteHotcache(dst, specs, inlineThreshold)
}
