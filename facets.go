package roaringrange

import (
	"encoding/binary"
	"hash/fnv"
	"io"
	"sort"
	"strings"

	"github.com/RoaringBitmap/roaring/v2"
)

const (
	// MagicFacet is the RRSF facet sidecar magic.
	MagicFacet = "RRSF"
	// VersionFacet is the RRSF format version number.
	VersionFacet = 1
	// facetHeaderSize is the fixed RRSF header size in bytes.
	facetHeaderSize = 24
	// facetFieldEntry is the size of one field-table entry in bytes:
	// nameOff(4) + nameLen(2) + pad(2) + catStart(4) + catCount(4).
	facetFieldEntry = 16
	// facetCatEntry is the size of one category-table entry in bytes:
	// key(8) + headOff(8) + headSize(4) + tailSize(4) + cardinality(4) +
	// nameOff(4) + nameLen(2) + pad(2).
	facetCatEntry = 36
	// facetSep separates field and category when hashing a category key.
	facetSep = 0x1f
	// facetFlagCaseSensitive is the RRSF reserved-field (offset 6) bit marking a sidecar
	// whose facet keys are case-sensitive (field/category not lowercased). Unset (the
	// default) keeps every sidecar byte-identical. Mirrors the Rust RRSF_FLAG_CASE_SENSITIVE.
	facetFlagCaseSensitive = 1
)

// FacetCategory is one category value within a field and the doc-ID posting of
// the documents carrying it.
type FacetCategory struct {
	Name   string
	Bitmap *roaring.Bitmap
}

// FacetField is a named facet field with its categories. Mirrors one field of a
// roaringsearch BitmapFilter.
type FacetField struct {
	Name       string
	Categories []FacetCategory
}

// FacetKey derives the category key as FNV-1a 64-bit over lower(field), a 0x1f
// separator, and lower(category). See FACETS.md. Equivalent to FacetKeyWith with
// case folding on (the default).
func FacetKey(field, category string) uint64 {
	return FacetKeyWith(field, category, true)
}

// FacetKeyWith is FacetKey with an explicit case-fold flag. When caseFold is false the
// field/category are hashed verbatim (a case-sensitive index keeps "Smith" and "smith"
// distinct). Build and pruning sides must pass the same mode. Mirrors the Rust facet_key.
func FacetKeyWith(field, category string, caseFold bool) uint64 {
	h := fnv.New64a()
	if caseFold {
		io.WriteString(h, strings.ToLower(field))
	} else {
		io.WriteString(h, field)
	}
	h.Write([]byte{facetSep})
	if caseFold {
		io.WriteString(h, strings.ToLower(category))
	} else {
		io.WriteString(h, category)
	}
	return h.Sum64()
}

// catOut is a category prepared for writing: split posting, key, count, and the
// string-blob slice of its name.
type catOut struct {
	key     uint64
	card    uint32
	nameOff uint32
	nameLen uint16
	head    []byte
	tail    []byte
}

// fieldOut is a field prepared for writing.
type fieldOut struct {
	nameOff  uint32
	nameLen  uint16
	catStart uint32
	cats     []catOut
}

// WriteFacets writes the RRSF facet sidecar for the given fields to dst. Each
// category posting is split into head (docs [0,65536)) and tail (docs
// [65536, ∞)) portable RoaringBitmaps, mirroring the text index. Categories are
// grouped by field and sorted by key within a field. See FACETS.md.
func WriteFacets(dst io.Writer, fields []FacetField) error {
	return WriteFacetsWith(dst, fields, true)
}

