package roaringrange

import (
	"bytes"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// TestOpenFacetsRoundTrip writes a facet sidecar, reopens it, checks each category
// posting merges correctly, and re-serializes byte-for-byte.
func TestOpenFacetsRoundTrip(t *testing.T) {
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
	var b bytes.Buffer
	if err := WriteFacets(&b, fields); err != nil {
		t.Fatalf("WriteFacets: %v", err)
	}
	orig := b.Bytes()

	f, err := OpenFacets(bytes.NewReader(orig))
	if err != nil {
		t.Fatalf("OpenFacets: %v", err)
	}
	if len(f.Fields()) != 2 {
		t.Fatalf("fields = %d, want 2", len(f.Fields()))
	}
	want := map[string]map[string]*roaring.Bitmap{
		"format":   {"ebook": bmOf(1, 3, 70000), "audiobook": bmOf(2, 70001)},
		"language": {"english": bmOf(1, 2, 3), "spanish": bmOf(70000, 70001)},
	}
	for field, cats := range want {
		for cat, exp := range cats {
			bm, ok, err := f.Posting(FacetKey(field, cat))
			if err != nil || !ok {
				t.Fatalf("Posting(%s/%s): ok=%v err=%v", field, cat, ok, err)
			}
			if !bm.Equals(exp) {
				t.Errorf("Posting(%s/%s) = %v, want %v", field, cat, bm.ToArray(), exp.ToArray())
			}
		}
	}

	// ReadAll reconstructs the writer structs; re-writing yields identical bytes
	// (WriteFacets re-sorts categories by key, matching the stored order).
	all, err := f.ReadAll()
	if err != nil {
		t.Fatalf("ReadAll: %v", err)
	}
	var re bytes.Buffer
	if err := WriteFacetsWith(&re, all, !f.CaseSensitive); err != nil {
		t.Fatalf("re-WriteFacets: %v", err)
	}
	if !bytes.Equal(re.Bytes(), orig) {
		t.Errorf("round-trip drifted")
	}
}
