package roaringrange

import (
	"bytes"
	"reflect"
	"slices"
	"testing"
)

// TestOpenLookupRoundTrip writes the RRIL golden entries, reopens the bytes, and
// checks the row count and that every distinct identifier resolves to its doc IDs.
func TestOpenLookupRoundTrip(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteLookup(&buf, rrilGoldenEntries); err != nil {
		t.Fatalf("WriteLookup: %v", err)
	}
	idx, err := OpenLookup(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenLookup: %v", err)
	}
	// Two of the eight entries have empty/non-alnum identifiers and are dropped.
	if idx.Count() != 6 {
		t.Fatalf("Count = %d, want 6", idx.Count())
	}

	want := map[string][]uint32{
		"978-0-13-468599-1": {5, 9},
		"B07XYZ1234":        {2, 8}, // "b07xyz1234" normalizes to the same key
		"isbn:0262033844":   {7},
		"AbC123":            {1},
		"missing":           nil,
	}
	for id, exp := range want {
		got, err := idx.Lookup(id)
		if err != nil {
			t.Fatalf("Lookup(%q): %v", id, err)
		}
		slices.Sort(got)
		if !reflect.DeepEqual(got, exp) {
			t.Errorf("Lookup(%q) = %v, want %v", id, got, exp)
		}
	}
}

// TestOpenLookupGolden parses the Rust-authored RRIL golden bytes with the Go
// reader, pinning it against the reference implementation's output.
func TestOpenLookupGolden(t *testing.T) {
	idx, err := OpenLookup(bytes.NewReader(loadGoldenBytes(t, "rril")))
	if err != nil {
		t.Fatalf("OpenLookup(golden): %v", err)
	}
	if idx.Count() != 6 {
		t.Fatalf("golden Count = %d, want 6", idx.Count())
	}
	rows, err := idx.Entries()
	if err != nil {
		t.Fatalf("Entries: %v", err)
	}
	// Rows are stored sorted by (hash, doc).
	for i := 1; i < len(rows); i++ {
		if rows[i-1].Hash > rows[i].Hash {
			t.Fatalf("rows not sorted by hash at %d", i)
		}
	}
	if got, _ := idx.Lookup("978-0-13-468599-1"); !reflect.DeepEqual(got, []uint32{5, 9}) {
		t.Errorf("golden Lookup = %v, want [5 9]", got)
	}
}

// TestDetectFormatLookup exercises the magic->format registry dispatch.
func TestDetectFormatLookup(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteLookup(&buf, rrilGoldenEntries); err != nil {
		t.Fatalf("WriteLookup: %v", err)
	}
	info, err := OpenHeader(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenHeader: %v", err)
	}
	if info.Magic != "RRIL" || info.Name != "lookup" {
		t.Errorf("info = %+v, want RRIL/lookup", info)
	}
}
