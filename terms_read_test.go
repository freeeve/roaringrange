package roaringrange

import (
	"bytes"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// TestOpenTermIndexRoundTrip builds a multi-block RRTI (a tiny block cap forces many
// blocks, exercising the router FST and front-code decode across boundaries), then
// checks the dictionary and every term's posting read back exactly.
func TestOpenTermIndexRoundTrip(t *testing.T) {
	postings := map[string]*roaring.Bitmap{
		"alpha":   bmOf(1, 2, 3),
		"beta":    bmOf(2, 70000), // spans head/tail
		"bitmap":  bmOf(5),
		"bitmaps": bmOf(5, 6, 70001),     // shares "bitmap" prefix (front-coding)
		"gamma":   bmOf(1, 65535, 65536), // straddles the boundary
		"roaring": bmOf(9, 9, 10),
		"zeta":    bmOf(4),
	}
	var buf bytes.Buffer
	dict, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, true, 16)
	if err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}

	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	if int(ti.Header().TermCount) != len(postings) {
		t.Fatalf("TermCount = %d, want %d", ti.Header().TermCount, len(postings))
	}
	if got := ti.Dict(); !reflect.DeepEqual(got, dict) {
		t.Errorf("Dict = %+v\nwant %+v", got, dict)
	}
	for term, want := range postings {
		bm, ok, err := ti.LookupTerm(term)
		if err != nil || !ok {
			t.Fatalf("LookupTerm(%q): ok=%v err=%v", term, ok, err)
		}
		if !bm.Equals(want) {
			t.Errorf("LookupTerm(%q) = %v, want %v", term, bm.ToArray(), want.ToArray())
		}
	}
	if _, ok, _ := ti.LookupTerm("missing"); ok {
		t.Errorf("LookupTerm of an absent term should miss")
	}
}

// TestOpenTermIndexTokenizer checks the query tokenizer is rebuilt from the header:
// a stemmed English index resolves "running" to the stored stem "run".
func TestOpenTermIndexTokenizer(t *testing.T) {
	postings := map[string]*roaring.Bitmap{"run": bmOf(1, 2), "book": bmOf(3)}
	var buf bytes.Buffer
	if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageEnglish, true, false, true, 0); err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	h := ti.Header()
	if !h.Stemmed || h.Language != TermLanguageEnglish {
		t.Fatalf("header = %+v, want stemmed English", h)
	}
	bm, ok, err := ti.Posting("Running")
	if err != nil || !ok {
		t.Fatalf("Posting(Running): ok=%v err=%v", ok, err)
	}
	if !bm.Equals(bmOf(1, 2)) {
		t.Errorf("Posting(Running) = %v, want [1 2]", bm.ToArray())
	}
}

