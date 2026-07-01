package roaringrange

import (
	"slices"
	"testing"
)

// TestStopwordListsWellformed checks every embedded stop-word list is non-empty, strictly
// byte-sorted, and de-duplicated (the binary-search invariant), and that English is the fixed
// back-compat list. The lists are the SAME embedded stopwords/<lang>.txt files the Rust core
// uses (include_str!), so this also guards their cross-language byte-identity.
func TestStopwordListsWellformed(t *testing.T) {
	for b := TermLanguage(1); b <= 18; b++ {
		list := termStopWordList(b)
		if len(list) == 0 {
			t.Fatalf("empty stop-word list for language byte %d", b)
		}
		if !slices.IsSorted(list) {
			t.Fatalf("stop-word list for byte %d is not sorted", b)
		}
		for i := 1; i < len(list); i++ {
			if list[i] == list[i-1] {
				t.Fatalf("duplicate %q in list for byte %d", list[i], b)
			}
		}
	}
	english := termStopWordList(TermLanguageEnglish)
	want := []string{
		"a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "had",
		"has", "have", "he", "in", "is", "it", "its", "of", "on", "or", "that", "the",
		"this", "to", "was", "were", "which", "will", "with",
	}
	if !slices.Equal(english, want) {
		t.Fatalf("english stop-word list mismatch:\n got %q\nwant %q", english, want)
	}
}

// TestStopwordsKeyOnLanguage verifies stop-word membership is per-language: "le" is a French
// stop word but not an English one, and vice-versa for "the".
func TestStopwordsKeyOnLanguage(t *testing.T) {
	cases := []struct {
		tok  string
		lang TermLanguage
		want bool
	}{
		{"le", TermLanguageFrench, true},
		{"le", TermLanguageEnglish, false},
		{"the", TermLanguageEnglish, true},
		{"the", TermLanguageFrench, false},
		{"the", TermLanguageNone, false},
	}
	for _, c := range cases {
		if got := isTermStopWord(c.tok, c.lang); got != c.want {
			t.Fatalf("isTermStopWord(%q, %d) = %v, want %v", c.tok, c.lang, got, c.want)
		}
	}
}

// TestStopwordsWithoutStemming verifies the tokenizer can strip a language's stop words
// WITHOUT stemming (task 055): "les" is dropped, "chats" is kept verbatim (not reduced).
func TestStopwordsWithoutStemming(t *testing.T) {
	tok := NewTermTokenizerFull(TermLanguageFrench, false /*stem*/, true /*stopwords*/, true /*caseFold*/)
	got := tok.Tokenize("les chats")
	if want := []string{"chats"}; !slices.Equal(got, want) {
		t.Fatalf("Tokenize = %q, want %q", got, want)
	}
}

// TestWriteTermIndexStopwordsRequireLanguage verifies the writer rejects a stop-word filter
// with no language (no silent English fallback).
func TestWriteTermIndexStopwordsRequireLanguage(t *testing.T) {
	if err := WriteTermIndexFull(nil, nil, 65536, TermLanguageNone, false, true, true, 0); err == nil {
		t.Fatal("stopwords with no language must error")
	}
}
