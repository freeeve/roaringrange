package roaringrange

import (
	"bytes"
	"encoding/hex"
	"os"
	"strings"
	"testing"
)

// rrilGoldenEntries is byte-identical to gen_rril_golden.rs's ENTRIES.
var rrilGoldenEntries = []LookupEntry{
	{"978-0-13-468599-1", 5},
	{"B07XYZ1234", 2},
	{"978-0-13-468599-1", 9},
	{"isbn:0262033844", 7},
	{"", 3},
	{"!!!", 4},
	{"AbC123", 1},
	{"b07xyz1234", 8},
}

// TestLookupBuilderMatchesRustGolden asserts the Go RRIL builder is byte-for-byte
// with the Rust write_lookup via the shared golden.
func TestLookupBuilderMatchesRustGolden(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteLookup(&buf, rrilGoldenEntries); err != nil {
		t.Fatalf("WriteLookup: %v", err)
	}
	raw, err := os.ReadFile("testdata/rril_build_golden.txt")
	if err != nil {
		t.Fatalf("read golden: %v", err)
	}
	name, h, ok := strings.Cut(strings.TrimSpace(string(raw)), " ")
	if !ok || name != "rril" {
		t.Fatalf("bad golden line")
	}
	want, err := hex.DecodeString(h)
	if err != nil {
		t.Fatalf("bad golden hex: %v", err)
	}
	if !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRIL drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}

// TestNormalizeID covers the ASCII-only, upper-casing normalization.
func TestNormalizeID(t *testing.T) {
	cases := map[string]string{
		"978-0-13-468599-1": "9780134685991", // dashes dropped
		"isbn:0262033844":   "ISBN0262033844",
		"AbC123":            "ABC123",
		"!!!":               "",
		"café":              "CAF", // non-ASCII 'é' bytes dropped
	}
	for in, want := range cases {
		if got := normalizeID(in); got != want {
			t.Errorf("normalizeID(%q) = %q, want %q", in, got, want)
		}
	}
}
