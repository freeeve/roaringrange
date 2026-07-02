package roaringrange

import (
	"encoding/binary"
	"io"
	"sort"

	"github.com/RoaringBitmap/roaring/v2"
)

const (
	// Magic is the RRS index magic.
	Magic = "RRSI"
	// Version is the RRS format version number.
	Version = 3
	// headerSize is the fixed RRS header size in bytes (v3): magic[4] + version[2] +
	// gramSize[2] + ngrams[4] + stride[4]. v2's trailing headBoundary[4] is gone.
	headerSize = 16
	// VersionV4 is the RRS format version emitted only for a case-sensitive index: identical
	// to v3 plus a trailing flags u16 at offset 16. Default (case-folding) builds stay v3.
	VersionV4 = 4
	// headerSizeV4 is the v4 header size (v3 header + a 2-byte flags field at offset 16).
	headerSizeV4 = 18
	// rrsiFlagCaseSensitive is the v4 flags bit 0: n-gram keys were not lowercased, so a query
	// skips lowercasing too. Mirrors the Rust index::RRSI_FLAG_CASE_SENSITIVE.
	rrsiFlagCaseSensitive = 1
	// dictEntry is the size of one v3 RRS dictionary entry in bytes:
	// key(8) + offset(8) + size(4).
	dictEntry = 20
	// DefaultStride is the default sparse-index stride.
	DefaultStride = 512
	// headLimit is the exclusive upper bound of the head doc-ID range used by the
	// RRSF facet sidecar (still head/tail in v3); the RRS index itself is one posting.
	headLimit = 65536
)

// indexEntry is one v3 RRS dictionary entry: a key and its single serialized posting.
type indexEntry struct {
	key     uint64
	posting []byte
}

// Transcode reads a roaringsearch FTSR index from src and writes the v3 RRS
// range-fetchable layout to dst using the default stride. Each term is written as
// one portable RoaringBitmap posting. Entries are sorted by key ascending. See FORMAT.md.
func Transcode(src io.Reader, dst io.Writer) error {
	return TranscodeStride(src, dst, DefaultStride)
}

// TranscodeStride is Transcode with an explicit sparse-index stride. A
// stride of zero or less is replaced by DefaultStride.
func TranscodeStride(src io.Reader, dst io.Writer, stride int) error {
	if stride <= 0 {
		stride = DefaultStride
	}
	data, err := io.ReadAll(src)
	if err != nil {
		return err
	}
	if len(data) < 12 || string(data[0:4]) != srcMagic {
		return ErrSrcMagic
	}
	gramSize := binary.LittleEndian.Uint16(data[6:8])
	count := binary.LittleEndian.Uint32(data[8:12])
	// Each FTSR entry is at least 12 bytes (key + size), so a valid index can
	// hold no more entries than its byte length allows. Reject a corrupt count
	// before it drives a multi-GB pre-allocation in parseSplitEntries.
	if uint64(count) > uint64(len(data)-12)/12 {
		return ErrTruncated
	}

	entries, err := parseEntries(data, count)
	if err != nil {
		return err
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].key < entries[j].key })

	return writeIndex(dst, gramSize, stride, entries)
}

// parseEntries walks the FTSR entries, deserializing each portable roaring bitmap and
// re-serializing it as one canonical v3 RRS posting (no head/tail split).
func parseEntries(data []byte, count uint32) ([]indexEntry, error) {
	entries := make([]indexEntry, 0, count)
	pos := 12
	for range count {
		if pos+12 > len(data) {
			return nil, ErrTruncated
		}
		key := binary.LittleEndian.Uint64(data[pos : pos+8])
		size := int(binary.LittleEndian.Uint32(data[pos+8 : pos+12]))
		pos += 12
		if pos+size > len(data) {
			return nil, ErrTruncated
		}
		posting, err := serializePosting(data[pos : pos+size])
		if err != nil {
			return nil, err
		}
		entries = append(entries, indexEntry{key: key, posting: posting})
		pos += size
	}
	return entries, nil
}

