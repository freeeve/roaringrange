package roaringrange

import "io"

// Registers the two formats that already had Go readers (RRSI index, RRSR record
// store) with the magic->format registry, so DetectFormat/OpenHeader and the CLI
// recognize them alongside the readers added for the other formats. The describe
// functions read only the fixed header.

func init() {
	register(Format{Magic: Magic, Name: "index", Ext: ".rrs", Describe: describeIndex})
	register(Format{Magic: MagicRecord, Name: "records", Ext: ".rrsr", Describe: describeRecords})
}

// describeIndex reads the RRSI (trigram/split index) header for `info`. v4 appends a
// 2-byte flags field marking a case-sensitive index.
func describeIndex(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, Magic, headerSize)
	if err != nil {
		return nil, err
	}
	version := u16(h[4:6])
	caseFold := true
	switch version {
	case Version:
	case VersionV4:
		flags := make([]byte, 2)
		if _, err := r.ReadAt(flags, headerSize); err != nil {
			return nil, err
		}
		caseFold = u16(flags)&rrsiFlagCaseSensitive == 0
	default:
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: Magic, Name: "index", Ext: ".rrs", Version: version,
		Fields: []Field{
			{"gramSize", u16(h[6:8])},
			{"ngrams", u32(h[8:12])},
			{"stride", u32(h[12:16])},
			{"caseSensitive", !caseFold},
		},
	}, nil
}

// describeRecords reads the RRSR record-store index header for `info`. The record
// blob and optional dictionary are separate files, so `info` reports only the count
// and version (1 = raw, 2 = framed/zstd).
func describeRecords(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, MagicRecord, recordHeaderSize)
	if err != nil {
		return nil, err
	}
	version := u16(h[4:6])
	if version != VersionRecord && version != versionRecordFramed {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: MagicRecord, Name: "records", Ext: ".rrsr", Version: version,
		Fields: []Field{
			{"records", u32(h[8:12])},
			{"framed", version == versionRecordFramed},
		},
	}, nil
}
