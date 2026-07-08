package roaringrange

import (
	"bytes"
	"encoding/binary"
	"io"
	"iter"
	"strings"

	"github.com/RoaringBitmap/roaring/v2"
	fst "github.com/freeeve/fst-go"
)

// The read side of the RRTI v2 term index (the inverse of WriteTermIndex in
// terms.go). The resident router FST maps each front-coded dict block's last term
// to its byte range; a lookup routes to one block, front-decodes it, and range-reads
// the term's [tailSize u32][head][tail] posting. The query tokenizer is rebuilt from
// the header flags + language byte so lookups tokenize exactly as the build did.
// Mirrors rust/src/terms.rs. See TERMS.md.

const rrtiHeaderSize = 40

// maxPrefixTerms is the upper bound on the number of prefix-matching dictionary
// terms SearchPrefix unions: a 1-2 char prefix over a huge vocabulary would
// otherwise fan out an unbounded number of posting reads, and the union of this
// many rank-ordered heads fills any realistic limit. Mirrors the Rust reader's
// MAX_PREFIX_TERMS; a prefix matching more terms is reported truncated.
const maxPrefixTerms = 2048

func init() {
	register(Format{Magic: "RRTI", Name: "terms", Ext: ".rrt", Describe: describeTerms})
}

// TermHeader is the RRTI index configuration decoded from the header.
type TermHeader struct {
	TermCount     uint32
	HeadBoundary  uint32
	BlockCap      uint32
	Language      TermLanguage
	Stemmed       bool
	Stopwords     bool
	CaseSensitive bool
}

// TermIndex is a reference reader over an RRTI index accessed by byte range. Only
// the router FST is resident; dict blocks and postings are range-read per lookup.
type TermIndex struct {
	r         io.ReaderAt
	router    *fst.FST
	hdr       TermHeader
	tok       *TermTokenizer
	dictStart int64 // byte offset of the front-coded dict region
	postStart int64 // byte offset of the postings region
}

