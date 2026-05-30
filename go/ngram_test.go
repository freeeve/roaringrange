package roaringrange

import "testing"

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