// TestTermIndexPrefix exercises Complete / PrefixPostings / SearchPrefix over a
// multi-block dictionary (a tiny block cap forces the prefix range across block
// boundaries), comparing against answers computed naively from the postings map.
func TestTermIndexPrefix(t *testing.T) {
	postings := map[string]*roaring.Bitmap{
		"alpha":   bmOf(1, 2, 3),
		"beta":    bmOf(2, 70000),
		"bit":     bmOf(7, 70002),
		"bitmap":  bmOf(5),
		"bitmaps": bmOf(5, 6, 70001),
		"bitset":  bmOf(8),
		"gamma":   bmOf(1, 65535, 65536),
		"roaring": bmOf(9, 10),
		"zeta":    bmOf(4),
	}
	var buf bytes.Buffer
	if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, true, 16); err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}

	// Complete case-folds the query prefix and returns terms in dictionary order.
	got, err := ti.Complete("BIT", 10)
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if want := []string{"bit", "bitmap", "bitmaps", "bitset"}; !reflect.DeepEqual(got, want) {
		t.Errorf("Complete(BIT) = %v, want %v", got, want)
	}
	if got, _ = ti.Complete("bit", 2); !reflect.DeepEqual(got, []string{"bit", "bitmap"}) {
		t.Errorf("Complete(bit, 2) = %v, want [bit bitmap]", got)
	}
	if got, _ = ti.Complete("q", 5); len(got) != 0 {
		t.Errorf("Complete(q) = %v, want none", got)
	}

	// PrefixPostings pairs each matched term with its exact posting.
	tps, truncated, err := ti.PrefixPostings("bit", 10)
	if err != nil || truncated {
		t.Fatalf("PrefixPostings(bit, 10): truncated=%v err=%v", truncated, err)
	}
	if len(tps) != 4 {
		t.Fatalf("PrefixPostings(bit, 10) matched %d terms, want 4", len(tps))
	}
	for _, tp := range tps {
		if !tp.Posting.Equals(postings[tp.Term]) {
			t.Errorf("PrefixPostings posting for %q = %v, want %v",
				tp.Term, tp.Posting.ToArray(), postings[tp.Term].ToArray())
		}
	}
	if _, truncated, _ = ti.PrefixPostings("bit", 3); !truncated {
		t.Errorf("PrefixPostings(bit, 3) should report truncation (4 terms match)")
	}

	// SearchPrefix unions the matched postings, ascending doc IDs, capped at limit.
	docs, truncated, err := ti.SearchPrefix("bit", 100)
	if err != nil || truncated {
		t.Fatalf("SearchPrefix(bit, 100): truncated=%v err=%v", truncated, err)
	}
	if want := []uint32{5, 6, 7, 8, 70001, 70002}; !reflect.DeepEqual(docs, want) {
		t.Errorf("SearchPrefix(bit, 100) = %v, want %v", docs, want)
	}
	// A limit the heads alone satisfy must not change the top of the ranking.
	if docs, _, _ = ti.SearchPrefix("bit", 2); !reflect.DeepEqual(docs, []uint32{5, 6}) {
		t.Errorf("SearchPrefix(bit, 2) = %v, want [5 6]", docs)
	}
	// An empty prefix matches the whole dictionary.
	if docs, _, _ = ti.SearchPrefix("", 100); len(docs) != 15 {
		t.Errorf("SearchPrefix(\"\", 100) returned %d docs, want 15", len(docs))
	}
	if docs, _, _ = ti.SearchPrefix("bit", 0); len(docs) != 0 {
		t.Errorf("SearchPrefix(bit, 0) = %v, want none", docs)
	}
	if docs, _, _ = ti.SearchPrefix("q", 10); len(docs) != 0 {
		t.Errorf("SearchPrefix(q, 10) = %v, want none", docs)
	}
}

// TestTermIndexPrefixCaseSensitive checks a case-sensitive index matches the
// query prefix verbatim instead of folding it.
func TestTermIndexPrefixCaseSensitive(t *testing.T) {
	postings := map[string]*roaring.Bitmap{
		"Bit": bmOf(1), "bit": bmOf(2), "bitmap": bmOf(3),
	}
	var buf bytes.Buffer
	if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, false, 16); err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	if got, _ := ti.Complete("Bit", 10); !reflect.DeepEqual(got, []string{"Bit"}) {
		t.Errorf("Complete(Bit) = %v, want [Bit]", got)
	}
	if got, _ := ti.Complete("bit", 10); !reflect.DeepEqual(got, []string{"bit", "bitmap"}) {
		t.Errorf("Complete(bit) = %v, want [bit bitmap]", got)
	}
	if docs, _, _ := ti.SearchPrefix("Bit", 10); !reflect.DeepEqual(docs, []uint32{1}) {
		t.Errorf("SearchPrefix(Bit) = %v, want [1]", docs)
	}
}

