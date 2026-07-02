package roaringrange

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"os"
	"sort"
	"strings"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
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

// TestBM25SidecarAddressesPairedRRT proves a pure-Go build can emit a BM25 .rrb that addresses
// its paired .rrt (not fabricated offsets). WriteTermIndexFullDict returns the real posting head
// offsets it front-coded into the .rrt; feeding them to WriteImpacts must yield a sidecar whose
// entries key on exactly those offsets. Correctness is checked independently of the writer's own
// offset math by parsing the .rrt postings region and asserting each returned HeadOff lands on
// that term's actual posting record.
func TestBM25SidecarAddressesPairedRRT(t *testing.T) {
	const headBoundary = uint32(65536)

	// Build the term->doc postings and the per-doc tf accumulator from the SAME plain
	// tokenization, keyed by the accumulator's own sequential doc IDs.
	acc := NewImpactsAccumulator(NewTermTokenizer(TermLanguageNone, false))
	tok := NewTermTokenizer(TermLanguageNone, false)
	postings := map[string]*roaring.Bitmap{}
	for _, d := range rrsbGoldenCorpus {
		doc := acc.AddDoc(d)
		for _, w := range tok.Tokenize(d) {
			bm := postings[w]
			if bm == nil {
				bm = roaring.New()
				postings[w] = bm
			}
			bm.Add(doc)
		}
	}

	// Build the real .rrt and capture its dictionary (terms in order, real head offsets).
	var rrt bytes.Buffer
	dict, err := WriteTermIndexFullDict(&rrt, postings, headBoundary, TermLanguageNone, false, false, true, 0)
	if err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	if len(dict) != len(postings) {
		t.Fatalf("dict has %d entries, want %d (one per term)", len(dict), len(postings))
	}

	// The sidecar must build over the real (ascending, complete) offsets without error.
	var rrb bytes.Buffer
	if err := WriteImpacts(&rrb, dict, acc, DefaultK1, DefaultB); err != nil {
		t.Fatalf("WriteImpacts over the real dict: %v", err)
	}

	// (A) Each returned HeadOff addresses that term's posting record in the .rrt region, which
	// starts after the 40-byte header + router FST + dict blocks.
	rrtBytes := rrt.Bytes()
	routerLen := binary.LittleEndian.Uint64(rrtBytes[16:24])
	dictLen := binary.LittleEndian.Uint64(rrtBytes[24:32])
	regionStart := 40 + routerLen + dictLen
	var prev uint64
	for i, de := range dict {
		if i > 0 && de.HeadOff <= prev {
			t.Fatalf("head offsets not strictly ascending at %q", de.Term)
		}
		prev = de.HeadOff
		wantHead, wantTail, err := splitBitmapHB(postings[de.Term], headBoundary)
		if err != nil {
			t.Fatalf("splitBitmapHB %q: %v", de.Term, err)
		}
		rec := rrtBytes[regionStart+de.HeadOff:]
		if gotTailLen := binary.LittleEndian.Uint32(rec[0:4]); int(gotTailLen) != len(wantTail) {
			t.Errorf("%q: tail len at HeadOff = %d, want %d", de.Term, gotTailLen, len(wantTail))
		}
		if gotHead := rec[4 : 4+len(wantHead)]; !bytes.Equal(gotHead, wantHead) {
			t.Errorf("%q: head posting at HeadOff %d does not match the term's posting", de.Term, de.HeadOff)
		}
	}

	// (B) The .rrb entries table keys on exactly those head offsets, in dict order, with the
	// posting's cardinality--closing the loop from the .rrt dictionary to the sidecar keys.
	rrbBytes := rrb.Bytes()
	termCount := binary.LittleEndian.Uint32(rrbBytes[24:28])
	entriesOff := binary.LittleEndian.Uint64(rrbBytes[32:40])
	if int(termCount) != len(dict) {
		t.Fatalf(".rrb term_count = %d, want %d", termCount, len(dict))
	}
	for i, de := range dict {
		base := entriesOff + uint64(i)*bm25EntrySize
		if gotHeadOff := binary.LittleEndian.Uint64(rrbBytes[base : base+8]); gotHeadOff != de.HeadOff {
			t.Errorf("%q: .rrb entry %d head_off = %d, want %d (the .rrt dict offset)", de.Term, i, gotHeadOff, de.HeadOff)
		}
		if gotCard := binary.LittleEndian.Uint32(rrbBytes[base+16 : base+20]); uint64(gotCard) != postings[de.Term].GetCardinality() {
			t.Errorf("%q: .rrb entry card = %d, want %d", de.Term, gotCard, postings[de.Term].GetCardinality())
		}
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
