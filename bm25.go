package roaringrange

// The Go build-side writer for the RRSB BM25 impact sidecar (.rrb) — the mirror of
// the Rust bm25::build::write_impacts, byte-for-byte (proven by the shared golden in
// bm25_test.go). The sidecar is keyed by each term's posting head_off (from the
// paired .rrt that WriteTermIndex emits), storing ONE quantized impact byte per
// (term, doc): the BM25 term-frequency component with the document-length norm
// folded in at build time, so query-time scoring is Σ idf(term)·byte·scale/255 with
// no separate norms file. See rust/src/bm25.rs for the frozen layout.

import (
	"encoding/binary"
	"fmt"
	"io"
	"math"
)

const (
	bm25Magic        = "RRSB"
	bm25Version      = 1
	bm25HeaderSize   = 64
	bm25EntrySize    = 20 // head_off u64 + impacts_rel u64 + card u32
	bm25SparseStride = 512

	// DefaultK1 / DefaultB are the BM25 parameters (match the Rust DEFAULT_K1/_B).
	DefaultK1 float32 = 1.2
	DefaultB  float32 = 0.75
)

// QuantizeImpact quantizes one (term, doc) BM25 impact to a byte (1–255, never 0 —
// presence in the posting implies a nonzero contribution): the tf component with the
// document-length norm folded in, scaled by k1+1. The arithmetic is float32
// throughout and rounds half-away-from-zero, matching the Rust quantize_impact
// byte-for-byte (f32→f64 widening is exact, and math.Round uses the same rounding
// rule as f32::round). The saturating float→int + clamp mirrors Rust's `as i64`.
func QuantizeImpact(tf uint32, dl, avgdl, k1, b float32) byte {
	tff := float32(tf)
	s := tff * (k1 + 1) / (tff + k1*(1-b+b*dl/avgdl))
	x := s * 255 / (k1 + 1)
	r := math.Round(float64(x))
	var i int64
	switch {
	case math.IsNaN(r):
		i = 0
	case r >= float64(math.MaxInt64):
		i = math.MaxInt64
	case r <= float64(math.MinInt64):
		i = math.MinInt64
	default:
		i = int64(r)
	}
	if i < 1 {
		i = 1
	}
	if i > 255 {
		i = 255
	}
	return byte(i)
}

// docTF is one (doc, term-frequency) entry in a term's accumulated list.
type docTF struct {
	doc uint32
	tf  uint32
}

// ImpactsAccumulator gathers per-term (doc, tf) plus per-doc lengths over a corpus
// added in ascending doc-ID order (the shared rank order), for WriteImpacts. Mirrors
// the Rust bm25::build::ImpactsAccumulator: the per-term lists are ascending by doc
// by construction, matching posting iteration order.
type ImpactsAccumulator struct {
	tok     *TermTokenizer
	terms   map[string][]docTF
	docLens []uint64
	nextDoc uint32
}

// NewImpactsAccumulator builds an accumulator over tok, which MUST match the .rrt
// build's tokenizer (same language / stop-word config) or the vocabularies diverge.
func NewImpactsAccumulator(tok *TermTokenizer) *ImpactsAccumulator {
	return &ImpactsAccumulator{tok: tok, terms: make(map[string][]docTF)}
}

// AddDoc tokenizes text as the next sequential doc ID and returns that ID.
func (a *ImpactsAccumulator) AddDoc(text string) uint32 {
	doc := a.nextDoc
	a.nextDoc++
	toks := a.tok.Tokenize(text)
	a.docLens = append(a.docLens, uint64(len(toks)))
	tf := make(map[string]uint32, len(toks))
	for _, t := range toks {
		tf[t]++
	}
	for t, n := range tf {
		a.terms[t] = append(a.terms[t], docTF{doc: doc, tf: n})
	}
	return doc
}

// DocCount is the number of documents accumulated so far.
func (a *ImpactsAccumulator) DocCount() uint32 { return uint32(len(a.docLens)) }

// DictEntry is one (term, posting head_off) pair from the paired .rrt dictionary, in
// dictionary order (ascending head_off).
type DictEntry struct {
	Term    string
	HeadOff uint64
}