// TestTermIndexPrefixReadScope asserts the prefix scan range-reads only the dict
// blocks spanning the prefix — not the whole dictionary — and stops fetching as
// soon as a term sorts past the prefix.
func TestTermIndexPrefixReadScope(t *testing.T) {
	postings := map[string]*roaring.Bitmap{}
	var doc uint32
	for c1 := 'a'; c1 <= 'z'; c1++ {
		for c2 := 'a'; c2 <= 'z'; c2++ {
			postings[string(c1)+string(c2)] = bmOf(doc)
			doc++
		}
	}
	var buf bytes.Buffer
	// The 64-byte block cap packs ~8-10 front-coded two-letter entries per block,
	// so the 676-term dictionary spans dozens of blocks.
	if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, true, 64); err != nil {
		t.Fatalf("WriteTermIndexFullDict: %v", err)
	}
	cr := &countReaderAt{r: bytes.NewReader(buf.Bytes())}
	ti, err := OpenTermIndex(cr)
	if err != nil {
		t.Fatalf("OpenTermIndex: %v", err)
	}
	blocks := 0
	ti.router.Iter(func([]byte, uint64) bool { blocks++; return true })
	if blocks < 40 {
		t.Fatalf("dictionary spans %d blocks, want >= 40 for a meaningful scope check", blocks)
	}

	cr.reads = 0
	got, err := ti.Complete("m", 100)
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if len(got) != 26 {
		t.Fatalf("Complete(m) matched %d terms, want 26", len(got))
	}
	// The 26 m-terms span a handful of blocks; a scan that read the dictionary
	// tail (or all the blocks) fails here.
	if cr.reads > 8 || cr.reads >= blocks/4 {
		t.Errorf("Complete(m) issued %d block reads over a %d-block dict, want <= 8", cr.reads, blocks)
	}

	cr.reads = 0
	if got, _ = ti.Complete("mm", 100); !reflect.DeepEqual(got, []string{"mm"}) {
		t.Fatalf("Complete(mm) = %v, want [mm]", got)
	}
	if cr.reads > 2 {
		t.Errorf("Complete(mm) issued %d block reads, want <= 2", cr.reads)
	}
}

// FuzzTermPrefixDifferential builds a small index from fuzz-derived terms and
// checks SearchPrefix / Complete against answers computed naively from the map:
// the block-scan fast path must agree with a full-dictionary filter.
func FuzzTermPrefixDifferential(f *testing.F) {
	f.Add("alpha beta bit bitmap bitmaps zeta", "bit")
	f.Add("aa ab ba bb", "a")
	f.Add("x", "")
	f.Fuzz(func(t *testing.T, words, prefix string) {
		terms := TermTokenize(words)
		if len(terms) == 0 || len(terms) > 64 {
			t.Skip()
		}
		postings := map[string]*roaring.Bitmap{}
		for i, term := range terms {
			if postings[term] == nil {
				postings[term] = roaring.New()
			}
			postings[term].Add(uint32(i))
			postings[term].Add(uint32(65536 + i*7))
		}
		var buf bytes.Buffer
		if _, err := WriteTermIndexFullDict(&buf, postings, 65536, TermLanguageNone, false, false, true, 4); err != nil {
			t.Fatalf("WriteTermIndexFullDict: %v", err)
		}
		ti, err := OpenTermIndex(bytes.NewReader(buf.Bytes()))
		if err != nil {
			t.Fatalf("OpenTermIndex: %v", err)
		}

		folded := strings.Join(TermTokenizeWith(prefix, true), "")
		var wantTerms []string
		want := roaring.New()
		for term, bm := range postings {
			if strings.HasPrefix(term, folded) {
				wantTerms = append(wantTerms, term)
				want.Or(bm)
			}
		}
		slices.Sort(wantTerms)

		gotTerms, err := ti.Complete(folded, len(postings)+1)
		if err != nil {
			t.Fatalf("Complete: %v", err)
		}
		if !slices.Equal(gotTerms, wantTerms) {
			t.Errorf("Complete(%q) = %v, want %v", folded, gotTerms, wantTerms)
		}

		docs, truncated, err := ti.SearchPrefix(folded, int(want.GetCardinality())+1)
		if err != nil || truncated {
			t.Fatalf("SearchPrefix: truncated=%v err=%v", truncated, err)
		}
		if !slices.Equal(docs, want.ToArray()) {
			t.Errorf("SearchPrefix(%q) = %v, want %v", folded, docs, want.ToArray())
		}
	})
}
