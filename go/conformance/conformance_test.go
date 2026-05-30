// Package conformance cross-checks the roaringsearch builder against the
// roaringrange transcoder + reference reader. roaringsearch and roaringrange
// each implement n-gram key derivation independently (and the Rust reader a
// third time); this test is the guard that they stay byte-compatible.
package conformance

import (
	"bytes"
	"os"
	"path/filepath"
	"sort"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
	rr "github.com/freeeve/roaringrange"
	rs "github.com/freeeve/roaringsearch"
)

// corpus exercises shared trigrams, multi-doc matches, Unicode (ASCII fast-path
// vs rune/hash path), and no-match terms.
var corpus = []string{
	"Legends & Lattes",
	"The Lattes of Legend",
	"A History of Coffee",
	"War and Peace",
	"Café society and naïve little cafés",
	"machine learning for coffee roasting",
	"the legend of the lattes returns",
}

// TestConformance builds the corpus with roaringsearch, transcodes it to RRS
// with roaringrange, opens it with the roaringrange reference reader, and asserts
// that single-word queries return the same doc set as roaringsearch's own
// strict-AND Search. Single words are used because roaringsearch tokenizes the
// whole query while roaringrange splits per word — for one word they coincide,
// isolating the n-gram key derivation + format round-trip.
func TestConformance(t *testing.T) {
	idx := rs.NewIndex(3)
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

	queries := []string{
		"lattes", "legend", "legends", "coffee", "history", "war", "peace",
		"machine", "roasting", "café", "cafés", "naïve", "society", "xyzzy", "zz",
	}
	for _, q := range queries {
		want := idx.Search(q)
		got := searchRR(t, ix, q)
		sort.Slice(want, func(i, j int) bool { return want[i] < want[j] })
		if !equalU32(want, got) {
			t.Errorf("query %q: roaringsearch=%v  roaringrange=%v", q, want, got)
		}
	}
}

// searchRR reproduces a strict-AND search through the roaringrange reference
// reader: derive the query's n-gram keys, OR each key's head+tail, AND the keys
// together. An absent key makes the strict-AND result empty.
func searchRR(t *testing.T, ix *rr.Index, q string) []uint32 {
	t.Helper()
	keys := rr.NgramKeys(q, 3)
	if len(keys) == 0 {
		return nil
	}
	var acc *roaring.Bitmap
	for _, k := range keys {
		full := roaring.New()
		present := false
		if hb, ok, err := ix.Head(k); err != nil {
			t.Fatalf("Head: %v", err)
		} else if ok {
			present = true
			b := roaring.New()
			if _, err := b.FromBuffer(append([]byte(nil), hb...)); err != nil {
				t.Fatalf("head FromBuffer: %v", err)
			}
			full.Or(b)
		}
		if tb, ok, err := ix.Tail(k); err != nil {
			t.Fatalf("Tail: %v", err)
		} else if ok {
			present = true
			b := roaring.New()
			if _, err := b.FromBuffer(append([]byte(nil), tb...)); err != nil {
				t.Fatalf("tail FromBuffer: %v", err)
			}
			full.Or(b)
		}
		if !present {
			return nil // absent key -> strict AND is empty
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
