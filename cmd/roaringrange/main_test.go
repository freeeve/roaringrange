package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
	rr "github.com/freeeve/roaringrange"
)

// buildCLI compiles the CLI to a temp binary once and returns its path.
func buildCLI(t *testing.T) string {
	t.Helper()
	bin := filepath.Join(t.TempDir(), "roaringrange")
	out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput()
	if err != nil {
		t.Fatalf("build CLI: %v\n%s", err, out)
	}
	return bin
}

// writeGolden decodes the single-line `<name> <hex>` golden into a temp file and
// returns its path.
func writeGolden(t *testing.T, name string) string {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join("..", "..", "testdata", name+"_build_golden.txt"))
	if err != nil {
		t.Fatalf("read golden %s: %v", name, err)
	}
	_, h, _ := strings.Cut(strings.TrimSpace(string(raw)), " ")
	b, err := hex.DecodeString(h)
	if err != nil {
		t.Fatalf("decode golden %s: %v", name, err)
	}
	p := filepath.Join(t.TempDir(), name+".bin")
	if err := os.WriteFile(p, b, 0o644); err != nil {
		t.Fatalf("write %s: %v", name, err)
	}
	return p
}

// TestInfoAndDump runs `info` and `dump --json` over the single-line goldens and
// checks the CLI exits 0, reports the right magic, and emits well-formed JSON with a
// "format" key.
func TestInfoAndDump(t *testing.T) {
	bin := buildCLI(t)
	cases := map[string]string{
		"rrsc": "RRSC", "rril": "RRIL", "rrhc": "RRHC",
		"rrvi": "RRVI", "rrvr": "RRVR", "rrsb": "RRSB",
	}
	for name, magic := range cases {
		t.Run(name, func(t *testing.T) {
			file := writeGolden(t, name)

			info, err := exec.Command(bin, "info", file).CombinedOutput()
			if err != nil {
				t.Fatalf("info: %v\n%s", err, info)
			}
			if !strings.Contains(string(info), magic) {
				t.Errorf("info missing magic %s:\n%s", magic, info)
			}

			dump, err := exec.Command(bin, "dump", file, "--postings").CombinedOutput()
			if err != nil {
				t.Fatalf("dump: %v\n%s", err, dump)
			}
			var obj map[string]any
			if err := json.Unmarshal(dump, &obj); err != nil {
				t.Fatalf("dump not valid JSON: %v\n%s", err, dump)
			}
			if obj["format"] != magic {
				t.Errorf("dump format = %v, want %s", obj["format"], magic)
			}
		})
	}
}

// TestGetPrefix builds a small RRTI and checks `get --prefix` lists the matched
// terms and the union posting as JSON.
func TestGetPrefix(t *testing.T) {
	bin := buildCLI(t)
	postings := map[string]*roaring.Bitmap{
		"bit":    roaring.BitmapOf(7),
		"bitmap": roaring.BitmapOf(5, 6),
		"zeta":   roaring.BitmapOf(4),
	}
	var buf bytes.Buffer
	if err := rr.WriteTermIndexFull(&buf, postings, 65536, rr.TermLanguageNone, false, false, true, 0); err != nil {
		t.Fatalf("WriteTermIndexFull: %v", err)
	}
	p := filepath.Join(t.TempDir(), "terms.rrt")
	if err := os.WriteFile(p, buf.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}

	out, err := exec.Command(bin, "get", p, "--prefix", "BIT").CombinedOutput()
	if err != nil {
		t.Fatalf("get --prefix: %v\n%s", err, out)
	}
	var obj map[string]any
	if err := json.Unmarshal(out, &obj); err != nil {
		t.Fatalf("get --prefix not valid JSON: %v\n%s", err, out)
	}
	if obj["found"] != true {
		t.Errorf("found = %v, want true:\n%s", obj["found"], out)
	}
	if terms, _ := obj["terms"].([]any); len(terms) != 2 {
		t.Errorf("terms = %v, want the 2 bit* terms:\n%s", obj["terms"], out)
	}
	if card, _ := obj["cardinality"].(float64); card != 3 {
		t.Errorf("cardinality = %v, want 3 (union of bit* postings):\n%s", obj["cardinality"], out)
	}
}

// TestUnknownFile checks a non-roaringrange file fails cleanly (non-zero exit, no panic).
func TestUnknownFile(t *testing.T) {
	bin := buildCLI(t)
	p := filepath.Join(t.TempDir(), "junk")
	if err := os.WriteFile(p, []byte("not a roaringrange file"), 0o644); err != nil {
		t.Fatal(err)
	}
	out, err := exec.Command(bin, "info", p).CombinedOutput()
	if err == nil {
		t.Errorf("expected non-zero exit for junk input, got:\n%s", out)
	}
}
