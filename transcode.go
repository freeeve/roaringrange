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
	Version = 1
	// headerSize is the fixed RRS header size in bytes.
	headerSize = 16
	// dictEntry is the size of one RRS dictionary entry in bytes:
	// key(8) + headOffset(8) + headSize(4) + tailSize(4).
	dictEntry = 24
	// DefaultStride is the default sparse-index stride.
	DefaultStride = 512
	// headLimit is the exclusive upper bound of the head doc-ID range; doc IDs
	// below it form the head posting (the first roaring container).
	headLimit = 65536
)

// splitEntry holds a posting that has been split into head and tail bitmaps,
// each re-serialized as a portable RoaringBitmap.
type splitEntry struct {
	key  uint64
	head []byte
	tail []byte
}

// Transcode reads a roaringsearch FTSR index from src and writes the RRS
// range-fetchable layout to dst using the default stride. Each posting is
// split into a head bitmap (docs [0,65536)) and a tail bitmap (docs
// [65536, MaxUint32]); both are re-serialized as portable RoaringBitmaps.
// Entries are sorted by key ascending. See FORMAT.md.
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

	entries, err := parseSplitEntries(data, count)
	if err != nil {
		return err
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].key < entries[j].key })

	return writeIndex(dst, gramSize, stride, entries)
}

// parseSplitEntries walks the FTSR entries, deserializes each portable roaring
// bitmap, and splits it into head and tail bitmaps re-serialized as bytes.
func parseSplitEntries(data []byte, count uint32) ([]splitEntry, error) {
	entries := make([]splitEntry, 0, count)
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
		head, tail, err := splitPosting(data[pos : pos+size])
		if err != nil {
			return nil, err
		}
		entries = append(entries, splitEntry{key: key, head: head, tail: tail})
		pos += size
	}
	return entries, nil
}

// splitPosting deserializes a portable roaring bitmap and returns the head
// (docs [0,65536)) and tail (docs [65536, MaxUint32]) as portable bitmaps.
func splitPosting(payload []byte) (head, tail []byte, err error) {
	bm := roaring.New()
	if _, err := bm.FromBuffer(append([]byte(nil), payload...)); err != nil {
		return nil, nil, err
	}
	return splitBitmap(bm)
}

// splitBitmap returns the head (docs [0,65536)) and tail (docs [65536, MaxUint32])
// of bm as independently-deserializable portable RoaringBitmaps.
func splitBitmap(bm *roaring.Bitmap) (head, tail []byte, err error) {
	headBM := roaring.New()
	headBM.AddRange(0, headLimit)
	headBM.And(bm)
	// Tail = the posting minus its head. Clone-and-remove avoids materializing a
	// ~500 MB full-range mask (AddRange to MaxUint32) for every posting.
	tailBM := bm.Clone()
	tailBM.RemoveRange(0, headLimit)

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
// given key-sorted entries.
func writeIndex(dst io.Writer, gramSize uint16, stride int, entries []splitEntry) error {
	n := len(entries)
	sparseCount := (n + stride - 1) / stride
	dictStart := headerSize + sparseCount*8
	postingsStart := dictStart + n*dictEntry

	header := make([]byte, headerSize)
	copy(header[0:4], Magic)
	binary.LittleEndian.PutUint16(header[4:6], Version)
	binary.LittleEndian.PutUint16(header[6:8], gramSize)
	binary.LittleEndian.PutUint32(header[8:12], uint32(n))
	binary.LittleEndian.PutUint32(header[12:16], uint32(stride))
	if _, err := dst.Write(header); err != nil {
		return err
	}

	sparse := make([]byte, sparseCount*8)
	for i := 0; i < sparseCount; i++ {
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
		binary.LittleEndian.PutUint32(b[16:20], uint32(len(e.head)))
		binary.LittleEndian.PutUint32(b[20:24], uint32(len(e.tail)))
		off += len(e.head) + len(e.tail)
	}
	if _, err := dst.Write(dict); err != nil {
		return err
	}

	for _, e := range entries {
		if _, err := dst.Write(e.head); err != nil {
			return err
		}
		if _, err := dst.Write(e.tail); err != nil {
			return err
		}
	}
	return nil
}
