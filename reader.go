package roaringrange

import (
	"encoding/binary"
	"io"
	"sort"
)

// maxReadBytes bounds any single buffer the RRS reader allocates from an
// untrusted header field -- the resident sparse index during Open, a dict block
// during lookup, or a posting during Posting. Index bytes may come from a
// hostile origin or a corrupt/partial upload, so a crafted header must produce a
// recoverable error rather than a multi-GB allocation (an OOM abort) or, on a
// 32-bit build, a negative length. 1 GiB is far above any legitimate value (a
// 100+ GB monolith's resident sparse index is tens of MB and its largest posting
// is well under 100 MB) while fitting a 32-bit int. Mirrors the checked size math
// the Rust reader adopted in 45fa50f.
const maxReadBytes = 1 << 30

// dictRec is one parsed v3 RRS dictionary entry: a key and its posting's byte range.
type dictRec struct {
	key    uint64
	offset uint64
	size   uint32
}

// Index is a reference reader over a v3 RRS index accessed by byte range. It
// reads only the 16-byte header and the sparse index up front (mirroring the
// browser reader's boot ranged GETs); the dictionary and postings are fetched
// lazily, one ranged ReadAt per block, never in their entirety. See FORMAT.md.
type Index struct {
	r io.ReaderAt
	// GramSize is the n-gram window the index was built with.
	GramSize int
	// CaseFold reports whether queries should lowercase their n-grams before keying
	// (false for a v4 case-sensitive index). Callers deriving keys should use
	// NgramKeysWith(query, GramSize, CaseFold) so they key exactly as the index was built.
	CaseFold   bool
	ngrams     int
	stride     int
	dictStart  int64
	sparseKeys []uint64
}

// Open reads and parses the RRS header and sparse index via ranged reads. It
// does not read the dictionary or postings; those are fetched per lookup.
func Open(r io.ReaderAt) (*Index, error) {
	header := make([]byte, headerSize)
	if _, err := r.ReadAt(header, 0); err != nil {
		return nil, err
	}
	if string(header[0:4]) != Magic {
		return nil, ErrMagic
	}
	// v3 (case-folding) is the default; v4 appends a 2-byte flags field at offset 16 and is
	// emitted only for a case-sensitive index. One extra tiny read fetches the v4 flags (the
	// 16-byte header read can't over-read a v3 file, whose bytes may end at offset 16).
	hdrSize := headerSize
	caseFold := true
	switch v := binary.LittleEndian.Uint16(header[4:6]); v {
	case Version:
	case VersionV4:
		hdrSize = headerSizeV4
		flagsBuf := make([]byte, 2)
		if _, err := r.ReadAt(flagsBuf, headerSize); err != nil {
			return nil, err
		}
		caseFold = binary.LittleEndian.Uint16(flagsBuf)&rrsiFlagCaseSensitive == 0
	default:
		return nil, ErrVersion
	}
	gramSize := int(binary.LittleEndian.Uint16(header[6:8]))
	ngrams := binary.LittleEndian.Uint32(header[8:12])
	stride := binary.LittleEndian.Uint32(header[12:16])
	if stride == 0 {
		return nil, ErrTruncated
	}

	// The header is untrusted: compute the sparse-index layout in uint64 so a
	// large ngrams/stride can't overflow the size math (or yield a negative
	// length on a 32-bit build), and bound the resident allocation before make.
	sparseCount := (uint64(ngrams) + uint64(stride) - 1) / uint64(stride)
	sparseLen := sparseCount * 8
	if sparseLen > maxReadBytes {
		return nil, ErrTruncated
	}
	sparseBytes := make([]byte, sparseLen)
	if sparseCount > 0 {
		if _, err := r.ReadAt(sparseBytes, int64(hdrSize)); err != nil {
			return nil, err
		}
	}
	sparseKeys := make([]uint64, sparseCount)
	for i := range sparseKeys {
		sparseKeys[i] = binary.LittleEndian.Uint64(sparseBytes[i*8:])
	}

	return &Index{
		r:          r,
		GramSize:   gramSize,
		CaseFold:   caseFold,
		ngrams:     int(ngrams),
		stride:     int(stride),
		dictStart:  int64(uint64(hdrSize) + sparseLen),
		sparseKeys: sparseKeys,
	}, nil
}

// lookup resolves a key to its dictionary record using one in-memory sparse
// binary search followed by a single ranged read of the relevant dict block,
// then a binary search within that block. ok is false if the key is absent.
func (s *Index) lookup(key uint64) (rec dictRec, ok bool, err error) {
	if s.ngrams == 0 {
		return dictRec{}, false, nil
	}
	b := sort.Search(len(s.sparseKeys), func(i int) bool { return s.sparseKeys[i] > key }) - 1
	if b < 0 {
		return dictRec{}, false, nil
	}
	base := b * s.stride
	blockLen := s.stride
	if base+blockLen > s.ngrams {
		blockLen = s.ngrams - base
	}
	// blockLen and the block size are derived from untrusted header fields; a
	// corrupt sparse/stride pairing can drive a non-positive or multi-GB length.
	// A valid index always yields blockLen >= 1, so treat anything else as
	// malformed rather than allocating on it.
	blockSize := uint64(blockLen) * dictEntry
	if blockLen <= 0 || blockSize > maxReadBytes {
		return dictRec{}, false, ErrTruncated
	}
	blockBytes := make([]byte, blockSize)
	if _, err := s.r.ReadAt(blockBytes, s.dictStart+int64(base*dictEntry)); err != nil {
		return dictRec{}, false, err
	}
	i := sort.Search(blockLen, func(i int) bool {
		return binary.LittleEndian.Uint64(blockBytes[i*dictEntry:]) >= key
	})
	if i >= blockLen {
		return dictRec{}, false, nil
	}
	off := i * dictEntry
	if binary.LittleEndian.Uint64(blockBytes[off:]) != key {
		return dictRec{}, false, nil
	}
	return dictRec{
		key:    key,
		offset: binary.LittleEndian.Uint64(blockBytes[off+8:]),
		size:   binary.LittleEndian.Uint32(blockBytes[off+16:]),
	}, true, nil
}

// Posting returns the full posting bytes for key via one ranged dictionary read
// and one ranged posting read, or ok=false if key is absent.
func (s *Index) Posting(key uint64) (data []byte, ok bool, err error) {
	rec, ok, err := s.lookup(key)
	if err != nil || !ok {
		return nil, ok, err
	}
	// rec.size is an untrusted u32 (up to ~4 GiB); bound the posting allocation.
	if uint64(rec.size) > maxReadBytes {
		return nil, false, ErrTruncated
	}
	buf := make([]byte, rec.size)
	if _, err := s.r.ReadAt(buf, int64(rec.offset)); err != nil {
		return nil, false, err
	}
	return buf, true, nil
}

// NgramCount returns the number of n-grams in the dictionary.
func (s *Index) NgramCount() int { return s.ngrams }
