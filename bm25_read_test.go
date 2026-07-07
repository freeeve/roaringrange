package roaringrange

import (
	"bytes"
	"sort"
	"testing"
)

// buildRRSBFixture reproduces the RRSB golden construction and returns the sidecar
// bytes, the dict, and the accumulator so a reader test can check decoded values.
func buildRRSBFixture(t *testing.T) ([]byte, []DictEntry, *ImpactsAccumulator) {
	t.Helper()
	tok := NewTermTokenizer(TermLanguageNone, false)
	acc := NewImpactsAccumulator(tok)
	for _, d := range rrsbGoldenCorpus {
		acc.AddDoc(d)
	}
	set := map[string]struct{}{}
	for _, d := range rrsbGoldenCorpus {
		for _, w := range NewTermTokenizer(TermLanguageNone, false).Tokenize(d) {
			set[w] = struct{}{}
		}
	}
	terms := make([]string, 0, len(set))
	for w := range set {
		terms = append(terms, w)
	}
	sort.Strings(terms)
	dict := make([]DictEntry, len(terms))
	for i, w := range terms {
		dict[i] = DictEntry{Term: w, HeadOff: uint64(i)*16 + 100}
	}
	var buf bytes.Buffer
	if err := WriteImpacts(&buf, dict, acc, DefaultK1, DefaultB); err != nil {
		t.Fatalf("WriteImpacts: %v", err)
	}
	return buf.Bytes(), dict, acc
}

// TestOpenImpactsRoundTrip decodes a freshly built RRSB sidecar and checks the
// header and that every dict term's impacts resolve by head_off with the right count.
func TestOpenImpactsRoundTrip(t *testing.T) {
	raw, dict, acc := buildRRSBFixture(t)
	b, err := OpenImpacts(bytes.NewReader(raw))
	if err != nil {
		t.Fatalf("OpenImpacts: %v", err)
	}
	h := b.Header()
	if h.DocCount != 4 {
		t.Errorf("DocCount = %d, want 4", h.DocCount)
	}
	if int(h.TermCount) != len(dict) {
		t.Errorf("TermCount = %d, want %d", h.TermCount, len(dict))
	}
	if h.K1 != DefaultK1 || h.B != DefaultB {
		t.Errorf("k1/b = %v/%v, want %v/%v", h.K1, h.B, DefaultK1, DefaultB)
	}
	for _, de := range dict {
		impacts, ok, err := b.Impacts(de.HeadOff)
		if err != nil || !ok {
			t.Fatalf("Impacts(%q): ok=%v err=%v", de.Term, ok, err)
		}
		wantCard := len(acc.terms[de.Term])
		if len(impacts) != wantCard {
			t.Errorf("term %q impacts len = %d, want %d", de.Term, len(impacts), wantCard)
		}
		for _, imp := range impacts {
			if imp == 0 {
				t.Errorf("term %q has a zero impact byte", de.Term)
			}
		}
	}
	if _, ok, _ := b.Impacts(999999); ok {
		t.Errorf("Impacts of an unknown head_off should miss")
	}
}

// TestOpenImpactsGolden decodes the Rust-authored RRSB golden and checks its header.
func TestOpenImpactsGolden(t *testing.T) {
	b, err := OpenImpacts(bytes.NewReader(loadGoldenBytes(t, "rrsb")))
	if err != nil {
		t.Fatalf("OpenImpacts(golden): %v", err)
	}
	if b.Header().DocCount != 4 {
		t.Errorf("golden DocCount = %d, want 4", b.Header().DocCount)
	}
}
