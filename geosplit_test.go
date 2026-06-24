package roaringrange

import "testing"

// geoConformanceBuild mirrors the Rust splitset_build::geo_conformance_build
// fixture exactly — doubling caps (300 → 1200) over a repeated corpus, pinning
// the per-tier cap arithmetic (capFor ⇄ Rust cap_for) cross-language: a one-off
// divergence in seal-boundary placement changes every byte after it.
func geoConformanceBuild(t *testing.T) *BuiltSplitSet {
	t.Helper()
	words := []string{
		"alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
	}
	b := NewSplitSetBuilder(SplitBuildConfig{
		Policy:          PolicyTiered,
		ByteCap:         300,
		CapMax:          1200,
		GramSize:        3,
		NamePrefix:      "geo",
		BloomBitsPerKey: 8,
	})
	for i := range 24 {
		text := words[i%len(words)] + " " + words[(i+3)%len(words)]
		year := 2018 + (i % 5)
		if _, err := b.AddFaceted(text, map[string][]string{"year": {itoa(year)}}); err != nil {
			t.Fatalf("add doc: %v", err)
		}
	}
	built, err := b.Finish()
	if err != nil {
		t.Fatalf("finish: %v", err)
	}
	return built
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var buf [20]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	return string(buf[i:])
}

// TestGeometricBuildMatchesSharedGolden asserts the doubling-cap seal boundaries
// match the Rust builder byte-for-byte.
func TestGeometricBuildMatchesSharedGolden(t *testing.T) {
	g := readGolden(t, "testdata/rrss_geo_build_golden.txt")
	built := geoConformanceBuild(t)

	if got, want := built.Manifest, g["manifest"]; !equalBytes(got, want) {
		t.Errorf("geo manifest drifted from the shared golden:\n got %x\nwant %x", got, want)
	}
	files := append(append([]NamedSplit{}, built.Splits...), built.Facets...)
	for _, f := range files {
		want, ok := g[f.Name]
		if !ok {
			t.Errorf("no golden for %s", f.Name)
			continue
		}
		if !equalBytes(f.Bytes, want) {
			t.Errorf("%s drifted from the shared golden", f.Name)
		}
	}
	if want := 1 + len(built.Splits) + len(built.Facets); len(g) != want {
		t.Errorf("geo golden entry count: got %d want %d", len(g), want)
	}
}
