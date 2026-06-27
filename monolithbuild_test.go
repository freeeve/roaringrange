package roaringrange

import (
	"bytes"
	"encoding/binary"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// monolithFixtureDocs is the corpus shared with the Rust gen_rrs_monolith_golden example:
// shared trigrams across docs, and an empty doc at id 2 so doc 3's postings prove the empty
// doc still advanced the doc-ID space.
func monolithFixtureDocs() []string {
	return []string{"roaring bitmaps", "roaring range", "", "bitmap range index"}
}

// TestTrigramMonolithBuilderMatchesRustGolden asserts the Go monolith builder is byte-for-byte
// with the Rust write_index path via the shared golden.
func TestTrigramMonolithBuilderMatchesRustGolden(t *testing.T) {
	b := NewTrigramMonolithBuilder(3, DefaultStride)
	for i, doc := range monolithFixtureDocs() {
		if got := b.AddText(doc); got != uint32(i) {
			t.Fatalf("AddText doc %d returned id %d, want %d", i, got, i)
		}
	}
	if b.DocCount() != 4 {
		t.Fatalf("DocCount = %d, want 4", b.DocCount())
	}
	var buf bytes.Buffer
	if err := b.Write(&buf); err != nil {
		t.Fatalf("Write: %v", err)
	}
	if want := loadGoldenBytes(t, "rrs_monolith"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRS monolith drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}

// monolithCaseSensitiveFixtureDocs is the mixed-case corpus shared with the Rust
// gen_rrs_monolith_cs_golden example: "Roaring" (doc 0) and "roaring" (doc 1) key on
// distinct trigrams when case folding is off, so the case-sensitive index differs from the
// default one.
func monolithCaseSensitiveFixtureDocs() []string {
	return []string{"Roaring Bitmaps", "roaring range", "", "Bitmap Range INDEX"}
}

// TestTrigramMonolithCaseSensitiveMatchesRustGolden asserts the Go case-sensitive monolith
// builder is byte-for-byte with the Rust write_index_with(.., false) path (a v4 RRSI) via the
// shared golden.
func TestTrigramMonolithCaseSensitiveMatchesRustGolden(t *testing.T) {
	b := NewTrigramMonolithBuilderWith(3, DefaultStride, true)
	for i, doc := range monolithCaseSensitiveFixtureDocs() {
		if got := b.AddText(doc); got != uint32(i) {
			t.Fatalf("AddText doc %d returned id %d, want %d", i, got, i)
		}
	}
	var buf bytes.Buffer
	if err := b.Write(&buf); err != nil {
		t.Fatalf("Write: %v", err)
	}
	if want := loadGoldenBytes(t, "rrs_monolith_cs"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("case-sensitive RRS monolith drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
	if v := binary.LittleEndian.Uint16(buf.Bytes()[4:6]); v != VersionV4 {
		t.Errorf("version word = %d, want v4 %d", v, VersionV4)
	}
}

// TestTrigramMonolithCaseSensitiveReadsBack confirms a case-sensitive monolith is well-formed
// and actually case-sensitive: Open reports CaseFold=false, and the case-sensitive "Roaring"
// query (trigrams keyed verbatim) resolves to doc 0 only — not doc 1's lowercase "roaring".
func TestTrigramMonolithCaseSensitiveReadsBack(t *testing.T) {
	b := NewTrigramMonolithBuilderWith(3, DefaultStride, true)
	for _, doc := range monolithCaseSensitiveFixtureDocs() {
		b.AddText(doc)
	}
	var buf bytes.Buffer
	if err := b.Write(&buf); err != nil {
		t.Fatal(err)
	}
	idx, err := Open(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	if idx.CaseFold {
		t.Fatal("Open reported CaseFold=true for a case-sensitive (v4) index")
	}
	// AND the case-sensitive trigrams of "Roaring" — doc 0 carries them all; doc 1's
	// "roaring" lacks the capitalized "Roa", so it must not match.
	got := andCaseSensitivePosting(t, idx, "Roaring")
	if !equalU32(got, []uint32{0}) {
		t.Errorf(`case-sensitive "Roaring" = %v, want [0]`, got)
	}
	if got := andCaseSensitivePosting(t, idx, "roaring"); !equalU32(got, []uint32{1}) {
		t.Errorf(`case-sensitive "roaring" = %v, want [1]`, got)
	}
}

// andCaseSensitivePosting intersects the case-sensitive trigram postings of query against idx.
func andCaseSensitivePosting(t *testing.T, idx *Index, query string) []uint32 {
	t.Helper()
	var acc *roaring.Bitmap
	for _, k := range NgramKeysWith(query, idx.GramSize, idx.CaseFold) {
		data, ok, err := idx.Posting(k)
		if err != nil {
			t.Fatalf("Posting: %v", err)
		}
		if !ok {
			return nil
		}
		bm := roaring.New()
		if _, err := bm.ReadFrom(bytes.NewReader(data)); err != nil {
			t.Fatalf("deserialize: %v", err)
		}
		if acc == nil {
			acc = bm
		} else {
			acc.And(bm)
		}
	}
	if acc == nil {
		return nil
	}
	return acc.ToArray()
}

// TestTrigramMonolithReadsBack builds a monolith and queries it through the RRS reader, to
// confirm the produced index is well-formed (not just golden-equal): the "ran" trigram (only
// in "range", docs 1 and 3) must read back as exactly {1, 3} — which holds only if the empty
// doc at id 2 advanced the doc-ID space.
func TestTrigramMonolithReadsBack(t *testing.T) {
	b := NewTrigramMonolithBuilder(3, DefaultStride)
	for _, doc := range monolithFixtureDocs() {
		b.AddText(doc)
	}
	var buf bytes.Buffer
	if err := b.Write(&buf); err != nil {
		t.Fatal(err)
	}
	idx, err := Open(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	ran := NgramKeys("ran", 3) // a single trigram, only present in "range"
	if len(ran) != 1 {
		t.Fatalf(`NgramKeys("ran", 3) = %v, want one key`, ran)
	}
	data, ok, err := idx.Posting(ran[0])
	if err != nil || !ok {
		t.Fatalf("Posting(ran): ok=%v err=%v", ok, err)
	}
	bm := roaring.New()
	if _, err := bm.ReadFrom(bytes.NewReader(data)); err != nil {
		t.Fatalf("deserialize posting: %v", err)
	}
	if got := bm.ToArray(); !equalU32(got, []uint32{1, 3}) {
		t.Errorf(`"ran" posting = %v, want [1 3] (empty doc id 2 must be skipped)`, got)
	}
}

// TestWriteIndexSortsAndGuardsStride checks the public WriteIndex mirrors the Rust
// write_index: unsorted entries are laid out key-sorted, and stride <= 0 becomes DefaultStride
// (visible in the header's stride word).
func TestWriteIndexSortsAndGuardsStride(t *testing.T) {
	entries := []IndexEntry{
		{Key: 30, Posting: mustPosting(t, 2)},
		{Key: 10, Posting: mustPosting(t, 0)},
		{Key: 20, Posting: mustPosting(t, 1)},
	}
	var buf bytes.Buffer
	if err := WriteIndex(&buf, 3, 0, entries); err != nil {
		t.Fatal(err)
	}
	b := buf.Bytes()
	if stride := binary.LittleEndian.Uint32(b[12:16]); stride != uint32(DefaultStride) {
		t.Errorf("stride word = %d, want DefaultStride %d", stride, DefaultStride)
	}
	// First dict key (after the 16-byte header + a 1-entry sparse index of 8 bytes) must be the
	// smallest key, proving WriteIndex sorted.
	dictStart := 16 + 8
	if k := binary.LittleEndian.Uint64(b[dictStart:]); k != 10 {
		t.Errorf("first dict key = %d, want 10 (entries should be sorted)", k)
	}
}

func mustPosting(t *testing.T, ids ...uint32) []byte {
	t.Helper()
	out, err := roaring.BitmapOf(ids...).ToBytes()
	if err != nil {
		t.Fatal(err)
	}
	return out
}
