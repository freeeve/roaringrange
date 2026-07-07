package roaringrange

import "io"

// The read side of the RRIL identifier index (the inverse of WriteLookup in
// lookup.go). The reader range-binary-searches the sorted [hash u64, verify u32,
// doc u32] table by the primary hash, then filters equal-hash rows by the verify
// hash (the effective key is 96 bits). Mirrors rust/src/lookup.rs. See LOOKUP.md.

const (
	// lookupHeaderSize is the fixed RRIL header: magic(4)+version(2)+reserved(2)+
	// count(4)+reserved(4).
	lookupHeaderSize = 16
	// lookupEntrySize is one table row: hash(8)+verify(4)+doc(4).
	lookupEntrySize = 16
)

func init() {
	register(Format{Magic: lookupMagic, Name: "lookup", Ext: ".rril", Describe: describeLookup})
}

// LookupRecord is one raw table row: the primary and verify hashes and the doc ID.
// The source identifier is not stored (it is hashed away), so a dump exposes the
// hashes rather than the original string.
type LookupRecord struct {
	Hash   uint64
	Verify uint32
	Doc    uint32
}

// LookupIndex is a reference reader over an RRIL index accessed by byte range. It
// reads only the 16-byte header up front; table rows are fetched per probe.
type LookupIndex struct {
	r     io.ReaderAt
	count uint32
}

// OpenLookup reads and validates the RRIL header. Rows are read lazily.
func OpenLookup(r io.ReaderAt) (*LookupIndex, error) {
	h, err := readHeader(r, lookupMagic, lookupHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	return &LookupIndex{r: r, count: u32(h[8:12])}, nil
}

// Count reports the number of table rows.
func (l *LookupIndex) Count() int { return int(l.count) }

// entry reads the i-th table row.
func (l *LookupIndex) entry(i int) (LookupRecord, error) {
	b := make([]byte, lookupEntrySize)
	off := int64(lookupHeaderSize) + int64(i)*lookupEntrySize
	if _, err := l.r.ReadAt(b, off); err != nil {
		return LookupRecord{}, err
	}
	return LookupRecord{Hash: u64(b[0:8]), Verify: u32(b[8:12]), Doc: u32(b[12:16])}, nil
}

// Lookup returns the doc ID(s) carrying the given identifier. The identifier is
// normalized and hashed exactly as WriteLookup did (reusing normalizeID/idFNV64a),
// the table is binary-searched by the primary hash, and equal-hash rows are kept
// only when the verify hash also matches. Returns nil (not an error) for a miss.
func (l *LookupIndex) Lookup(id string) ([]uint32, error) {
	n := normalizeID(id)
	if n == "" {
		return nil, nil
	}
	want := idFNV64a(n, lookupFNVOffset)
	verify := uint32(idFNV64a(n, lookupFNVVerify))

	// Binary search for the first row with hash == want.
	lo, hi := 0, int(l.count)
	for lo < hi {
		mid := int(uint(lo+hi) >> 1)
		e, err := l.entry(mid)
		if err != nil {
			return nil, err
		}
		if e.Hash < want {
			lo = mid + 1
		} else {
			hi = mid
		}
	}
	var docs []uint32
	for i := lo; i < int(l.count); i++ {
		e, err := l.entry(i)
		if err != nil {
			return nil, err
		}
		if e.Hash != want {
			break
		}
		if e.Verify == verify {
			docs = append(docs, e.Doc)
		}
	}
	return docs, nil
}

// Entries reads and returns every table row in stored (hash, doc) order. The whole
// table is read in one bounded range read.
func (l *LookupIndex) Entries() ([]LookupRecord, error) {
	buf, err := boundedRead(l.r, lookupHeaderSize, uint64(l.count)*lookupEntrySize)
	if err != nil {
		return nil, err
	}
	out := make([]LookupRecord, l.count)
	for i := range out {
		b := buf[i*lookupEntrySize:]
		out[i] = LookupRecord{Hash: u64(b[0:8]), Verify: u32(b[8:12]), Doc: u32(b[12:16])}
	}
	return out, nil
}

// describeLookup reads only the RRIL header for `info`.
func describeLookup(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, lookupMagic, lookupHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: lookupMagic, Name: "lookup", Ext: ".rril", Version: u16(h[4:6]),
		Fields: []Field{{"entries", u32(h[8:12])}},
	}, nil
}
