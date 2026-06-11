package roaringrange

import (
	"bufio"
	"encoding/hex"
	"os"
	"strings"
	"testing"
)

// readGolden parses a shared cross-language golden (`name <hex>` per line).
func readGolden(t *testing.T, path string) map[string][]byte {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatalf("open golden: %v", err)
	}
	defer f.Close()
	out := make(map[string][]byte)
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<24)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" {
			continue
		}
		name, hx, ok := strings.Cut(line, " ")
		if !ok {
			t.Fatalf("malformed golden line: %q", line)
		}
		b, err := hex.DecodeString(hx)
		if err != nil {
			t.Fatalf("golden hex for %s: %v", name, err)
		}
		out[name] = b
	}
	if err := sc.Err(); err != nil {
		t.Fatalf("scan golden: %v", err)
	}
	return out
}

// termConformanceBuild mirrors the Rust splitset_build::term_conformance_build
// fixture EXACTLY — same corpus (with its deliberate Unicode stress: U+0130,
// a circled digit, Greek capitals, combining accents), same config.
func termConformanceBuild(t *testing.T) *BuiltSplitSet {
	t.Helper()
	docs := []struct {
		text   string
		facets map[string][]string
	}{
		{"The running runner runs quickly", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"İstanbul naïve cafés résumé", map[string][]string{"year": {"2021"}, "kind": {"b"}}},
		{"status ① bitmap roaring bitmaps", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"ΣΟΦΌΣ σοφός wisdom connection", map[string][]string{"year": {"2022"}, "kind": {"b"}}},
		{"connected connecting connections", map[string][]string{"year": {"2021"}, "kind": {"a"}}},
		{"the a an and are", map[string][]string{"year": {"2022"}, "kind": {"b"}}}, // all stop words
	}
	b := NewTermSplitSetBuilder(TermSplitBuildConfig{
		Policy:     PolicyTiered,
		ByteCap:    400,
		NamePrefix: "tcorpus",
		Language:   TermLanguageEnglish,
		Stopwords:  true,
	})
	for _, d := range docs {
		if _, err := b.AddFaceted(d.text, d.facets); err != nil {
			t.Fatalf("add doc: %v", err)
		}
	}
	built, err := b.Finish()
	if err != nil {
		t.Fatalf("finish: %v", err)
	}
	return built
}

// TestTermSplitBuildMatchesSharedGolden asserts the Go term-bodied split builder
// reproduces the Rust output byte-for-byte — manifest, every RRTI split, every
// RRSF facet sidecar. The same file is asserted by the Rust side
// (splitset_build::term_conformance), so it is the single source of truth.
func TestTermSplitBuildMatchesSharedGolden(t *testing.T) {
	g := readGolden(t, "testdata/rrti_term_split_golden.txt")
	built := termConformanceBuild(t)

	if got, want := built.Manifest, g["manifest"]; !equalBytes(got, want) {
		t.Errorf("manifest drifted from the shared golden:\n got %x\nwant %x", got, want)
	}
	files := append(append([]NamedSplit{}, built.Splits...), built.Facets...)
	for _, f := range files {
		want, ok := g[f.Name]
		if !ok {
			t.Errorf("no golden for %s", f.Name)
			continue
		}
		if !equalBytes(f.Bytes, want) {
			t.Errorf("%s drifted from the shared golden:\n got %x\nwant %x", f.Name, f.Bytes, want)
		}
	}
	if want := 1 + len(built.Splits) + len(built.Facets); len(g) != want {
		t.Errorf("golden entry count: got %d want %d", len(g), want)
	}
}

func equalBytes(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
