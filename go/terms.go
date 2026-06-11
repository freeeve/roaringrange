package roaringrange

// The Go build-side writer for the RRTI v2 term index (.rrt) — the mirror of the
// Rust terms_build::write_term_index_from_postings, byte-for-byte (proven by the
// shared golden in termsplitsetbuild_test.go). The dictionary is partitioned into
// byte-capped, front-coded blocks with a resident router FST (github.com/freeeve/
// fst-go, itself byte-exact against the Rust fst crate) mapping each block's last
// term to its byte range; the postings region holds one
// [tailSize u32 LE][head roaring][tail roaring] block per term in dictionary
// order, head/tail split at the rank head boundary like the trigram RRS.
//
// The tokenizer reproduces the Rust reader/builder contract exactly: maximal runs
// of char::is_alphanumeric (the Unicode Alphabetic property plus Nd/Nl/No — wider
// than unicode.IsLetter/IsDigit), each rune lowercased with the FULL per-rune
// mapping (U+0130 İ expands to "i̇", the one unconditional multi-rune
// lowercase in Unicode), then optional stop-word removal, then optional Snowball
// stemming (github.com/freeeve/go-stemmers, byte-exact against rust-stemmers).

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"io"
	"slices"
	"unicode"

	"github.com/RoaringBitmap/roaring/v2"
	fst "github.com/freeeve/fst-go"
	stemmers "github.com/freeeve/go-stemmers"
)

const (
	rrtiVersion       = 2
	termFlagStemmed   = 1
	termFlagStopwords = 2
	// defaultDictBlockCap is the dict block byte cap when the caller passes 0
	// (== the Rust DEFAULT_DICT_BLOCK_CAP).
	defaultDictBlockCap = 4096
	// termSizeBits packs (off << termSizeBits) | size router/dict locations
	// (== the Rust terms_dict::SIZE_BITS).
	termSizeBits = 24
)

// TermLanguage selects the Snowball stemmer recorded in the RRTI header (the
// on-disk language byte; values match the Rust Language::to_u8).
type TermLanguage uint8

const (
	// TermLanguageNone builds an unstemmed index.
	TermLanguageNone TermLanguage = 0
	// TermLanguageEnglish stems with Snowball English (Porter2).
	TermLanguageEnglish TermLanguage = 1
)

// termStopWords mirrors the Rust STOP_WORDS list (sorted; removed from the index
// and, symmetrically, from queries — only when the index was built with
// stop-word removal).
var termStopWords = []string{
	"a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "had", "has", "have",
	"he", "in", "is", "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were",
	"which", "will", "with",
}

// isTermStopWord reports whether the lowercased token is a stop word.
func isTermStopWord(t string) bool {
	_, ok := slices.BinarySearch(termStopWords, t)
	return ok
}

// otherAlphabetic is Unicode's Other_Alphabetic property — combined with L* and
// Nl it forms the Alphabetic property Rust's char::is_alphabetic tests.
var otherAlphabetic = unicode.Properties["Other_Alphabetic"]

// isRustAlphanumeric mirrors Rust char::is_alphanumeric: Alphabetic (L* + Nl +
// Other_Alphabetic) or numeric (Nd + Nl + No). Wider than the trigram builder's
// IsLetter||IsDigit — e.g. circled digits (No) are alphanumeric here.
func isRustAlphanumeric(r rune) bool {
	return unicode.IsLetter(r) ||
		unicode.Is(unicode.Nl, r) ||
		unicode.Is(otherAlphabetic, r) ||
		unicode.Is(unicode.Nd, r) ||
		unicode.Is(unicode.No, r)
}

// appendLowerRune appends the FULL per-rune lowercase mapping of r, mirroring
// Rust char::to_lowercase: identical to the simple mapping for every rune except
// U+0130 (LATIN CAPITAL LETTER I WITH DOT ABOVE), Unicode's one unconditional
// multi-rune lowercase, which expands to "i" + combining dot above.
func appendLowerRune(dst []rune, r rune) []rune {
	if r == 0x0130 {
		return append(dst, 'i', 0x0307)
	}
	return append(dst, unicode.ToLower(r))
}

