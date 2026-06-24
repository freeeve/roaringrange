package roaringrange

import (
	"bytes"
	"testing"
)

// TestSortcolsBuilderMatchesRustGolden asserts the Go RRSC builder is byte-for-byte
// with the Rust write_sortcols via the shared golden.
func TestSortcolsBuilderMatchesRustGolden(t *testing.T) {
	cols := []SortColumn{
		{"year", U16Column{2020, 2019, 2021, 2018}},
		{"citations", U32Column{100, 5, 9999, 0}},
		{"delta", I32Column{-5, 10, -100, 42}},
		{"score", F32Column{1.5, -2.25, 0.0, 3.5}},
	}
	var buf bytes.Buffer
	if err := WriteSortcols(&buf, cols); err != nil {
		t.Fatalf("WriteSortcols: %v", err)
	}
	if want := loadGoldenBytes(t, "rrsc"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRSC drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}

// TestWritePermIsPrimaryU32Column checks WritePerm == a one-column u32 "primary" store.
func TestWritePermIsPrimaryU32Column(t *testing.T) {
	perm := []uint32{3, 1, 0, 2}
	var a, b bytes.Buffer
	if err := WritePerm(&a, perm); err != nil {
		t.Fatal(err)
	}
	if err := WriteSortcols(&b, []SortColumn{{"primary", U32Column(perm)}}); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(a.Bytes(), b.Bytes()) {
		t.Errorf("WritePerm differs from WriteSortcols(primary u32)")
	}
}
