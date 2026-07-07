package roaringrange

import (
	"bytes"
	"reflect"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// TestOpenTermIndexRoundTrip builds a multi-block RRTI (a tiny block cap forces many
// blocks, exercising the router FST and front-code decode across boundaries), then
// checks the dictionary and every term's posting read back exactly.
func TestOpenTermIndexRoundTrip(t *testing.T) {
	postings := map[string]*roaring.Bitmap{
		"alpha":   bmOf(1, 2, 3),
		"beta":    bmOf(2, 70000), // spans head/tail
		"bitmap":  bmOf(5),
		"bitmaps": bmOf(5, 6, 70001),     // shares "bitmap" prefix (front-coding)
		"gamma":   bmOf(1, 65535, 65536), // straddles the boundary
		"roaring": bmOf(9, 9, 10),
		"zeta":    bmOf(4),
	}
	var buf bytes.Buffer
	dict, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, true, 16)
	if err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}

	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	if int(ti.Header().TermCount) != len(postings) {
		t.Fatalf("TermCount = %d, want %d", ti.Header().TermCount, len(postings))
	}
	if got := ti.Dict(); !reflect.DeepEqual(got, dict) {
		t.Errorf("Dict = %+v\nwant %+v", got, dict)
	}
	for term, want := range postings {
		bm, ok, err := ti.LookupTerm(term)
		if err != nil || !ok {
			t.Fatalf("LookupTerm(%q): ok=%v err=%v", term, ok, err)
		}
		if !bm.Equals(want) {
			t.Errorf("LookupTerm(%q) = %v, want %v", term, bm.ToArray(), want.ToArray())
		}
	}
	if _, ok, _ := ti.LookupTerm("missing"); ok {
		t.Errorf("LookupTerm of an absent term should miss")
	}
}

// TestOpenTermIndexTokenizer checks the query tokenizer is rebuilt from the header:
// a stemmed English index resolves "running" to the stored stem "run".
func TestOpenTermIndexTokenizer(t *testing.T) {
	postings := map[string]*roaring.Bitmap{"run": bmOf(1, 2), "book": bmOf(3)}
	var buf bytes.Buffer
	if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageEnglish, true, false, true, 0); err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	h := ti.Header()
	if !h.Stemmed || h.Language != TermLanguageEnglish {
		t.Fatalf("header = %+v, want stemmed English", h)
	}
	bm, ok, err := ti.Posting("Running")
	if err != nil || !ok {
		t.Fatalf("Posting(Running): ok=%v err=%v", ok, err)
	}
	if !bm.Equals(bmOf(1, 2)) {
		t.Errorf("Posting(Running) = %v, want [1 2]", bm.ToArray())
	}
}