// TermTokenize is the base RRTI tokenizer (no stemming / stop words): maximal
// runs of Rust-alphanumeric runes, lowercased with the full per-rune mapping.
// Mirrors the Rust terms::tokenize.
func TermTokenize(text string) []string {
	var tokens []string
	var cur []rune
	for _, r := range text {
		if isRustAlphanumeric(r) {
			cur = appendLowerRune(cur, r)
		} else if len(cur) > 0 {
			tokens = append(tokens, string(cur))
			cur = cur[:0]
		}
	}
	if len(cur) > 0 {
		tokens = append(tokens, string(cur))
	}
	return tokens
}

// TermTokenizer is the configured token-filter chain (base tokenize → optional
// stop-word removal → optional Snowball stemming), fixed at build time and
// recorded in the header so queries tokenize identically. Mirrors the Rust
// terms::Tokenizer.
type TermTokenizer struct {
	stem      *stemmers.Stemmer
	stopwords bool
}

// NewTermTokenizer builds the chain for the given language / stop-word setting.
func NewTermTokenizer(lang TermLanguage, stopwords bool) *TermTokenizer {
	var st *stemmers.Stemmer
	if lang == TermLanguageEnglish {
		st = stemmers.New(stemmers.English)
	}
	return &TermTokenizer{stem: st, stopwords: stopwords}
}

// Tokenize applies the full chain to text.
func (t *TermTokenizer) Tokenize(text string) []string {
	base := TermTokenize(text)
	out := base[:0]
	for _, tok := range base {
		if t.stopwords && isTermStopWord(tok) {
			continue
		}
		if t.stem != nil {
			tok = t.stem.Stem(tok)
		}
		out = append(out, tok)
	}
	return out
}

// uvarintLen is the encoded LEB128 length of v.
func uvarintLen(v uint64) int {
	n := 1
	for v >= 0x80 {
		v >>= 7
		n++
	}
	return n
}

// dictBlock is one emitted front-coded dictionary block.
type dictBlock struct {
	bytes    []byte
	off      uint64
	lastTerm []byte
}

// dictBlockWriter front-codes (term, headOff, headSize) entries pushed in sorted
// byte order into byte-capped blocks. Mirrors the Rust terms_dict::BlockWriter.
type dictBlockWriter struct {
	cap         int
	cur         []byte
	lastTerm    []byte
	prevTerm    []byte
	prevHeadOff uint64
	count       int
	blocks      []dictBlock
	dictLen     uint64
}

func newDictBlockWriter(cap int) *dictBlockWriter {
	if cap == 0 {
		cap = defaultDictBlockCap
	}
	return &dictBlockWriter{cap: cap}
}

// commonPrefixLen is the byte-wise common prefix length of a and b.
func commonPrefixLen(a, b []byte) int {
	n := min(len(a), len(b))
	i := 0
	for i < n && a[i] == b[i] {
		i++
	}
	return i
}

func (w *dictBlockWriter) push(term []byte, headOff, headSize uint64) {
	// Seal first if front-coding this entry would push the block past the cap;
	// the entry then opens a fresh block as its full (unshared) first term.
	if w.count > 0 {
		shared := commonPrefixLen(w.prevTerm, term)
		delta := headOff - w.prevHeadOff
		entryLen := uvarintLen(uint64(shared)) +
			uvarintLen(uint64(len(term)-shared)) +
			(len(term) - shared) +
			uvarintLen(delta) +
			uvarintLen(headSize)
		if len(w.cur)+entryLen > w.cap {
			w.flush()
		}
	}

	first := w.count == 0
	shared := 0
	if !first {
		shared = commonPrefixLen(w.prevTerm, term)
	}
	headOffD := headOff
	if !first {
		headOffD = headOff - w.prevHeadOff
	}
	w.cur = binary.AppendUvarint(w.cur, uint64(shared))
	w.cur = binary.AppendUvarint(w.cur, uint64(len(term)-shared))
	w.cur = append(w.cur, term[shared:]...)
	w.cur = binary.AppendUvarint(w.cur, headOffD)
	w.cur = binary.AppendUvarint(w.cur, headSize)

	w.lastTerm = append(w.lastTerm[:0], term...)
	w.prevTerm = append(w.prevTerm[:0], term...)
	w.prevHeadOff = headOff
	w.count++
}