// serializePosting deserializes a portable roaring bitmap and re-serializes it as one
// canonical portable RoaringBitmap — the v3 RRS posting (mirrors Rust build::serialize_posting).
//
// FromBuffer keeps a (copy-on-write) reference into payload rather than copying it,
// so payload must outlive bm. It does here: bm is only read (ToBytes) and discarded
// before returning, while payload is a slice of the caller's live index bytes and is
// never mutated — so the defensive copy the buffer contract would otherwise require
// is unnecessary, saving a posting-sized allocation per posting across a large
// transcode. Do not retain bm or mutate it without cloning its containers first.
func serializePosting(payload []byte) ([]byte, error) {
	bm := roaring.New()
	if _, err := bm.FromBuffer(payload); err != nil {
		return nil, err
	}
	return bm.ToBytes()
}

// splitBitmap returns the head (docs [0,65536)) and tail (docs [65536, MaxUint32])
// of bm as independently-deserializable portable RoaringBitmaps.
func splitBitmap(bm *roaring.Bitmap) (head, tail []byte, err error) {
	return splitBitmapHB(bm, headLimit)
}

// splitBitmapHB is splitBitmap with an explicit head/tail boundary (a multiple of
// 65536). Used by the split-set builder, which mirrors the Rust split_posting.
func splitBitmapHB(bm *roaring.Bitmap, headBoundary uint32) (head, tail []byte, err error) {
	headBM := roaring.New()
	headBM.AddRange(0, uint64(headBoundary))
	headBM.And(bm)
	// Tail = the posting minus its head. Clone-and-remove avoids materializing a
	// ~500 MB full-range mask (AddRange to MaxUint32) for every posting.
	tailBM := bm.Clone()
	tailBM.RemoveRange(0, uint64(headBoundary))

	headBytes, err := headBM.ToBytes()
	if err != nil {
		return nil, nil, err
	}
	tailBytes, err := tailBM.ToBytes()
	if err != nil {
		return nil, nil, err
	}
	return headBytes, tailBytes, nil
}

// writeIndex emits the header, sparse index, dictionary, and postings for the
// given key-sorted entries, using the default head boundary (65536).
// writeIndex emits the v3 RRS header, sparse index, dictionary, and postings (one bitmap per
// term) for the given key-sorted entries — the build-side mirror of the Rust build::write_index.
func writeIndex(dst io.Writer, gramSize uint16, stride int, entries []indexEntry) error {
	return writeIndexWith(dst, gramSize, stride, entries, true)
}

// writeIndexWith is writeIndex with an explicit caseNormalization flag. true (the default)
// emits a v3 header byte-identical to before; false emits a v4 header with a trailing flags
// field marking the index case-sensitive. The caller must key entries with the matching case
// mode (NgramKeysWith). Mirrors the Rust build::write_index_with.
func writeIndexWith(dst io.Writer, gramSize uint16, stride int, entries []indexEntry, caseNormalization bool) error {
	n := len(entries)
	sparseCount := (n + stride - 1) / stride
	hdrSize := headerSize
	if !caseNormalization {
		hdrSize = headerSizeV4
	}
	dictStart := hdrSize + sparseCount*8
	postingsStart := dictStart + n*dictEntry

	header := make([]byte, hdrSize)
	copy(header[0:4], Magic)
	version := Version
	if !caseNormalization {
		version = VersionV4
	}
	binary.LittleEndian.PutUint16(header[4:6], uint16(version))
	binary.LittleEndian.PutUint16(header[6:8], gramSize)
	binary.LittleEndian.PutUint32(header[8:12], uint32(n))
	binary.LittleEndian.PutUint32(header[12:16], uint32(stride))
	if !caseNormalization {
		binary.LittleEndian.PutUint16(header[16:18], rrsiFlagCaseSensitive)
	}
	if _, err := dst.Write(header); err != nil {
		return err
	}

	sparse := make([]byte, sparseCount*8)
	for i := range sparseCount {
		binary.LittleEndian.PutUint64(sparse[i*8:], entries[i*stride].key)
	}
	if _, err := dst.Write(sparse); err != nil {
		return err
	}

	dict := make([]byte, n*dictEntry)
	off := postingsStart
	for i, e := range entries {
		b := dict[i*dictEntry:]
		binary.LittleEndian.PutUint64(b[0:8], e.key)
		binary.LittleEndian.PutUint64(b[8:16], uint64(off))
		binary.LittleEndian.PutUint32(b[16:20], uint32(len(e.posting)))
		off += len(e.posting)
	}
	if _, err := dst.Write(dict); err != nil {
		return err
	}

	for _, e := range entries {
		if _, err := dst.Write(e.posting); err != nil {
			return err
		}
	}
	return nil
}
