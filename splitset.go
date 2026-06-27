package roaringrange

import (
	"encoding/binary"
	"fmt"
	"io"
	"math"
)

// The RRSS split-set manifest writer — the Go build-side mirror of the Rust
// splitset_build::write_splitset, emitting the byte layout in SPLITSET.md. A split
// is a vanilla RRS over a doc subset in its own file; the manifest only names the
// splits and carries the cross-split pruning metadata (rank tier, doc-id range,
// byte size, supersession epoch) plus the base/delta boundary and the reserved
// per-split summary regions. The bytes are identical to the Rust writer's, proven
// by the shared golden in splitset_test.go.

const (
	// MagicSplitSet is the RRSS split-set manifest magic.
	MagicSplitSet = "RRSS"
	// VersionSplitSet is the RRSS format version number.
	VersionSplitSet = 1
	// splitSetHeaderSize is the fixed RRSS header size in bytes.
	splitSetHeaderSize = 64
	// splitEntrySize is the size of one split-table entry in bytes.
	splitEntrySize = 56

	// PolicyTiered assigns docs to splits by rank (top-cited first).
	PolicyTiered = 0
	// PolicyStableKey assigns docs by ingest order; rank is a query-time RRSC column.
	PolicyStableKey = 1

	// BodyKindTrigram marks split data files as trigram RRS indexes (header byte 9; the
	// default, so older manifests read back as trigram). Go only builds trigram bodies.
	BodyKindTrigram = 0
	// BodyKindTerm marks split data files as term RRTI (FST) indexes (header byte 9).
	BodyKindTerm = 1

	// SplitSetFlagBloom marks per-split term Bloom-filter summaries present (header flag).
	SplitSetFlagBloom = 1 << 0
	// SplitSetFlagFacet marks per-split facet-presence summaries present (header flag).
	SplitSetFlagFacet = 1 << 1
	// SplitSetFlagTime marks per-split time min/max summaries present (header flag).
	SplitSetFlagTime = 1 << 2
	// SplitSetFlagTombstones marks per-split tombstone postings present (header flag).
	SplitSetFlagTombstones = 1 << 3
	// SplitSetFlagCaseSensitive marks a case-sensitive split set: n-gram and facet keys were
	// not lowercased, so a query derives keys without folding. Unset (the default) keeps every
	// manifest byte-identical. Mirrors the Rust splitset::FLAG_CASE_SENSITIVE.
	SplitSetFlagCaseSensitive = 1 << 4

	// SplitFlagHasTombstone marks a split that carries a tombstone posting (per-split flag).
	SplitFlagHasTombstone = 1 << 0
	// SplitFlagAbsoluteIDs marks a split that stores absolute global doc IDs (per-split flag).
	SplitFlagAbsoluteIDs = 1 << 1

	// SortColFlagDescending marks a descending rank sort-column (higher value = better rank).
	SortColFlagDescending = 1 << 0
)

// SplitSpec is one split recorded in the manifest. The split's RRS bytes live in
// its own DataFile and are not passed here.
type SplitSpec struct {
	DataFile string // the split's RRS data-file name
	Tier     uint16 // rank tier (tiered policy; 0 for stable-key / delta)
	DocCount uint32 // docs in the split
	DocIDLo  uint32 // min global doc id present (inclusive); the local-id base
	DocIDHi  uint32 // max global doc id present (inclusive)
	Epoch    uint64 // flush/build epoch (supersession ordering)
	ByteSize uint64 // the split RRS file size in bytes
	Flags    uint16 // per-split flags (SplitFlagHasTombstone | SplitFlagAbsoluteIDs)
	Summary  []byte // opaque summary TLV bytes (Bloom / facet / time / tombstone)
}

// SortColSpec is the stable-key rank source recorded in the manifest header.
type SortColSpec struct {
	Name       string // the RRSC data-file name holding the rank column
	Column     uint16 // column index within that RRSC
	Descending bool   // whether a higher value ranks better
}

// SplitSetConfig is the manifest-level configuration.
type SplitSetConfig struct {
	Policy    int          // PolicyTiered | PolicyStableKey
	BodyKind  uint8        // BodyKindTrigram (RRS) | BodyKindTerm (RRTI); 0 keeps older manifests byte-identical
	TierCount uint16       // number of rank tiers (tiered); 0 for stable-key
	BaseCount uint32       // splits [0, BaseCount) are base, the rest delta
	ByteCap   uint64       // the per-split byte cap (informational)
	GramSize  uint16       // n-gram window the splits were built with (for Bloom pruning); 0 for a term-bodied set
	SortCol   *SortColSpec // stable-key rank source, or nil
	Flags     uint16       // header summary-presence flags
}