// OpenTermIndex reads and validates the RRTI header and resident router FST. Dict
// blocks and postings are read lazily.
func OpenTermIndex(r io.ReaderAt) (*TermIndex, error) {
	h, err := readHeader(r, "RRTI", rrtiHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != rrtiVersion {
		return nil, ErrVersion
	}
	flags := u16(h[6:8])
	routerLen := u64(h[16:24])
	dictLen := u64(h[24:32])
	hdr := TermHeader{
		TermCount:     u32(h[8:12]),
		HeadBoundary:  u32(h[12:16]),
		BlockCap:      u32(h[32:36]),
		Language:      TermLanguage(h[36]),
		Stemmed:       flags&termFlagStemmed != 0,
		Stopwords:     flags&termFlagStopwords != 0,
		CaseSensitive: flags&termFlagCaseSensitive != 0,
	}

	routerBytes, err := boundedRead(r, rrtiHeaderSize, routerLen)
	if err != nil {
		return nil, err
	}
	router, err := fst.New(routerBytes)
	if err != nil {
		return nil, err
	}
	dictStart := int64(rrtiHeaderSize) + int64(routerLen)
	return &TermIndex{
		r:      r,
		router: router,
		hdr:    hdr,
		tok: NewTermTokenizerFull(hdr.Language, hdr.Stemmed, hdr.Stopwords,
			!hdr.CaseSensitive),
		dictStart: dictStart,
		postStart: dictStart + int64(dictLen),
	}, nil
}

// Header returns the index configuration.
func (t *TermIndex) Header() TermHeader { return t.hdr }

// dictItem is one front-decoded dictionary entry within a block.
type dictItem struct {
	term     string
	headOff  uint64
	headSize uint64
}

// decodeBlock front-decodes one dict block into its entries. Mirrors the inverse of
// dictBlockWriter.push: each entry is uvarint(shared), uvarint(suffixLen), suffix,
// uvarint(headOffDelta), uvarint(headSize); the first entry's delta is absolute.
func decodeBlock(block []byte) ([]dictItem, error) {
	var items []dictItem
	var prevTerm []byte
	var prevHeadOff uint64
	pos := 0
	for pos < len(block) {
		shared, n := binary.Uvarint(block[pos:])
		if n <= 0 {
			return nil, ErrTruncated
		}
		pos += n
		suffixLen, n := binary.Uvarint(block[pos:])
		if n <= 0 {
			return nil, ErrTruncated
		}
		pos += n
		if uint64(shared) > uint64(len(prevTerm)) || pos+int(suffixLen) > len(block) {
			return nil, ErrTruncated
		}
		term := make([]byte, 0, int(shared)+int(suffixLen))
		term = append(term, prevTerm[:shared]...)
		term = append(term, block[pos:pos+int(suffixLen)]...)
		pos += int(suffixLen)
		delta, n := binary.Uvarint(block[pos:])
		if n <= 0 {
			return nil, ErrTruncated
		}
		pos += n
		headSize, n := binary.Uvarint(block[pos:])
		if n <= 0 {
			return nil, ErrTruncated
		}
		pos += n
		headOff := prevHeadOff + delta
		items = append(items, dictItem{term: string(term), headOff: headOff, headSize: headSize})
		prevTerm = term
		prevHeadOff = headOff
	}
	return items, nil
}

// block reads and decodes the dict block at (blockOff, blockLen) relative to the
// dict region.
func (t *TermIndex) block(blockVal uint64) ([]dictItem, error) {
	blockOff := blockVal >> termSizeBits
	blockLen := blockVal & ((1 << termSizeBits) - 1)
	raw, err := boundedRead(t.r, t.dictStart+int64(blockOff), blockLen)
	if err != nil {
		return nil, err
	}
	return decodeBlock(raw)
}

// find locates an exact dictionary token: it routes to the block whose last term is
// >= token, decodes it, and returns the matching entry.
func (t *TermIndex) find(token string) (dictItem, bool, error) {
	_, val, ok := t.router.Ge([]byte(token))
	if !ok {
		return dictItem{}, false, nil
	}
	items, err := t.block(val)
	if err != nil {
		return dictItem{}, false, err
	}
	for _, it := range items {
		if it.term == token {
			return it, true, nil
		}
	}
	return dictItem{}, false, nil
}

// readPosting range-reads and merges a term's [tailSize u32][head][tail] posting.
func (t *TermIndex) readPosting(it dictItem) (*roaring.Bitmap, error) {
	tailLenBuf, err := boundedRead(t.r, t.postStart+int64(it.headOff), 4)
	if err != nil {
		return nil, err
	}
	tailSize := u32(tailLenBuf)
	buf, err := boundedRead(t.r, t.postStart+int64(it.headOff)+4, it.headSize+uint64(tailSize))
	if err != nil {
		return nil, err
	}
	head, err := deserializeBitmap(buf[:it.headSize])
	if err != nil {
		return nil, err
	}
	tail, err := deserializeBitmap(buf[it.headSize:])
	if err != nil {
		return nil, err
	}
	head.Or(tail)
	return head, nil
}

// readHead range-reads a term's tail-size word and head bitmap in one read,
// returning the head and the tail's location for an optional follow-up read (the
// head holds the top-ranked docs, so a union that fills its limit from heads
// alone never pays for the tails).
func (t *TermIndex) readHead(it dictItem) (head *roaring.Bitmap, tailOff int64, tailSize uint32, err error) {
	buf, err := boundedRead(t.r, t.postStart+int64(it.headOff), 4+it.headSize)
	if err != nil {
		return nil, 0, 0, err
	}
	head, err = deserializeBitmap(buf[4:])
	if err != nil {
		return nil, 0, 0, err
	}
	return head, t.postStart + int64(it.headOff) + 4 + int64(it.headSize), u32(buf[:4]), nil
}

// foldPrefix normalizes a query prefix the way the index's tokenizer normalized
// its terms: lowercased per-rune for a case-folding index, verbatim for a
// case-sensitive one. The prefix path skips stemming and stop words by design (a
// prefix is already fuzzy). Mirrors the Rust reader's fold_prefix.
func (t *TermIndex) foldPrefix(prefix string) string {
	if t.hdr.CaseSensitive {
		return prefix
	}
	var out []rune
	for _, r := range prefix {
		out = appendLowerRune(out, r)
	}
	return string(out)
}

// scanPrefix walks the dictionary forward from the first block that could contain
// the folded prefix p, range-reading only the blocks spanning the prefix: the
// router walk skips blocks whose last term sorts before p without fetching them,
// and both the walk and the fetches stop as soon as a term sorts past the prefix
// or max entries are found. Returns the matching entries in dictionary order and
// whether more than max matched. Mirrors the Rust reader's scan_prefix.
func (t *TermIndex) scanPrefix(p string, max int) (items []dictItem, truncated bool, err error) {
	if max <= 0 {
		return nil, false, nil
	}
	pb := []byte(p)
	var scanErr error
	t.router.Iter(func(key []byte, val uint64) bool {
		if bytes.Compare(key, pb) < 0 {
			return true // block's last term sorts before the prefix: skip, no fetch
		}
		blockItems, err := t.block(val)
		if err != nil {
			scanErr = err
			return false
		}
		for _, it := range blockItems {
			if it.term < p {
				continue // before the prefix (only possible in the first fetched block)
			}
			if !strings.HasPrefix(it.term, p) {
				return false // sorted: the prefix range has ended
			}
			if len(items) == max {
				truncated = true
				return false
			}
			items = append(items, it)
		}
		return true
	})
	if scanErr != nil {
		return nil, false, scanErr
	}
	return items, truncated, nil
}

// Complete autocompletes prefix: up to maxTerms dictionary terms that start with
// it, in lexicographic order. The prefix is case-folded the way the index's
// tokenizer folds (verbatim for a case-sensitive index). Range-reads only the
// dict blocks spanning the prefix. Mirrors the Rust reader's complete.
func (t *TermIndex) Complete(prefix string, maxTerms int) ([]string, error) {
	items, _, err := t.scanPrefix(t.foldPrefix(prefix), maxTerms)
	if err != nil {
		return nil, err
	}
	terms := make([]string, len(items))
	for i, it := range items {
		terms[i] = it.term
	}
	return terms, nil
}

// TermPosting pairs a prefix-matched dictionary term with its full posting.
type TermPosting struct {
	Term    string
	Posting *roaring.Bitmap
}

// PrefixPostings returns up to limit dictionary terms starting with prefix, in
// lexicographic order, each with its full posting. truncated is true when more
// than limit terms matched. The prefix is case-folded the way the index's
// tokenizer folds; only the dict blocks spanning the prefix and the matched
// terms' postings are range-read.
func (t *TermIndex) PrefixPostings(prefix string, limit int) ([]TermPosting, bool, error) {
	items, truncated, err := t.scanPrefix(t.foldPrefix(prefix), limit)
	if err != nil {
		return nil, false, err
	}
	out := make([]TermPosting, 0, len(items))
	for _, it := range items {
		bm, err := t.readPosting(it)
		if err != nil {
			return nil, false, err
		}
		out = append(out, TermPosting{Term: it.term, Posting: bm})
	}
	return out, truncated, nil
}

// SearchPrefix returns up to limit doc IDs matching any dictionary term that
// starts with prefix (the union of every matching term's posting), ascending —
// which is descending rank under the rank-remapped doc-ID space. At most
// maxPrefixTerms terms are unioned; truncated is true when the prefix matched
// more, making the result a bounded approximation of the full union. Head
// bitmaps are read first and the tails only when the heads underfill limit,
// mirroring the Rust reader's search_prefix_capped / union_locs.
func (t *TermIndex) SearchPrefix(prefix string, limit int) (docs []uint32, truncated bool, err error) {
	if limit <= 0 {
		return nil, false, nil
	}
	items, truncated, err := t.scanPrefix(t.foldPrefix(prefix), maxPrefixTerms)
	if err != nil {
		return nil, false, err
	}
	type tailLoc struct {
		off  int64
		size uint32
	}
	acc := roaring.New()
	var tails []tailLoc
	for _, it := range items {
		head, tailOff, tailSize, err := t.readHead(it)
		if err != nil {
			return nil, false, err
		}
		acc.Or(head)
		if tailSize > 0 {
			tails = append(tails, tailLoc{off: tailOff, size: tailSize})
		}
	}
	if acc.GetCardinality() < uint64(limit) {
		for _, tl := range tails {
			buf, err := boundedRead(t.r, tl.off, uint64(tl.size))
			if err != nil {
				return nil, false, err
			}
			tail, err := deserializeBitmap(buf)
			if err != nil {
				return nil, false, err
			}
			acc.Or(tail)
		}
	}
	docs = make([]uint32, 0, min(uint64(limit), acc.GetCardinality()))
	di := acc.Iterator()
	for di.HasNext() && len(docs) < limit {
		docs = append(docs, di.Next())
	}
	return docs, truncated, nil
}

// LookupTerm returns the doc posting for an exact dictionary token (already in the
// stored, tokenized form). ok is false when the token is absent.
func (t *TermIndex) LookupTerm(token string) (*roaring.Bitmap, bool, error) {
	it, ok, err := t.find(token)
	if err != nil || !ok {
		return nil, false, err
	}
	bm, err := t.readPosting(it)
	return bm, err == nil, err
}

// Posting tokenizes query with the index's tokenizer and returns the union of the
// resulting tokens' postings (a single-word query resolves to one token). ok is
// false when the query yields no token or none is present.
func (t *TermIndex) Posting(query string) (*roaring.Bitmap, bool, error) {
	tokens := t.tok.Tokenize(query)
	var out *roaring.Bitmap
	any := false
	for _, tok := range tokens {
		bm, ok, err := t.LookupTerm(tok)
		if err != nil {
			return nil, false, err
		}
		if !ok {
			continue
		}
		if out == nil {
			out = bm
		} else {
			out.Or(bm)
		}
		any = true
	}
	return out, any, nil
}

// Terms iterates every dictionary term with its posting head offset, in
// byte-lexicographic dictionary order, decoding one block at a time.
func (t *TermIndex) Terms() iter.Seq2[string, uint64] {
	return func(yield func(string, uint64) bool) {
		var iterErr error
		t.router.Iter(func(_ []byte, val uint64) bool {
			items, err := t.block(val)
			if err != nil {
				iterErr = err
				return false
			}
			for _, it := range items {
				if !yield(it.term, it.headOff) {
					return false
				}
			}
			return true
		})
		_ = iterErr
	}
}

// Dict collects the whole dictionary as (term, head_off) entries in dictionary
// order (the shape WriteImpacts consumes and WriteTermIndexFullDict returns).
func (t *TermIndex) Dict() []DictEntry {
	var out []DictEntry
	for term, headOff := range t.Terms() {
		out = append(out, DictEntry{Term: term, HeadOff: headOff})
	}
	return out
}

// describeTerms reads only the RRTI header for `info`.
func describeTerms(r io.ReaderAt) (*FileInfo, error) {
	t, err := OpenTermIndex(r)
	if err != nil {
		return nil, err
	}
	return &FileInfo{
		Magic: "RRTI", Name: "terms", Ext: ".rrt", Version: rrtiVersion,
		Fields: []Field{
			{"terms", t.hdr.TermCount},
			{"language", uint8(t.hdr.Language)},
			{"stemmed", t.hdr.Stemmed},
			{"stopwords", t.hdr.Stopwords},
			{"caseSensitive", t.hdr.CaseSensitive},
			{"headBoundary", t.hdr.HeadBoundary},
			{"blockCap", t.hdr.BlockCap},
		},
	}, nil
}
