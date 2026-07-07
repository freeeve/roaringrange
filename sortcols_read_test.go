package roaringrange

import (
	"bytes"
	"reflect"
	"testing"
)

// sortcolsFixture mirrors the RRSC golden inputs (sortcols_test.go).
func sortcolsFixture() []SortColumn {
	return []SortColumn{
		{"year", U16Column{2020, 2019, 2021, 2018}},
		{"citations", U32Column{100, 5, 9999, 0}},
		{"delta", I32Column{-5, 10, -100, 42}},
		{"score", F32Column{1.5, -2.25, 0.0, 3.5}},
	}
}

// TestOpenSortcolsRoundTrip writes the fixture columns, reopens, and checks the
// decoded columns are identical and re-serialize byte-for-byte.
func TestOpenSortcolsRoundTrip(t *testing.T) {
	cols := sortcolsFixture()
	var buf bytes.Buffer
	if err := WriteSortcols(&buf, cols); err != nil {
		t.Fatalf("WriteSortcols: %v", err)
	}
	s, err := OpenSortcols(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenSortcols: %v", err)
	}
	if s.Rows() != 4 {
		t.Fatalf("Rows = %d, want 4", s.Rows())
	}
	got, err := s.ReadAll()
	if err != nil {
		t.Fatalf("ReadAll: %v", err)
	}
	if !reflect.DeepEqual(got, cols) {
		t.Errorf("ReadAll = %+v, want %+v", got, cols)
	}
	var re bytes.Buffer
	if err := WriteSortcols(&re, got); err != nil {
		t.Fatalf("re-WriteSortcols: %v", err)
	}
	if !bytes.Equal(re.Bytes(), buf.Bytes()) {
		t.Errorf("round-trip drifted")
	}
}

// TestOpenSortcolsGolden decodes the Rust-authored RRSC golden and checks the
// column shapes and values match the known fixture.
func TestOpenSortcolsGolden(t *testing.T) {
	s, err := OpenSortcols(bytes.NewReader(loadGoldenBytes(t, "rrsc")))
	if err != nil {
		t.Fatalf("OpenSortcols(golden): %v", err)
	}
	meta := s.Columns()
	wantNames := []string{"year", "citations", "delta", "score"}
	wantTypes := []string{"u16", "u32", "i32", "f32"}
	for i, m := range meta {
		if m.Name != wantNames[i] || m.Type != wantTypes[i] || m.Rows != 4 {
			t.Errorf("col %d = %+v", i, m)
		}
	}
	got, err := s.ReadAll()
	if err != nil {
		t.Fatalf("ReadAll: %v", err)
	}
	if !reflect.DeepEqual(got, sortcolsFixture()) {
		t.Errorf("golden columns = %+v", got)
	}
}