// WriteImpacts writes the RRSB sidecar to dst for a finished .rrt whose dictionary is
// dict over the stats in acc — byte-for-byte with the Rust write_impacts. Every dict
// term must have accumulated stats (else error — a tokenizer-config mismatch would
// otherwise mis-address every later term). k1/b are the BM25 parameters used.
//
// PERFORMANCE: this emits many small (~20-byte) writes to dst; pass a buffered
// writer (e.g. bufio.Writer) when dst is a file or socket and flush it after. The
// library does not buffer internally.
func WriteImpacts(dst io.Writer, dict []DictEntry, acc *ImpactsAccumulator, k1, b float32) error {
	nDocs := uint64(len(acc.docLens))
	if nDocs == 0 {
		return fmt.Errorf("RRSB build over zero documents")
	}
	var sum uint64
	for _, l := range acc.docLens {
		sum += l
	}
	avgdl := float32(sum) / float32(nDocs)
	scale := k1 + 1

	type entry struct {
		headOff uint64
		rel     uint64
		card    uint32
	}
	entries := make([]entry, 0, len(dict))
	var impacts []byte
	havePrev := false
	var prev uint64
	for _, de := range dict {
		if havePrev && de.HeadOff <= prev {
			return fmt.Errorf("RRSB dict head_offs not ascending")
		}
		havePrev = true
		prev = de.HeadOff
		tfs, ok := acc.terms[de.Term]
		if !ok {
			return fmt.Errorf("dictionary term %q has no accumulated stats — tokenizer mismatch?", de.Term)
		}
		rel := uint64(len(impacts))
		for _, dt := range tfs {
			dl := float32(acc.docLens[dt.doc])
			impacts = append(impacts, QuantizeImpact(dt.tf, dl, avgdl, k1, b))
		}
		entries = append(entries, entry{headOff: de.HeadOff, rel: rel, card: uint32(len(tfs))})
	}

	termCount := uint32(len(entries))
	sparseCount := (len(entries) + bm25SparseStride - 1) / bm25SparseStride
	entriesOff := uint64(bm25HeaderSize + sparseCount*8)
	impactsOff := entriesOff + uint64(len(entries)*bm25EntrySize)

	header := make([]byte, bm25HeaderSize)
	copy(header[0:4], bm25Magic)
	binary.LittleEndian.PutUint16(header[4:6], bm25Version)
	binary.LittleEndian.PutUint16(header[6:8], 0)
	binary.LittleEndian.PutUint32(header[8:12], math.Float32bits(scale))
	binary.LittleEndian.PutUint32(header[12:16], math.Float32bits(k1))
	binary.LittleEndian.PutUint32(header[16:20], math.Float32bits(b))
	binary.LittleEndian.PutUint32(header[20:24], math.Float32bits(avgdl))
	binary.LittleEndian.PutUint32(header[24:28], termCount)
	binary.LittleEndian.PutUint32(header[28:32], bm25SparseStride)
	binary.LittleEndian.PutUint64(header[32:40], entriesOff)
	binary.LittleEndian.PutUint64(header[40:48], impactsOff)
	binary.LittleEndian.PutUint64(header[48:56], nDocs)
	// header[56:64] reserved (zero).
	if _, err := dst.Write(header); err != nil {
		return err
	}
	var le8 [8]byte
	for i := range sparseCount {
		binary.LittleEndian.PutUint64(le8[:], entries[i*bm25SparseStride].headOff)
		if _, err := dst.Write(le8[:]); err != nil {
			return err
		}
	}
	rec := make([]byte, bm25EntrySize)
	for _, e := range entries {
		binary.LittleEndian.PutUint64(rec[0:8], e.headOff)
		binary.LittleEndian.PutUint64(rec[8:16], e.rel)
		binary.LittleEndian.PutUint32(rec[16:20], e.card)
		if _, err := dst.Write(rec); err != nil {
			return err
		}
	}
	if _, err := dst.Write(impacts); err != nil {
		return err
	}
	return nil
}
