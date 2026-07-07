package roaringrange

import (
	"io"

	"github.com/RoaringBitmap/roaring/v2"
)

// The read side of the RRSF facet sidecar (the inverse of WriteFacets in
// facets.go). The field and category directories plus the name blob are read up
// front; each category's doc posting (split head/tail portable RoaringBitmaps) is
// range-read and merged on demand. Mirrors rust/src/facet.rs. See FACETS.md.

func init() {
	register(Format{Magic: MagicFacet, Name: "facets", Ext: ".rrf", Describe: describeFacets})
}

// FacetFieldMeta is one facet field's directory entry.
type FacetFieldMeta struct {
	Name     string
	CatStart uint32
	CatCount uint32
}

// FacetCatMeta is one category's directory entry (without its posting bytes).
type FacetCatMeta struct {
	Key         uint64
	Name        string
	Cardinality uint32
	headOff     uint64
	headSize    uint32
	tailSize    uint32
}

// FacetIndex is a reference reader over an RRSF sidecar accessed by byte range. The
// directories are resident after Open; postings are fetched per Posting call.
type FacetIndex struct {
	r             io.ReaderAt
	CaseSensitive bool
	fields        []FacetFieldMeta
	cats          []FacetCatMeta
}

// OpenFacets reads and validates the RRSF header and both directories. Postings are
// read lazily.
func OpenFacets(r io.ReaderAt) (*FacetIndex, error) {
	h, err := readHeader(r, MagicFacet, facetHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != VersionFacet {
		return nil, ErrVersion
	}
	caseSensitive := u16(h[6:8])&facetFlagCaseSensitive != 0
	fieldCount := int(u32(h[8:12]))
	catCount := int(u32(h[12:16]))
	strBytes := u32(h[16:20])

	fieldTable, err := boundedRead(r, facetHeaderSize, uint64(fieldCount)*facetFieldEntry)
	if err != nil {
		return nil, err
	}
	catOff := int64(facetHeaderSize) + int64(fieldCount)*facetFieldEntry
	catTable, err := boundedRead(r, catOff, uint64(catCount)*facetCatEntry)
	if err != nil {
		return nil, err
	}
	strOff := catOff + int64(catCount)*facetCatEntry
	blob, err := boundedRead(r, strOff, uint64(strBytes))
	if err != nil {
		return nil, err
	}
	str := func(off uint32, ln uint16) (string, bool) {
		if uint64(off)+uint64(ln) > uint64(len(blob)) {
			return "", false
		}
		return string(blob[off : uint32(off)+uint32(ln)]), true
	}

	fields := make([]FacetFieldMeta, fieldCount)
	for i := range fields {
		e := fieldTable[i*facetFieldEntry:]
		name, ok := str(u32(e[0:4]), u16(e[4:6]))
		if !ok {
			return nil, ErrTruncated
		}
		fields[i] = FacetFieldMeta{Name: name, CatStart: u32(e[8:12]), CatCount: u32(e[12:16])}
	}
	cats := make([]FacetCatMeta, catCount)
	for i := range cats {
		e := catTable[i*facetCatEntry:]
		name, ok := str(u32(e[28:32]), u16(e[32:34]))
		if !ok {
			return nil, ErrTruncated
		}
		cats[i] = FacetCatMeta{
			Key:         u64(e[0:8]),
			headOff:     u64(e[8:16]),
			headSize:    u32(e[16:20]),
			tailSize:    u32(e[20:24]),
			Cardinality: u32(e[24:28]),
			Name:        name,
		}
	}
	return &FacetIndex{r: r, CaseSensitive: caseSensitive, fields: fields, cats: cats}, nil
}

// Fields returns every field's directory entry.
func (f *FacetIndex) Fields() []FacetFieldMeta { return f.fields }

// Categories returns every category's directory entry, in stored order (grouped by
// field, sorted by key within a field).
func (f *FacetIndex) Categories() []FacetCatMeta { return f.cats }

// Category returns the directory entry for a category key. Category keys are not
// globally sorted (only within a field), so this scans linearly.
func (f *FacetIndex) Category(key uint64) (FacetCatMeta, bool) {
	for _, c := range f.cats {
		if c.Key == key {
			return c, true
		}
	}
	return FacetCatMeta{}, false
}

// deserializeBitmap parses a portable RoaringBitmap, copying the input so the
// returned bitmap does not alias the read buffer.
func deserializeBitmap(b []byte) (*roaring.Bitmap, error) {
	bm := roaring.New()
	if len(b) == 0 {
		return bm, nil
	}
	if err := bm.UnmarshalBinary(b); err != nil {
		return nil, err
	}
	return bm, nil
}

// posting range-reads a category's head+tail bytes and merges them into the full
// doc posting.
func (f *FacetIndex) posting(c FacetCatMeta) (*roaring.Bitmap, error) {
	buf, err := boundedRead(f.r, int64(c.headOff), uint64(c.headSize)+uint64(c.tailSize))
	if err != nil {
		return nil, err
	}
	head, err := deserializeBitmap(buf[:c.headSize])
	if err != nil {
		return nil, err
	}
	tail, err := deserializeBitmap(buf[c.headSize:])
	if err != nil {
		return nil, err
	}
	head.Or(tail)
	return head, nil
}

// Posting returns the merged doc posting for a category key, or ok=false if the key
// is absent.
func (f *FacetIndex) Posting(key uint64) (*roaring.Bitmap, bool, error) {
	c, ok := f.Category(key)
	if !ok {
		return nil, false, nil
	}
	bm, err := f.posting(c)
	if err != nil {
		return nil, false, err
	}
	return bm, true, nil
}

// ReadAll reconstructs the []FacetField that WriteFacets consumes (postings
// included), so WriteFacetsWith(ReadAll(x), !x.CaseSensitive) reproduces x.
func (f *FacetIndex) ReadAll() ([]FacetField, error) {
	out := make([]FacetField, len(f.fields))
	for i, fm := range f.fields {
		field := FacetField{Name: fm.Name}
		for j := uint32(0); j < fm.CatCount; j++ {
			c := f.cats[fm.CatStart+j]
			bm, err := f.posting(c)
			if err != nil {
				return nil, err
			}
			field.Categories = append(field.Categories, FacetCategory{Name: c.Name, Bitmap: bm})
		}
		out[i] = field
	}
	return out, nil
}

// describeFacets reads only the RRSF header for `info`.
func describeFacets(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, MagicFacet, facetHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != VersionFacet {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: MagicFacet, Name: "facets", Ext: ".rrf", Version: u16(h[4:6]),
		Fields: []Field{
			{"fields", u32(h[8:12])},
			{"categories", u32(h[12:16])},
			{"caseSensitive", u16(h[6:8])&facetFlagCaseSensitive != 0},
		},
	}, nil
}
