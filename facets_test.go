package roaringrange

import (
	"bytes"
	"encoding/binary"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// parsedCat mirrors one category-table entry decoded from an RRSF buffer.
type parsedCat struct {
	key      uint64
	headOff  uint64
	headSize uint32
	tailSize uint32
	card     uint32
	name     string
}

// parseFacets decodes an RRSF buffer into fieldName -> ordered categories for
// assertions in the tests.
func parseFacets(t *testing.T, buf []byte) map[string][]parsedCat {
	t.Helper()
	if string(buf[0:4]) != MagicFacet {
		t.Fatalf("magic = %q, want %q", buf[0:4], MagicFacet)
	}
	if v := binary.LittleEndian.Uint16(buf[4:6]); v != VersionFacet {
		t.Fatalf("version = %d, want %d", v, VersionFacet)
	}
	fields := binary.LittleEndian.Uint32(buf[8:12])
	cats := binary.LittleEndian.Uint32(buf[12:16])
	strBytes := binary.LittleEndian.Uint32(buf[16:20])

	fieldTable := facetHeaderSize
	catTable := fieldTable + int(fields)*facetFieldEntry
	strBlob := catTable + int(cats)*facetCatEntry
	if strBlob+int(strBytes) > len(buf) {
		t.Fatalf("string blob overruns buffer")
	}
	str := func(off uint32, ln uint16) string { return string(buf[strBlob+int(off) : strBlob+int(off)+int(ln)]) }

	out := make(map[string][]parsedCat)
	for i := 0; i < int(fields); i++ {
		b := buf[fieldTable+i*facetFieldEntry:]
		fname := str(binary.LittleEndian.Uint32(b[0:4]), binary.LittleEndian.Uint16(b[4:6]))
		catStart := binary.LittleEndian.Uint32(b[8:12])
		catCount := binary.LittleEndian.Uint32(b[12:16])
		var pcs []parsedCat
		for j := uint32(0); j < catCount; j++ {
			c := buf[catTable+int(catStart+j)*facetCatEntry:]
			pcs = append(pcs, parsedCat{
				key:      binary.LittleEndian.Uint64(c[0:8]),
				headOff:  binary.LittleEndian.Uint64(c[8:16]),
				headSize: binary.LittleEndian.Uint32(c[16:20]),
				tailSize: binary.LittleEndian.Uint32(c[20:24]),
				card:     binary.LittleEndian.Uint32(c[24:28]),
				name:     str(binary.LittleEndian.Uint32(c[28:32]), binary.LittleEndian.Uint16(c[32:34])),
			})
		}
		out[fname] = pcs
	}
	return out
}

// posting deserializes the head and tail at the given offsets and returns their union.
func posting(t *testing.T, buf []byte, pc parsedCat) *roaring.Bitmap {
	t.Helper()
	bm := roaring.New()
	for _, seg := range [][2]uint64{{pc.headOff, uint64(pc.headSize)}, {pc.headOff + uint64(pc.headSize), uint64(pc.tailSize)}} {
		part := roaring.New()
		if _, err := part.FromBuffer(append([]byte(nil), buf[seg[0]:seg[0]+seg[1]]...)); err != nil {
			t.Fatalf("deserialize posting: %v", err)
		}
		bm.Or(part)
	}
	return bm
}

func bmOf(vals ...uint32) *roaring.Bitmap {
	bm := roaring.New()
	bm.AddMany(vals)
	return bm
}

func TestWriteFacetsRoundTrip(t *testing.T) {
	fields := []FacetField{
		{Name: "format", Categories: []FacetCategory{
			{Name: "ebook", Bitmap: bmOf(1, 3, 70000)},
			{Name: "audiobook", Bitmap: bmOf(2, 70001)},
		}},
		{Name: "language", Categories: []FacetCategory{
			{Name: "english", Bitmap: bmOf(1, 2, 3)},
			{Name: "spanish", Bitmap: bmOf(70000, 70001)},
		}},
	}
	want := map[string]map[string]*roaring.Bitmap{
		"format":   {"ebook": bmOf(1, 3, 70000), "audiobook": bmOf(2, 70001)},
		"language": {"english": bmOf(1, 2, 3), "spanish": bmOf(70000, 70001)},
	}

	var b bytes.Buffer
	if err := WriteFacets(&b, fields); err != nil {
		t.Fatalf("WriteFacets: %v", err)
	}
	buf := b.Bytes()
	parsed := parseFacets(t, buf)

	if len(parsed) != 2 {
		t.Fatalf("fields = %d, want 2", len(parsed))
	}
	for fname, cats := range parsed {
		if len(cats) != 2 {
			t.Fatalf("field %q has %d cats, want 2", fname, len(cats))
		}
		// Keys must be ascending within a field.
		if cats[0].key > cats[1].key {
			t.Fatalf("field %q categories not key-sorted", fname)
		}
		for _, pc := range cats {
			w, ok := want[fname][pc.name]
			if !ok {
				t.Fatalf("unexpected category %q in field %q", pc.name, fname)
			}
			if uint64(pc.card) != w.GetCardinality() {
				t.Errorf("%s/%s cardinality = %d, want %d", fname, pc.name, pc.card, w.GetCardinality())
			}
			if pc.key != FacetKey(fname, pc.name) {
				t.Errorf("%s/%s key mismatch", fname, pc.name)
			}
			got := posting(t, buf, pc)
			if !got.Equals(w) {
				t.Errorf("%s/%s posting = %v, want %v", fname, pc.name, got.ToArray(), w.ToArray())
			}
		}
	}
}

// TestWriteFacetsHeadTailSplit verifies the head holds docs <65536 and the tail
// holds docs >=65536, independently deserializable.
func TestWriteFacetsHeadTailSplit(t *testing.T) {
	fields := []FacetField{{Name: "f", Categories: []FacetCategory{
		{Name: "c", Bitmap: bmOf(5, 65535, 65536, 200000)},
	}}}
	var b bytes.Buffer
	if err := WriteFacets(&b, fields); err != nil {
		t.Fatalf("WriteFacets: %v", err)
	}
	buf := b.Bytes()
	pc := parseFacets(t, buf)["f"][0]

	head := roaring.New()
	if _, err := head.FromBuffer(append([]byte(nil), buf[pc.headOff:pc.headOff+uint64(pc.headSize)]...)); err != nil {
		t.Fatalf("head: %v", err)
	}
	tail := roaring.New()
	toff := pc.headOff + uint64(pc.headSize)
	if _, err := tail.FromBuffer(append([]byte(nil), buf[toff:toff+uint64(pc.tailSize)]...)); err != nil {
		t.Fatalf("tail: %v", err)
	}
	if !head.Equals(bmOf(5, 65535)) {
		t.Errorf("head = %v, want [5 65535]", head.ToArray())
	}
	if !tail.Equals(bmOf(65536, 200000)) {
		t.Errorf("tail = %v, want [65536 200000]", tail.ToArray())
	}
}
