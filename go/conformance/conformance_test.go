// Package conformance cross-checks the roaringsearch builder against the
// roaringrange transcoder + reference reader. roaringsearch and roaringrange
// each implement n-gram key derivation independently (and the Rust reader a
// third time); this test is the guard that they stay byte-compatible.
package conformance

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
	rr "github.com/freeeve/roaringrange/go"
	rs "github.com/freeeve/roaringsearch"
)

// corpus exercises shared trigrams, multi-doc matches, Unicode (ASCII fast-path
// vs rune/hash path), the letter/digit normalization filter (the trailing doc
// carries Nl/No runes — Ⅰ ② ① — that IsLetter||IsDigit must drop), and no-match
// terms.
var corpus = []string{
	"Legends & Lattes",
	"The Lattes of Legend",
	"A History of Coffee",
	"War and Peace",
	"Café society and naïve little cafés",
	"machine learning for coffee roasting",
	"the legend of the lattes returns",
	"Volume Ⅰ notes ② and ① of naïveté",
}

// TestConformance builds the corpus with roaringsearch, transcodes it to RRS
// with roaringrange, opens it with the roaringrange reference reader, and asserts
// that single-word queries return the same doc set as roaringsearch's own
// strict-AND Search. Single words are used because roaringsearch tokenizes the
// whole query while roaringrange splits per word — for one word they coincide,
// isolating the n-gram key derivation + format round-trip. It runs across gram
// sizes 2 (32-bit packing), 3 (ASCII 8-bit packing), and 8 (the packing
// boundary) so every keying path is pinned against the builder, not just gram 3.
func TestConformance(t *testing.T) {
	cases := []struct {
		gram    int
		queries []string
	}{
		{2, []string{"la", "co", "wa", "naïve", "café", "notes", "zz"}},
		{3, []string{
			"lattes", "legend", "legends", "coffee", "history", "war", "peace",
			"machine", "roasting", "café", "cafés", "naïve", "society", "notes",
			"volume", "naïveté", "Ⅰ", "notes②", "xyzzy", "zz",
		}},
		{8, []string{"roasting", "learning", "absentee"}},
	}
	for _, tc := range cases {
		t.Run(fmt.Sprintf("gram%d", tc.gram), func(t *testing.T) {
			runConformance(t, tc.gram, tc.queries)
		})
	}
}

// runConformance builds the corpus at gramSize, round-trips it through the
// transcoder + reference reader, and asserts every query matches roaringsearch.
func runConformance(t *testing.T, gram int, queries []string) {
	t.Helper()
	idx := rs.NewIndex(gram)
	for i, d := range corpus {
		idx.Add(uint32(i), d)
	}

	dir := t.TempDir()
	ftsr := filepath.Join(dir, "corpus.ftsr")
	if err := idx.SaveToFile(ftsr); err != nil {
		t.Fatalf("SaveToFile: %v", err)
	}
	src, err := os.Open(ftsr)
	if err != nil {
		t.Fatalf("open ftsr: %v", err)
	}
	var rrs bytes.Buffer
	if err := rr.Transcode(src, &rrs); err != nil {
		t.Fatalf("Transcode: %v", err)
	}
	src.Close()

	ix, err := rr.Open(bytes.NewReader(rrs.Bytes()))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}

	for _, q := range queries {
		want := idx.Search(q)
		got := searchRR(t, ix, q, gram)
		sort.Slice(want, func(i, j int) bool { return want[i] < want[j] })
		if !equalU32(want, got) {
			t.Errorf("gram=%d query %q: roaringsearch=%v  roaringrange=%v", gram, q, want, got)
		}
	}
}

// searchRR reproduces a strict-AND search through the roaringrange reference
// reader: derive the query's n-gram keys, deserialize each key's (v3 single)
// posting, AND the keys together. An absent key makes the strict-AND result empty.
func searchRR(t *testing.T, ix *rr.Index, q string, gram int) []uint32 {
	t.Helper()
	keys := rr.NgramKeys(q, gram)
	if len(keys) == 0 {
		return nil
	}
	var acc *roaring.Bitmap
	for _, k := range keys {
		pb, ok, err := ix.Posting(k)
		if err != nil {
			t.Fatalf("Posting: %v", err)
		}
		if !ok {
			return nil // absent key -> strict AND is empty
		}
		full := roaring.New()
		if _, err := full.FromBuffer(append([]byte(nil), pb...)); err != nil {
			t.Fatalf("posting FromBuffer: %v", err)
		}
		if acc == nil {
			acc = full
		} else {
			acc.And(full)
		}
	}
	if acc == nil {
		return nil
	}
	return acc.ToArray()
}

func equalU32(a, b []uint32) bool {
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