// WriteFacetsWith is WriteFacets with an explicit caseNormalization flag. true (the default)
// lowercases field/category for the facet-key hash and writes a byte-identical v1 sidecar;
// false keys on the raw bytes (a case-sensitive index) and sets the reserved-field flag so a
// split-set's facet pruning recomputes keys identically. Category display names are stored
// verbatim either way. Mirrors the Rust write_facets_with.
func WriteFacetsWith(dst io.Writer, fields []FacetField, caseNormalization bool) error {
	var blob []byte
	addStr := func(s string) (uint32, uint16) {
		off := uint32(len(blob))
		blob = append(blob, s...)
		return off, uint16(len(s))
	}

	fos := make([]fieldOut, 0, len(fields))
	totalCats := 0
	for _, f := range fields {
		nameOff, nameLen := addStr(f.Name)
		fo := fieldOut{nameOff: nameOff, nameLen: nameLen, catStart: uint32(totalCats)}
		for _, c := range f.Categories {
			head, tail, err := splitBitmap(c.Bitmap)
			if err != nil {
				return err
			}
			cnOff, cnLen := addStr(c.Name)
			fo.cats = append(fo.cats, catOut{
				key:     FacetKeyWith(f.Name, c.Name, caseNormalization),
				card:    uint32(c.Bitmap.GetCardinality()),
				nameOff: cnOff,
				nameLen: cnLen,
				head:    head,
				tail:    tail,
			})
		}
		sort.Slice(fo.cats, func(i, j int) bool { return fo.cats[i].key < fo.cats[j].key })
		totalCats += len(fo.cats)
		fos = append(fos, fo)
	}

	strBlobOff := facetHeaderSize + len(fos)*facetFieldEntry + totalCats*facetCatEntry
	postingsStart := strBlobOff + len(blob)

	header := make([]byte, facetHeaderSize)
	copy(header[0:4], MagicFacet)
	binary.LittleEndian.PutUint16(header[4:6], VersionFacet)
	if !caseNormalization {
		binary.LittleEndian.PutUint16(header[6:8], facetFlagCaseSensitive) // reserved @6
	}
	binary.LittleEndian.PutUint32(header[8:12], uint32(len(fos)))
	binary.LittleEndian.PutUint32(header[12:16], uint32(totalCats))
	binary.LittleEndian.PutUint32(header[16:20], uint32(len(blob)))
	if _, err := dst.Write(header); err != nil {
		return err
	}

	fieldTable := make([]byte, len(fos)*facetFieldEntry)
	for i, fo := range fos {
		b := fieldTable[i*facetFieldEntry:]
		binary.LittleEndian.PutUint32(b[0:4], fo.nameOff)
		binary.LittleEndian.PutUint16(b[4:6], fo.nameLen)
		binary.LittleEndian.PutUint32(b[8:12], fo.catStart)
		binary.LittleEndian.PutUint32(b[12:16], uint32(len(fo.cats)))
	}
	if _, err := dst.Write(fieldTable); err != nil {
		return err
	}

	catTable := make([]byte, totalCats*facetCatEntry)
	off := postingsStart
	ci := 0
	for _, fo := range fos {
		for _, c := range fo.cats {
			b := catTable[ci*facetCatEntry:]
			binary.LittleEndian.PutUint64(b[0:8], c.key)
			binary.LittleEndian.PutUint64(b[8:16], uint64(off))
			binary.LittleEndian.PutUint32(b[16:20], uint32(len(c.head)))
			binary.LittleEndian.PutUint32(b[20:24], uint32(len(c.tail)))
			binary.LittleEndian.PutUint32(b[24:28], c.card)
			binary.LittleEndian.PutUint32(b[28:32], c.nameOff)
			binary.LittleEndian.PutUint16(b[32:34], c.nameLen)
			off += len(c.head) + len(c.tail)
			ci++
		}
	}
	if _, err := dst.Write(catTable); err != nil {
		return err
	}

	if _, err := dst.Write(blob); err != nil {
		return err
	}

	for _, fo := range fos {
		for _, c := range fo.cats {
			if _, err := dst.Write(c.head); err != nil {
				return err
			}
			if _, err := dst.Write(c.tail); err != nil {
				return err
			}
		}
	}
	return nil
}
