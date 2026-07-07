package roaringrange

import "io"

// The read side of the RRSS split-set manifest (the inverse of WriteSplitSet in
// splitset.go). The manifest is small, so it is parsed whole: the header, split
// directory, name blob, and summary blob are read up front and rehydrated into the
// SplitSpec/SplitSetConfig/SortColSpec values WriteSplitSet consumes, so
// WriteSplitSet(OpenSplitSet(x).Splits, .Config) reproduces x. The split bodies
// themselves live in the named data files. Mirrors rust/src/splitset.rs. See SPLITSET.md.

func init() {
	register(Format{Magic: MagicSplitSet, Name: "splitset", Ext: ".rrss", Describe: describeSplitSet})
}

// SplitSet is a reference reader over an RRSS manifest. The whole manifest is
// resident after Open.
type SplitSet struct {
	Config SplitSetConfig
	Splits []SplitSpec
}

// OpenSplitSet reads and validates the RRSS manifest, rehydrating the config and
// every split entry.
func OpenSplitSet(r io.ReaderAt) (*SplitSet, error) {
	h, err := readHeader(r, MagicSplitSet, splitSetHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != VersionSplitSet {
		return nil, ErrVersion
	}
	splitCount := int(u32(h[12:16]))
	strBytes := u32(h[20:24])
	summaryBytes := u64(h[24:32])

	entries, err := boundedRead(r, splitSetHeaderSize, uint64(splitCount)*splitEntrySize)
	if err != nil {
		return nil, err
	}
	strOff := int64(splitSetHeaderSize) + int64(splitCount)*splitEntrySize
	blob, err := boundedRead(r, strOff, uint64(strBytes))
	if err != nil {
		return nil, err
	}
	summaryBlob, err := boundedRead(r, strOff+int64(strBytes), summaryBytes)
	if err != nil {
		return nil, err
	}

	str := func(off uint32, ln uint16) (string, bool) {
		if uint64(off)+uint64(ln) > uint64(len(blob)) {
			return "", false
		}
		return string(blob[off : uint32(off)+uint32(ln)]), true
	}

	cfg := SplitSetConfig{
		Policy:    int(h[8]),
		BodyKind:  h[9],
		TierCount: u16(h[10:12]),
		BaseCount: u32(h[16:20]),
		ByteCap:   u64(h[32:40]),
		GramSize:  u16(h[49:51]),
		Flags:     u16(h[6:8]),
	}
	if sortcolNameLen := u16(h[44:46]); sortcolNameLen > 0 {
		name, ok := str(u32(h[40:44]), sortcolNameLen)
		if !ok {
			return nil, ErrTruncated
		}
		cfg.SortCol = &SortColSpec{
			Name:       name,
			Column:     u16(h[46:48]),
			Descending: h[48]&SortColFlagDescending != 0,
		}
	}

	splits := make([]SplitSpec, splitCount)
	for i := range splits {
		e := entries[i*splitEntrySize:]
		name, ok := str(u32(e[0:4]), u16(e[4:6]))
		if !ok {
			return nil, ErrTruncated
		}
		s := SplitSpec{
			DataFile: name,
			Tier:     u16(e[6:8]),
			DocCount: u32(e[8:12]),
			DocIDLo:  u32(e[12:16]),
			DocIDHi:  u32(e[16:20]),
			Flags:    u16(e[20:22]),
			ByteSize: u64(e[24:32]),
			Epoch:    u64(e[32:40]),
		}
		if summaryLen := u32(e[48:52]); summaryLen > 0 {
			off := u64(e[40:48])
			if off+uint64(summaryLen) > uint64(len(summaryBlob)) {
				return nil, ErrTruncated
			}
			s.Summary = summaryBlob[off : off+uint64(summaryLen)]
		}
		splits[i] = s
	}
	return &SplitSet{Config: cfg, Splits: splits}, nil
}

// bodyKindName maps the body-kind byte to a display name.
func bodyKindName(k uint8) string {
	if k == BodyKindTerm {
		return "term"
	}
	return "trigram"
}

// describeSplitSet reads only the RRSS header for `info`.
func describeSplitSet(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, MagicSplitSet, splitSetHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != VersionSplitSet {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: MagicSplitSet, Name: "splitset", Ext: ".rrss", Version: u16(h[4:6]),
		Fields: []Field{
			{"bodyKind", bodyKindName(h[9])},
			{"splits", u32(h[12:16])},
			{"tiers", u16(h[10:12])},
			{"baseCount", u32(h[16:20])},
			{"caseSensitive", u16(h[6:8])&SplitSetFlagCaseSensitive != 0},
		},
	}, nil
}
