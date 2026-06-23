package roaringrange

import (
	"encoding/hex"
	"os"
	"strings"
	"testing"
)

// loadGoldenBytes reads the shared conformance golden testdata/<name>_build_golden.txt
// (one line, `<name> <hex>`) and returns the bytes.
func loadGoldenBytes(t *testing.T, name string) []byte {
	t.Helper()
	raw, err := os.ReadFile("testdata/" + name + "_build_golden.txt")
	if err != nil {
		t.Fatalf("read golden %s: %v", name, err)
	}
	got, h, ok := strings.Cut(strings.TrimSpace(string(raw)), " ")
	if !ok || got != name {
		t.Fatalf("golden %s: bad `<name> <hex>` header", name)
	}
	b, err := hex.DecodeString(h)
	if err != nil {
		t.Fatalf("golden %s: bad hex: %v", name, err)
	}
	return b
}
