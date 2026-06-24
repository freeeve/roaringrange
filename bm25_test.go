package roaringrange

import (
	"bytes"
	"encoding/hex"
	"os"
	"sort"
	"strings"
	"testing"
)

// rrsbGoldenCorpus is byte-identical to gen_rrsb_golden.rs's CORPUS.
var rrsbGoldenCorpus = []string{
	"the quick brown fox jumps over the lazy dog",
	"quick brown bitmaps roaring over data",
	"roaring fox bitmaps fast and quick",
	"the lazy dog and the quick fox",
}

// buildRRSBGolden reproduces the shared construction: plain tokenizer, a synthetic
// dict (distinct terms lexicographically, ascending head_off), default k1/b.
func buildRRSBGolden(t *testing.T) []byte {
	t.Helper()
	tok := NewTermTokenizer(TermLanguageNone, false) // plain: no stem / no stopwords
	acc := NewImpactsAccumulator(tok)
	for _, d := range rrsbGoldenCorpus {
		acc.AddDoc(d)
	}
	tok2 := NewTermTokenizer(TermLanguageNone, false)
	set := map[string]struct{}{}
	for _, d := range rrsbGoldenCorpus {
		for _, w := range tok2.Tokenize(d) {
			set[w] = struct{}{}
		}
	}
	terms := make([]string, 0, len(set))
	for w := range set {
		terms = append(terms, w)
	}
	sort.Strings(terms) // byte-lexicographic, matching the Rust BTreeSet order
	dict := make([]DictEntry, len(terms))
	for i, w := range terms {
		dict[i] = DictEntry{Term: w, HeadOff: uint64(i)*16 + 100}
	}
	var buf bytes.Buffer
	if err := WriteImpacts(&buf, dict, acc, DefaultK1, DefaultB); err != nil {
		t.Fatalf("WriteImpacts: %v", err)
	}
	return buf.Bytes()
}

// TestBM25BuilderMatchesRustGolden asserts the Go RRSB builder is byte-for-byte with
// the Rust write_impacts via the shared golden (also asserted Rust-side by
// bm25::tests::rrsb_golden_matches).
func TestBM25BuilderMatchesRustGolden(t *testing.T) {
	got := buildRRSBGolden(t)

	raw, err := os.ReadFile("testdata/rrsb_build_golden.txt")
	if err != nil {
		t.Fatalf("read golden: %v", err)
	}
	name, h, ok := strings.Cut(strings.TrimSpace(string(raw)), " ")
	if !ok || name != "rrsb" {
		t.Fatalf("bad golden line")
	}
	want, err := hex.DecodeString(h)
	if err != nil {
		t.Fatalf("bad golden hex: %v", err)
	}

	if !bytes.Equal(got, want) {
		t.Errorf("RRSB drifted from the Rust golden:\n got %x\nwant %x", got, want)
	}
}

// TestQuantizeImpactBounds sanity-checks the byte range and monotonicity in tf.
func TestQuantizeImpactBounds(t *testing.T) {
	avgdl, k1, b := float32(7), DefaultK1, DefaultB
	prev := byte(0)
	for tf := uint32(1); tf <= 50; tf++ {
		v := QuantizeImpact(tf, 7, avgdl, k1, b)
		if v == 0 {
			t.Fatalf("tf=%d quantized to 0 (must be 1–255)", tf)
		}
		if v < prev {
			t.Fatalf("tf=%d not monotonic: %d < %d", tf, v, prev)
		}
		prev = v
	}
}
