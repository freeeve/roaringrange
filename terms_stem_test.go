package roaringrange

import (
	"bufio"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"

	stemmers "github.com/freeeve/go-stemmers"
)

// TestTokenizerStemMatchesRustGolden asserts NewTermTokenizerFull wires the correct Snowball
// stemmer for every language: for each (language byte, word) in the Rust-generated golden
// (rust/examples/gen_tokenizer_stem_golden.rs) the Go tokenizer must reproduce the exact
// stemmed tokens, proving the Go TermLanguage -> go-stemmers map matches the Rust Language ->
// rust-stemmers map byte-for-byte. The words stem non-trivially and to distinct roots, so a
// wrong or swapped mapping is caught.
func TestTokenizerStemMatchesRustGolden(t *testing.T) {
	f, err := os.Open("testdata/tokenizer_stem_golden.txt")
	if err != nil {
		t.Fatalf("open golden: %v", err)
	}
	defer f.Close()

	seen := 0
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if strings.TrimSpace(line) == "" {
			continue
		}
		parts := strings.SplitN(line, "\t", 3)
		if len(parts) != 3 {
			t.Fatalf("malformed golden line %q", line)
		}
		byteVal, err := strconv.ParseUint(parts[0], 10, 8)
		if err != nil {
			t.Fatalf("bad language byte in %q: %v", line, err)
		}
		lang := TermLanguage(byteVal)
		input, want := parts[1], strings.Fields(parts[2])

		tok := NewTermTokenizerFull(lang, true /*stem*/, false /*stopwords*/, true /*caseFold*/)
		got := tok.Tokenize(input)
		if !slices.Equal(got, want) {
			t.Errorf("lang %d %q: Tokenize = %q, want %q (Rust golden)", byteVal, input, got, want)
		}
		seen++
	}
	if err := sc.Err(); err != nil {
		t.Fatalf("scan golden: %v", err)
	}
	if seen != 18 {
		t.Fatalf("golden covered %d languages, want all 18 Snowball languages", seen)
	}
}

// TestStemAlgorithmCoversAllLanguages asserts the TermLanguage -> stemmers.Algorithm map is
// complete and one-to-one: every Snowball byte 1..=18 builds a stemmer, distinct algorithms map
// to distinct languages, and TermLanguageNone stays unstemmed even when stemming is requested.
func TestStemAlgorithmCoversAllLanguages(t *testing.T) {
	if len(stemAlgorithm) != 18 {
		t.Fatalf("stemAlgorithm has %d entries, want 18", len(stemAlgorithm))
	}
	for b := 1; b <= 18; b++ {
		lang := TermLanguage(b)
		if _, ok := stemAlgorithm[lang]; !ok {
			t.Errorf("language byte %d has no stemmer mapping", b)
		}
		if tok := NewTermTokenizerFull(lang, true, false, true); tok.stem == nil {
			t.Errorf("language byte %d: stem requested but stemmer is nil", b)
		}
	}
	if tok := NewTermTokenizerFull(TermLanguageNone, true, false, true); tok.stem != nil {
		t.Errorf("TermLanguageNone must leave the stemmer nil even when stem is requested")
	}
	seen := map[stemmers.Algorithm]TermLanguage{}
	for lang, algo := range stemAlgorithm {
		if prev, dup := seen[algo]; dup {
			t.Errorf("algorithm %v mapped by both language %d and %d", algo, prev, lang)
		}
		seen[algo] = lang
	}
}
