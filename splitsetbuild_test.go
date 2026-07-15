package roaringrange

import (
	"bufio"
	"encoding/hex"
	"os"
	"strings"
	"testing"
)

// conformanceBuild builds the fixed fixture the Rust side builds in
// splitset_build::conformance_build — the same faceted docs, in rank order, and config.
// Both builders must emit byte-identical manifests, split RRS files, and RRSF facet sidecars.
func conformanceBuild(t *testing.T) *BuiltSplitSet {
	t.Helper()
	docs := []struct {
		text   string
		facets map[string][]string
	}{
		{"alpha beta", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"beta gamma", map[string][]string{"year": {"2021"}, "kind": {"b"}}},
		{"gamma delta", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"delta alpha", map[string][]string{"year": {"2021"}, "kind": {"b"}}},
		{"alpha gamma", map[string][]string{"year": {"2022"}, "kind": {"a"}}},
	}
	b := NewSplitSetBuilder(SplitBuildConfig{
		Policy:          PolicyTiered,
		ByteCap:         600,
		GramSize:        3,
		NamePrefix:      "corpus",
		BloomBitsPerKey: 8,
	})
	for _, d := range docs {
		if _, err := b.AddFaceted(d.text, d.facets); err != nil {
			t.Fatalf("AddFaceted(%q): %v", d.text, err)
		}
	}
	built, err := b.Finish()
	if err != nil {
		t.Fatalf("Finish: %v", err)
	}
	return built
}

// loadGolden parses the shared `name <hex>` golden (also asserted by the Rust
// builder in splitset_build::conformance_full_build) into name -> bytes.
func loadGolden(t *testing.T) map[string][]byte {
	t.Helper()
	return loadGoldenFile(t, "testdata/rrss_build_golden.txt")
}

// loadGoldenFile parses a shared `name <hex>` golden file into name -> bytes.
func loadGoldenFile(t *testing.T, path string) map[string][]byte {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatalf("open golden: %v", err)
	}
	defer f.Close()
	out := make(map[string][]byte)
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" {
			continue
		}
		name, hx, ok := strings.Cut(line, " ")
		if !ok {
			t.Fatalf("bad golden line: %q", line)
		}
		b, err := hex.DecodeString(hx)
		if err != nil {
			t.Fatalf("bad golden hex for %q: %v", name, err)
		}
		out[name] = b
	}
	if err := sc.Err(); err != nil {
		t.Fatalf("scan golden: %v", err)
	}
	return out
}

// TestSplitSetBuilderMatchesRustGolden proves the Go builder reproduces the Rust
// builder's manifest AND every split RRS byte-for-byte (split assignment, the
// head/tail roaring serialization, and the term Bloom filters all agree).
func TestSplitSetBuilderMatchesRustGolden(t *testing.T) {
	golden := loadGolden(t)
	built := conformanceBuild(t)

	if got, want := built.Manifest, golden["manifest"]; !bytesEqual(got, want) {
		t.Fatalf("manifest bytes differ from the Rust golden:\n got %s\nwant %s",
			hex.EncodeToString(got), hex.EncodeToString(want))
	}
	// Every split RRS and every facet RRSF sidecar must match the Rust golden byte-for-byte.
	files := append(append([]NamedSplit{}, built.Splits...), built.Facets...)
	if got, want := len(files), len(golden)-1; got != want {
		t.Fatalf("split+facet count = %d, want %d", got, want)
	}
	for _, s := range files {
		want, ok := golden[s.Name]
		if !ok {
			t.Fatalf("no golden for %q", s.Name)
		}
		if !bytesEqual(s.Bytes, want) {
			t.Fatalf("%q bytes differ from the Rust golden:\n got %s\nwant %s",
				s.Name, hex.EncodeToString(s.Bytes), hex.EncodeToString(want))
		}
	}
}

// TestSplitSetDigestMatchesRustGolden proves SetFacetDigest reproduces the Rust
// with_facet_digest bytes exactly -- the digest TLV's category ordering (count desc,
// name asc), name spans, and posting ranges, over the shared conformance corpus.
func TestSplitSetDigestMatchesRustGolden(t *testing.T) {
	golden := loadGoldenFile(t, "testdata/rrss_digest_build_golden.txt")
	docs := []struct {
		text   string
		facets map[string][]string
	}{
		{"alpha beta", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"beta gamma", map[string][]string{"year": {"2021"}, "kind": {"b"}}},
		{"gamma delta", map[string][]string{"year": {"2020"}, "kind": {"a"}}},
		{"delta alpha", map[string][]string{"year": {"2021"}, "kind": {"b"}}},
		{"alpha gamma", map[string][]string{"year": {"2022"}, "kind": {"a"}}},
	}
	b := NewSplitSetBuilder(SplitBuildConfig{
		Policy:          PolicyTiered,
		ByteCap:         600,
		GramSize:        3,
		NamePrefix:      "corpus",
		BloomBitsPerKey: 8,
	})
	b.SetFacetDigest(2)
	for _, d := range docs {
		if _, err := b.AddFaceted(d.text, d.facets); err != nil {
			t.Fatalf("AddFaceted(%q): %v", d.text, err)
		}
	}
	built, err := b.Finish()
	if err != nil {
		t.Fatalf("Finish: %v", err)
	}
	if got, want := built.Manifest, golden["manifest"]; !bytesEqual(got, want) {
		t.Fatalf("digest manifest differs from the Rust golden:\n got %s\nwant %s",
			hex.EncodeToString(got), hex.EncodeToString(want))
	}
	files := append(append([]NamedSplit{}, built.Splits...), built.Facets...)
	if got, want := len(files), len(golden)-1; got != want {
		t.Fatalf("split+facet count = %d, want %d", got, want)
	}
	for _, s := range files {
		want, ok := golden[s.Name]
		if !ok {
			t.Fatalf("no golden for %q", s.Name)
		}
		if !bytesEqual(s.Bytes, want) {
			t.Fatalf("%q bytes differ from the Rust golden:\n got %s\nwant %s",
				s.Name, hex.EncodeToString(s.Bytes), hex.EncodeToString(want))
		}
	}
}

// TestSplitSetBuilderRejectsSingleDocOverCap mirrors the Rust degenerate-corpus check.
func TestSplitSetBuilderRejectsSingleDocOverCap(t *testing.T) {
	b := NewSplitSetBuilder(SplitBuildConfig{
		Policy:     PolicyTiered,
		ByteCap:    10,
		GramSize:   3,
		NamePrefix: "corpus",
	})
	if _, err := b.AddText("alpha beta gamma"); err != nil {
		t.Fatalf("AddText: %v", err)
	}
	if _, err := b.Finish(); err == nil {
		t.Fatal("expected an error for a single document exceeding the byte cap")
	}
}

func bytesEqual(a, b []byte) bool {
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
