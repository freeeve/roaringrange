package roaringrange

import (
	"encoding/binary"
	"io"
	"sort"
)

// dictRec is one parsed RRS dictionary entry.
type dictRec struct {
	key        uint64
	headOffset uint64
	headSize   uint32
	tailSize   uint32
}

// Index is a reference reader over an RRS index accessed by byte range. It
// reads only the 20-byte header and the sparse index up front (mirroring the
// browser reader's boot ranged GETs); the dictionary and postings are fetched
// lazily, one ranged ReadAt per block, never in their entirety. See FORMAT.md.
type Index struct {
	r io.ReaderAt
	// GramSize is the n-gram window the index was built with.
	GramSize int
	// HeadBoundary is the doc-ID head/tail split point recorded in the header.
	HeadBoundary uint32
	ngrams       int
	stride       int
	dictStart    int64
	sparseKeys   []uint64
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
	gramSize := int(binary.LittleEndian.Uint16(header[6:8]))
	ngrams := int(binary.LittleEndian.Uint32(header[8:12]))
	stride := int(binary.LittleEndian.Uint32(header[12:16]))
	headBoundary := binary.LittleEndian.Uint32(header[16:20])
	if stride <= 0 {
		return nil, ErrTruncated
	}

	sparseCount := (ngrams + stride - 1) / stride
	sparseBytes := make([]byte, sparseCount*8)
	if sparseCount > 0 {
		if _, err := r.ReadAt(sparseBytes, headerSize); err != nil {
			return nil, err
		}
	}
	sparseKeys := make([]uint64, sparseCount)
	for i := range sparseKeys {
		sparseKeys[i] = binary.LittleEndian.Uint64(sparseBytes[i*8:])
	}

	return &Index{
		r:            r,
		GramSize:     gramSize,
		HeadBoundary: headBoundary,
		ngrams:       ngrams,
		stride:       stride,
		dictStart:    int64(headerSize + sparseCount*8),
		sparseKeys:   sparseKeys,
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
	blockBytes := make([]byte, blockLen*dictEntry)
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
		key:        key,
		headOffset: binary.LittleEndian.Uint64(blockBytes[off+8:]),
		headSize:   binary.LittleEndian.Uint32(blockBytes[off+16:]),
		tailSize:   binary.LittleEndian.Uint32(blockBytes[off+20:]),
	}, true, nil
}

// Head returns the head posting bytes (docs [0,65536)) for key via one ranged
// dictionary read and one ranged posting read, or ok=false if key is absent.
func (s *Index) Head(key uint64) (data []byte, ok bool, err error) {
	rec, ok, err := s.lookup(key)
	if err != nil || !ok {
		return nil, ok, err
	}
	buf := make([]byte, rec.headSize)
	if _, err := s.r.ReadAt(buf, int64(rec.headOffset)); err != nil {
		return nil, false, err
	}
	return buf, true, nil
}

// Tail returns the tail posting bytes (docs [65536, MaxUint32]) for key via one
// ranged dictionary read and one ranged posting read, or ok=false if key is
// absent.
func (s *Index) Tail(key uint64) (data []byte, ok bool, err error) {
	rec, ok, err := s.lookup(key)
	if err != nil || !ok {
		return nil, ok, err
	}
	buf := make([]byte, rec.tailSize)
	if _, err := s.r.ReadAt(buf, int64(rec.headOffset+uint64(rec.headSize))); err != nil {
		return nil, false, err
	}
	return buf, true, nil
}

// NgramCount returns the number of n-grams in the dictionary.
func (s *Index) NgramCount() int { return s.ngrams }
