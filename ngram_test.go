package roaringrange

import (
	"slices"
	"testing"
)

// TestNgramKeyerMatchesWrapper asserts the reusable NgramKeyer produces byte-identical
// keys to NgramKeysWith, including across successive calls (the reused buffers must be
// reset so an earlier, longer query never leaks into a later one).
func TestNgramKeyerMatchesWrapper(t *testing.T) {
	queries := []string{
		"machine learning",
		"A-b!C",
		"legends travis scott",
		"",                 // empty
		"überwald café",    // non-ASCII
		"a",                // shorter than gramSize
		"machine learning", // repeat to exercise buffer reuse
	}
	for _, caseFold := range []bool{true, false} {
		var k NgramKeyer
		for _, q := range queries {
			want := NgramKeysWith(q, 3, caseFold)
			got := k.Keys(q, 3, caseFold)
			if !slices.Equal(got, want) {
				t.Errorf("Keys(%q, caseFold=%v) = %v, want %v", q, caseFold, got, want)
			}
		}
	}
}

// BenchmarkNgramKeys compares the fresh-allocation NgramKeysWith path against a reused
// NgramKeyer on the per-document ingest hot path (b.ReportAllocs for the task-069 note).
func BenchmarkNgramKeys(b *testing.B) {
	const doc = "the quick brown fox jumps over the lazy dog machine learning"
	b.Run("Fresh", func(b *testing.B) {
		b.ReportAllocs()
		for b.Loop() {
			_ = NgramKeysWith(doc, 3, true)
		}
	})
	b.Run("Reused", func(b *testing.B) {
		b.ReportAllocs()
		var k NgramKeyer
		for b.Loop() {
			_ = k.Keys(doc, 3, true)
		}
	})
}

// TestNgramKeysASCII pins the ASCII trigram key packing and dedup so the
// Rust port can be checked against the same vectors.
func TestNgramKeysASCII(t *testing.T) {
	// "abc" -> ('a'<<16)|('b'<<8)|'c' = 6382179
	got := NgramKeys("abc", 3)
	if len(got) != 1 || got[0] != 6382179 {
		t.Fatalf("NgramKeys(abc) = %v, want [6382179]", got)
	}
	// normalization drops punctuation/case: "A-b!C" == "abc"
	if g := NgramKeys("A-b!C", 3); len(g) != 1 || g[0] != 6382179 {
		t.Fatalf("NgramKeys(A-b!C) = %v, want [6382179]", g)
	}
	// dedup: "aaaa" yields a single distinct trigram
	if g := NgramKeys("aaaa", 3); len(g) != 1 {
		t.Fatalf("NgramKeys(aaaa) = %v, want 1 key", g)
	}
	// too short for a trigram
	if g := NgramKeys("ab", 3); g != nil {
		t.Fatalf("NgramKeys(ab) = %v, want nil", g)
	}
}