func (w *dictBlockWriter) flush() {
	if w.count == 0 {
		return
	}
	w.blocks = append(w.blocks, dictBlock{
		bytes:    w.cur,
		off:      w.dictLen,
		lastTerm: w.lastTerm,
	})
	w.dictLen += uint64(len(w.cur))
	w.cur = nil
	w.lastTerm = nil
	w.count = 0
	w.prevTerm = w.prevTerm[:0]
	w.prevHeadOff = 0
}

func (w *dictBlockWriter) finish() []dictBlock {
	w.flush()
	return w.blocks
}

// WriteTermIndex writes an RRTI v2 term index over postings (term → bitmap of
// the shared rank-order doc IDs) to dst — byte-for-byte the Rust
// write_term_index_from_postings. headBoundary is the head/tail doc-ID split (a
// multiple of 65536); language/stopwords are recorded in the header so the
// reader tokenizes queries identically; blockCap of 0 takes the default.
func WriteTermIndex(dst io.Writer, postings map[string]*roaring.Bitmap, headBoundary uint32, lang TermLanguage, stopwords bool, blockCap int) error {
	terms := make([]string, 0, len(postings))
	for t := range postings {
		terms = append(terms, t)
	}
	slices.Sort(terms) // byte-lexicographic, the dictionary's required order

	var region bytes.Buffer
	blocks := newDictBlockWriter(blockCap)
	for _, term := range terms {
		head, tail, err := splitBitmapHB(postings[term], headBoundary)
		if err != nil {
			return err
		}
		headOff := uint64(region.Len())
		if len(head) >= 1<<termSizeBits {
			return fmt.Errorf("term %q: head posting %d B exceeds the 24-bit size limit", term, len(head))
		}
		if headOff >= 1<<(64-termSizeBits) {
			return fmt.Errorf("postings region exceeds the 40-bit offset limit")
		}
		var tailLen [4]byte
		binary.LittleEndian.PutUint32(tailLen[:], uint32(len(tail)))
		region.Write(tailLen[:])
		region.Write(head)
		region.Write(tail)
		blocks.push([]byte(term), headOff, uint64(len(head)))
	}
	bs := blocks.finish()

	// Router FST: each block's last term → (blockOff << 24) | blockLen, offsets
	// relative to the dict region. Keys arrive sorted and distinct.
	builder := fst.NewBuilder()
	var dictLen uint64
	for _, b := range bs {
		blockLen := uint64(len(b.bytes))
		if blockLen >= 1<<termSizeBits {
			return fmt.Errorf("dict block exceeds the 24-bit block-length limit")
		}
		if b.off >= 1<<(64-termSizeBits) {
			return fmt.Errorf("dict region exceeds the 40-bit block-offset limit")
		}
		if err := builder.Insert(b.lastTerm, b.off<<termSizeBits|blockLen); err != nil {
			return fmt.Errorf("router fst insert: %w", err)
		}
		dictLen += blockLen
	}
	routerBytes := builder.Finish()

	// Header (40 B): flags record the tokenizer; reserved[0] (offset 36) carries
	// the stemmer language byte.
	var flags uint16
	if lang != TermLanguageNone {
		flags |= termFlagStemmed
	}
	if stopwords {
		flags |= termFlagStopwords
	}
	capUsed := blockCap
	if capUsed == 0 {
		capUsed = defaultDictBlockCap
	}
	header := make([]byte, 0, 40)
	header = append(header, "RRTI"...)
	header = binary.LittleEndian.AppendUint16(header, rrtiVersion)
	header = binary.LittleEndian.AppendUint16(header, flags)
	header = binary.LittleEndian.AppendUint32(header, uint32(len(terms)))
	header = binary.LittleEndian.AppendUint32(header, headBoundary)
	header = binary.LittleEndian.AppendUint64(header, uint64(len(routerBytes)))
	header = binary.LittleEndian.AppendUint64(header, dictLen)
	header = binary.LittleEndian.AppendUint32(header, uint32(capUsed))
	header = append(header, byte(lang), 0, 0, 0)
	if _, err := dst.Write(header); err != nil {
		return err
	}
	if _, err := dst.Write(routerBytes); err != nil {
		return err
	}
	for _, b := range bs {
		if _, err := dst.Write(b.bytes); err != nil {
			return err
		}
	}
	_, err := dst.Write(region.Bytes())
	return err
}
