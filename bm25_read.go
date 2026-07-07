package roaringrange

import (
	"io"
	"math"
)

// The read side of the RRSB BM25 impact sidecar (the inverse of WriteImpacts in
// bm25.go). The sidecar is keyed by each term's posting head_off (the join key into
// the paired .rrt dictionary): the entries table is ascending by head_off and
// binary-searched by ranged reads, and a term's per-doc impact bytes are range-read
// from the impacts region. Mirrors rust/src/bm25.rs. See BM25.md.

func init() {
	register(Format{Magic: bm25Magic, Name: "bm25", Ext: ".rrb", Describe: describeImpacts})
}

// ImpactHeader is the RRSB scoring configuration.
type ImpactHeader struct {
	Scale, K1, B, AvgDL float32
	TermCount           uint32
	DocCount            uint64
}

// ImpactIndex is a reference reader over an RRSB sidecar accessed by byte range.
// Only the header is resident; entries and impacts are fetched per lookup.
type ImpactIndex struct {
	r          io.ReaderAt
	hdr        ImpactHeader
	entriesOff int64
	impactsOff int64
}

// OpenImpacts reads and validates the RRSB header.
func OpenImpacts(r io.ReaderAt) (*ImpactIndex, error) {
	h, err := readHeader(r, bm25Magic, bm25HeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != bm25Version {
		return nil, ErrVersion
	}
	return &ImpactIndex{
		r: r,
		hdr: ImpactHeader{
			Scale:     math.Float32frombits(u32(h[8:12])),
			K1:        math.Float32frombits(u32(h[12:16])),
			B:         math.Float32frombits(u32(h[16:20])),
			AvgDL:     math.Float32frombits(u32(h[20:24])),
			TermCount: u32(h[24:28]),
			DocCount:  u64(h[48:56]),
		},
		entriesOff: int64(u64(h[32:40])),
		impactsOff: int64(u64(h[40:48])),
	}, nil
}

// Header returns the scoring configuration.
func (b *ImpactIndex) Header() ImpactHeader { return b.hdr }

// Len reports the number of term entries.
func (b *ImpactIndex) Len() int { return int(b.hdr.TermCount) }

// EntryAt reads the i-th entry: its posting head_off, the byte offset of its
// impacts relative to the impacts region, and the number of docs (impact bytes).
func (b *ImpactIndex) EntryAt(i int) (headOff, rel uint64, card uint32, err error) {
	buf := make([]byte, bm25EntrySize)
	if _, err = b.r.ReadAt(buf, b.entriesOff+int64(i)*bm25EntrySize); err != nil {
		return 0, 0, 0, err
	}
	return u64(buf[0:8]), u64(buf[8:16]), u32(buf[16:20]), nil
}

// Entry finds the entry for a posting head_off by binary search over the ascending
// entries table. ok is false when no term maps to that head_off.
func (b *ImpactIndex) Entry(headOff uint64) (rel uint64, card uint32, ok bool, err error) {
	lo, hi := 0, int(b.hdr.TermCount)
	for lo < hi {
		mid := int(uint(lo+hi) >> 1)
		ho, r, c, err := b.EntryAt(mid)
		if err != nil {
			return 0, 0, false, err
		}
		switch {
		case ho == headOff:
			return r, c, true, nil
		case ho < headOff:
			lo = mid + 1
		default:
			hi = mid
		}
	}
	return 0, 0, false, nil
}

// Impacts returns the per-doc quantized impact bytes for a posting head_off, in the
// posting's doc order. ok is false when no term maps to that head_off.
func (b *ImpactIndex) Impacts(headOff uint64) ([]byte, bool, error) {
	rel, card, ok, err := b.Entry(headOff)
	if err != nil || !ok {
		return nil, ok, err
	}
	buf, err := boundedRead(b.r, b.impactsOff+int64(rel), uint64(card))
	if err != nil {
		return nil, false, err
	}
	return buf, true, nil
}

// describeImpacts reads only the RRSB header for `info`.
func describeImpacts(r io.ReaderAt) (*FileInfo, error) {
	b, err := OpenImpacts(r)
	if err != nil {
		return nil, err
	}
	return &FileInfo{
		Magic: bm25Magic, Name: "bm25", Ext: ".rrb", Version: bm25Version,
		Fields: []Field{
			{"terms", b.hdr.TermCount},
			{"docs", b.hdr.DocCount},
			{"k1", b.hdr.K1},
			{"b", b.hdr.B},
			{"avgdl", b.hdr.AvgDL},
		},
	}, nil
}
