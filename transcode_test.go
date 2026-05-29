package roaringrange

import (
	"bytes"
	"encoding/binary"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// makeBitmap builds a portable roaring bitmap from the given values and returns
// its serialized bytes for embedding in a synthetic FTSR index.
func makeBitmap(t *testing.T, values ...uint32) []byte {
	t.Helper()
	bm := roaring.New()
	for _, v := range values {
		bm.Add(v)
	}
	b, err := bm.ToBytes()
	if err != nil {
		t.Fatalf("ToBytes: %v", err)
	}
	return b
}

// splitValues partitions values into head (<65536) and tail (>=65536) sets.
func splitValues(values []uint32) (head, tail []uint32) {
	for _, v := range values {
		if v < headLimit {
			head = append(head, v)
		} else {
			tail = append(tail, v)
		}
	}
	return head, tail
}

// bitmapValues deserializes a portable roaring bitmap and returns its members.
func bitmapValues(t *testing.T, data []byte) []uint32 {
	t.Helper()
	bm := roaring.New()
	if _, err := bm.FromBuffer(append([]byte(nil), data...)); err != nil {
		t.Fatalf("FromBuffer: %v", err)
	}
	return bm.ToArray()
}

// equalU32 reports whether two uint32 slices hold the same elements in order.
func equalU32(a, b []uint32) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// buildFTSR assembles a synthetic roaringsearch FTSR index — the transcode
// input: the "FTSR" magic, a reserved version word, the gram size, the entry
// count, then each entry as key(8) + size(4) + payload. Entry order is
// irrelevant because Transcode sorts by key.
func buildFTSR(gramSize uint16, entries map[uint64][]byte) []byte {
	var b bytes.Buffer
	b.WriteString(srcMagic)
	var u16 [2]byte
	b.Write(u16[:]) // reserved/version word at [4:6]; Transcode does not read it
	binary.LittleEndian.PutUint16(u16[:], gramSize)
	b.Write(u16[:])
	var u32 [4]byte
	binary.LittleEndian.PutUint32(u32[:], uint32(len(entries)))
	b.Write(u32[:])
	for key, payload := range entries {
		var kb [8]byte
		binary.LittleEndian.PutUint64(kb[:], key)
		b.Write(kb[:])
		binary.LittleEndian.PutUint32(u32[:], uint32(len(payload)))
		b.Write(u32[:])
		b.Write(payload)
	}
	return b.Bytes()
}

// TestTranscodeRoundTrip transcodes real roaring bitmaps spanning the head
// and tail ranges to RRS and verifies the header, dictionary ordering, and
// per-key head/tail splits through ranged reads, including absent keys and
// multi-block sparse traversal.
func TestTranscodeRoundTrip(t *testing.T) {
	values := map[uint64][]uint32{
		50: {1, 2, 100, 65535, 65536, 70000, 200000},
		10: {0, 65536},
		30: {5, 6, 7, 99999, 4000000000},
		40: {65535},
		20: {65536, 65537, 1000000},
		60: {3, 4, 5},
	}
	ftsrEntries := make(map[uint64][]byte, len(values))
	for k, vs := range values {
		ftsrEntries[k] = makeBitmap(t, vs...)
	}
	ftsr := buildFTSR(3, ftsrEntries)

	var rrs bytes.Buffer
	if err := TranscodeStride(bytes.NewReader(ftsr), &rrs, 2); err != nil {
		t.Fatalf("transcode: %v", err)
	}

	raw := rrs.Bytes()
	if string(raw[0:4]) != Magic {
		t.Fatalf("magic = %q, want %q", raw[0:4], Magic)
	}
	if v := binary.LittleEndian.Uint16(raw[4:6]); v != Version {
		t.Fatalf("version = %d, want %d", v, Version)
	}
	if g := binary.LittleEndian.Uint16(raw[6:8]); g != 3 {
		t.Fatalf("gramSize = %d, want 3", g)
	}
	if n := binary.LittleEndian.Uint32(raw[8:12]); int(n) != len(values) {
		t.Fatalf("ngrams = %d, want %d", n, len(values))
	}
	if st := binary.LittleEndian.Uint32(raw[12:16]); st != 2 {
		t.Fatalf("stride = %d, want 2", st)
	}

	idx, err := Open(bytes.NewReader(raw))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if idx.GramSize != 3 {
		t.Fatalf("GramSize = %d, want 3", idx.GramSize)
	}
	if idx.NgramCount() != len(values) {
		t.Fatalf("NgramCount = %d, want %d", idx.NgramCount(), len(values))
	}

	// With stride 2 and 6 ngrams there must be multiple sparse blocks.
	if len(idx.sparseKeys) != 3 {
		t.Fatalf("sparseKeys = %d, want 3", len(idx.sparseKeys))
	}
	for i := 1; i < len(idx.sparseKeys); i++ {
		if idx.sparseKeys[i-1] >= idx.sparseKeys[i] {
			t.Fatalf("sparse index not key-sorted at %d", i)
		}
	}

	for key, vs := range values {
		wantHead, wantTail := splitValues(vs)

		headBytes, ok, err := idx.Head(key)
		if err != nil || !ok {
			t.Fatalf("head %d: ok=%v err=%v", key, ok, err)
		}
		if got := bitmapValues(t, headBytes); !equalU32(got, wantHead) {
			t.Fatalf("head %d = %v, want %v", key, got, wantHead)
		}

		tailBytes, ok, err := idx.Tail(key)
		if err != nil || !ok {
			t.Fatalf("tail %d: ok=%v err=%v", key, ok, err)
		}
		if got := bitmapValues(t, tailBytes); !equalU32(got, wantTail) {
			t.Fatalf("tail %d = %v, want %v", key, got, wantTail)
		}
	}

	if _, ok, _ := idx.Head(999); ok {
		t.Fatalf("head 999 should be absent")
	}
	if _, ok, _ := idx.Tail(999); ok {
		t.Fatalf("tail 999 should be absent")
	}
	if _, ok, _ := idx.Head(0); ok {
		t.Fatalf("head 0 (below first key) should be absent")
	}
}

// TestTranscodeDictSorted verifies the on-disk dictionary is key-sorted and
// its absolute head offsets and sizes are internally consistent.
func TestTranscodeDictSorted(t *testing.T) {
	values := map[uint64][]uint32{
		7: {1, 65536}, 3: {2, 70000}, 9: {3}, 1: {65540}, 5: {4, 5},
	}
	ftsrEntries := make(map[uint64][]byte, len(values))
	for k, vs := range values {
		ftsrEntries[k] = makeBitmap(t, vs...)
	}
	var rrs bytes.Buffer
	if err := TranscodeStride(bytes.NewReader(buildFTSR(3, ftsrEntries)), &rrs, 2); err != nil {
		t.Fatalf("transcode: %v", err)
	}

	raw := rrs.Bytes()
	n := int(binary.LittleEndian.Uint32(raw[8:12]))
	stride := int(binary.LittleEndian.Uint32(raw[12:16]))
	sparseCount := (n + stride - 1) / stride
	dictStart := headerSize + sparseCount*8
	postingsStart := dictStart + n*dictEntry

	var prevKey uint64
	expectOff := postingsStart
	for i := 0; i < n; i++ {
		b := raw[dictStart+i*dictEntry:]
		key := binary.LittleEndian.Uint64(b[0:8])
		headOff := binary.LittleEndian.Uint64(b[8:16])
		headSize := binary.LittleEndian.Uint32(b[16:20])
		tailSize := binary.LittleEndian.Uint32(b[20:24])
		if i > 0 && key <= prevKey {
			t.Fatalf("dictionary not key-sorted at %d", i)
		}
		if headOff != uint64(expectOff) {
			t.Fatalf("headOffset[%d] = %d, want %d", i, headOff, expectOff)
		}
		prevKey = key
		expectOff += int(headSize) + int(tailSize)
	}
	if expectOff != len(raw) {
		t.Fatalf("postings end = %d, want file len %d", expectOff, len(raw))
	}
}