// WriteSplitSet writes the RRSS manifest for splits to w, in the order given
// (base splits first, then delta splits — BaseCount marks the boundary). The
// output is byte-for-byte identical to the Rust writer for the same inputs. See
// SPLITSET.md.
func WriteSplitSet(w io.Writer, splits []SplitSpec, config SplitSetConfig) error {
	if int(config.BaseCount) > len(splits) {
		return fmt.Errorf("RRSS base_count %d exceeds split count %d", config.BaseCount, len(splits))
	}
	if len(splits) > math.MaxUint32 {
		return fmt.Errorf("RRSS split count exceeds the 32-bit limit")
	}

	// String blob: split data-file names in order, then the optional sort-column name.
	var stringBlob []byte
	nameOffs := make([]uint32, len(splits))
	nameLens := make([]uint16, len(splits))
	pushName := func(name string) (uint32, uint16, error) {
		off := len(stringBlob)
		if off > math.MaxUint32 {
			return 0, 0, fmt.Errorf("RRSS string blob exceeds the 32-bit limit")
		}
		if len(name) > math.MaxUint16 {
			return 0, 0, fmt.Errorf("RRSS name exceeds the 16-bit length limit")
		}
		stringBlob = append(stringBlob, name...)
		return uint32(off), uint16(len(name)), nil
	}
	for i := range splits {
		off, l, err := pushName(splits[i].DataFile)
		if err != nil {
			return err
		}
		nameOffs[i], nameLens[i] = off, l
	}
	var sortcolNameOff uint32
	var sortcolNameLen, sortcolColumn uint16
	var sortcolFlags byte
	if config.SortCol != nil {
		off, l, err := pushName(config.SortCol.Name)
		if err != nil {
			return err
		}
		sortcolNameOff, sortcolNameLen = off, l
		sortcolColumn = config.SortCol.Column
		if config.SortCol.Descending {
			sortcolFlags = SortColFlagDescending
		}
	}

	// Summary blob: each non-empty split summary appended; the rest record (0, 0).
	var summaryBlob []byte
	summaryOffs := make([]uint64, len(splits))
	summaryLens := make([]uint32, len(splits))
	for i := range splits {
		s := splits[i].Summary
		if len(s) == 0 {
			continue
		}
		if len(s) > math.MaxUint32 {
			return fmt.Errorf("RRSS split summary exceeds the 32-bit limit")
		}
		summaryOffs[i] = uint64(len(summaryBlob))
		summaryLens[i] = uint32(len(s))
		summaryBlob = append(summaryBlob, s...)
	}

	// Header (64 B).
	hdr := make([]byte, splitSetHeaderSize)
	copy(hdr[0:4], MagicSplitSet)
	binary.LittleEndian.PutUint16(hdr[4:], VersionSplitSet)
	binary.LittleEndian.PutUint16(hdr[6:], config.Flags)
	hdr[8] = byte(config.Policy)
	hdr[9] = config.BodyKind // 0 = trigram RRS, 1 = term RRTI
	binary.LittleEndian.PutUint16(hdr[10:], config.TierCount)
	binary.LittleEndian.PutUint32(hdr[12:], uint32(len(splits)))
	binary.LittleEndian.PutUint32(hdr[16:], config.BaseCount)
	binary.LittleEndian.PutUint32(hdr[20:], uint32(len(stringBlob)))
	binary.LittleEndian.PutUint64(hdr[24:], uint64(len(summaryBlob)))
	binary.LittleEndian.PutUint64(hdr[32:], config.ByteCap)
	binary.LittleEndian.PutUint32(hdr[40:], sortcolNameOff)
	binary.LittleEndian.PutUint16(hdr[44:], sortcolNameLen)
	binary.LittleEndian.PutUint16(hdr[46:], sortcolColumn)
	hdr[48] = sortcolFlags
	binary.LittleEndian.PutUint16(hdr[49:], config.GramSize)
	// hdr[51:56] is pad1; hdr[56:64] is reserved.
	if _, err := w.Write(hdr); err != nil {
		return err
	}

	// Split entries (56 B each), in split order.
	for i := range splits {
		s := &splits[i]
		e := make([]byte, splitEntrySize)
		binary.LittleEndian.PutUint32(e[0:], nameOffs[i])
		binary.LittleEndian.PutUint16(e[4:], nameLens[i])
		binary.LittleEndian.PutUint16(e[6:], s.Tier)
		binary.LittleEndian.PutUint32(e[8:], s.DocCount)
		binary.LittleEndian.PutUint32(e[12:], s.DocIDLo)
		binary.LittleEndian.PutUint32(e[16:], s.DocIDHi)
		binary.LittleEndian.PutUint16(e[20:], s.Flags)
		// e[22:24] is pad.
		binary.LittleEndian.PutUint64(e[24:], s.ByteSize)
		binary.LittleEndian.PutUint64(e[32:], s.Epoch)
		binary.LittleEndian.PutUint64(e[40:], summaryOffs[i])
		binary.LittleEndian.PutUint32(e[48:], summaryLens[i])
		// e[52:56] is reserved.
		if _, err := w.Write(e); err != nil {
			return err
		}
	}

	if _, err := w.Write(stringBlob); err != nil {
		return err
	}
	_, err := w.Write(summaryBlob)
	return err
}
