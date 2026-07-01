package roaringrange

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"testing"

	"github.com/RoaringBitmap/roaring/v2"
)

// Mutation fuzzing of the Go reference readers, mirroring the Rust
// src/fuzz_tests.rs harness added in 45fa50f: index bytes are treated as
// untrusted (hostile origin or a corrupt/partial upload), so a crafted header,
// dictionary record, or offset pair must produce an error -- never a panic or an
// unbounded (multi-GB) allocation. See task 057.

// validRRSIndex builds a small, well-formed v3 RRS index with a stride that
// exercises the sparse-index blocks. Postings are portable roaring bitmaps.
func validRRSIndex(tb testing.TB) []byte {
	tb.Helper()
	posting := func(ids ...uint32) []byte {
		b, err := roaring.BitmapOf(ids...).ToBytes()
		if err != nil {
			tb.Fatalf("posting ToBytes: %v", err)
		}
		return b
	}
	entries := []IndexEntry{
		{Key: 10, Posting: posting(1, 2, 3, 70000)},
		{Key: 20, Posting: posting(5, 130000)},
		{Key: 30, Posting: posting(2, 4, 200005)},
		{Key: 40, Posting: posting(1, 5, 9)},
		{Key: 50, Posting: posting(7)},
	}
	var buf bytes.Buffer
	if err := WriteIndex(&buf, 3, 2, entries); err != nil {
		tb.Fatalf("WriteIndex: %v", err)
	}
	return buf.Bytes()
}

// validRRSRStore builds a small, well-formed v1 RRSR record store (index, blob).
func validRRSRStore(tb testing.TB) (idx, bin []byte) {
	tb.Helper()
	var binBuf, idxBuf bytes.Buffer
	recs := [][]byte{[]byte("rec-0"), {}, []byte("rec-2"), []byte("a longer record 3")}
	if err := WriteRecords(&binBuf, &idxBuf, recs); err != nil {
		tb.Fatalf("WriteRecords: %v", err)
	}
	return idxBuf.Bytes(), binBuf.Bytes()
}

// exerciseRRS runs the RRS reader over data, catching any panic. A clean parse
// error is fine; only a panic (or an OOM crash) is a failure.
func exerciseRRS(data []byte) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic in RRS reader: %v", r)
		}
	}()
	idx, oerr := Open(bytes.NewReader(data))
	if oerr != nil {
		return nil
	}
	_ = idx.NgramCount()
	for _, k := range []uint64{0, 1, 10, 20, 30, 40, 50, 12345, ^uint64(0)} {
		_, _, _ = idx.Posting(k)
	}
	return nil
}

// exerciseRRSR runs the record-store reader over (idxData, binData), catching any
// panic.
func exerciseRRSR(idxData, binData []byte) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic in RRSR reader: %v", r)
		}
	}()
	s, oerr := OpenRecordStore(bytes.NewReader(idxData), bytes.NewReader(binData))
	if oerr != nil {
		return nil
	}
	n := s.Len()
	for id := uint32(0); id < n && id < 64; id++ {
		_, _, _ = s.Get(id)
	}
	for _, id := range []uint32{n, n + 1, 1000, ^uint32(0)} {
		_, _, _ = s.Get(id)
	}
	return nil
}

// TestRRSOpenRejectsHostileSparseAllocation crafts a header whose ngrams/stride
// would size the resident sparse index at ~34 GB. Open must reject it rather than
// attempt the allocation.
func TestRRSOpenRejectsHostileSparseAllocation(t *testing.T) {
	h := make([]byte, headerSize)
	copy(h[0:4], Magic)
	binary.LittleEndian.PutUint16(h[4:6], Version)
	binary.LittleEndian.PutUint16(h[6:8], 3)
	binary.LittleEndian.PutUint32(h[8:12], 0xFFFFFFFF) // ngrams
	binary.LittleEndian.PutUint32(h[12:16], 1)         // stride 1 -> sparse == ngrams
	if _, err := Open(bytes.NewReader(h)); err == nil {
		t.Fatal("Open accepted an oversized sparse index; want error")
	}
}

// TestRRSLookupRejectsHostileDictBlock crafts a header whose sparse index is a
// single entry (so Open succeeds) but whose stride sizes a lookup's dict block at
// ~40 GB. The lookup must reject it rather than allocate.
func TestRRSLookupRejectsHostileDictBlock(t *testing.T) {
	const big = uint32(1) << 31
	h := make([]byte, headerSize+8) // header + one sparse key
	copy(h[0:4], Magic)
	binary.LittleEndian.PutUint16(h[4:6], Version)
	binary.LittleEndian.PutUint16(h[6:8], 3)
	binary.LittleEndian.PutUint32(h[8:12], big)  // ngrams
	binary.LittleEndian.PutUint32(h[12:16], big) // stride -> sparseCount == 1
	binary.LittleEndian.PutUint64(h[16:24], 0)   // sparse key[0] = 0
	idx, err := Open(bytes.NewReader(h))
	if err != nil {
		t.Fatalf("Open rejected a one-entry sparse index: %v", err)
	}
	if _, _, err := idx.Posting(5); err == nil {
		t.Fatal("lookup accepted an oversized dict block; want error")
	}
}

// TestRecordStoreGetRejectsHostileSpan crafts an offset pair whose end-start spans
// hundreds of TB. Get must reject it rather than allocate.
func TestRecordStoreGetRejectsHostileSpan(t *testing.T) {
	idx := make([]byte, recordHeaderSize+16)
	copy(idx[0:4], MagicRecord)
	binary.LittleEndian.PutUint16(idx[4:6], VersionRecord)
	binary.LittleEndian.PutUint32(idx[8:12], 1)               // count = 1
	binary.LittleEndian.PutUint64(idx[16:24], 0)              // start
	binary.LittleEndian.PutUint64(idx[24:32], 0xFFFFFFFFFFFF) // end
	s, err := OpenRecordStore(bytes.NewReader(idx), bytes.NewReader(nil))
	if err != nil {
		t.Fatalf("OpenRecordStore: %v", err)
	}
	if _, _, err := s.Get(0); err == nil {
		t.Fatal("Get accepted an oversized record span; want error")
	}
}

// mutations returns deterministic single-mutation variants of base: every length
// truncation, each of the first bytes forced to 0x00 and 0xFF, and 0xFF-filled
// u32/u64 windows at each aligned header offset (count/offset inflation).
func mutations(base []byte) [][]byte {
	var out [][]byte
	for l := 0; l <= len(base); l++ {
		out = append(out, append([]byte(nil), base[:l]...))
	}
	hi := min(len(base), 64)
	for i := range hi {
		for _, v := range []byte{0x00, 0xFF} {
			m := append([]byte(nil), base...)
			m[i] = v
			out = append(out, m)
		}
	}
	for off := 0; off+4 <= len(base) && off < 64; off += 4 {
		m := append([]byte(nil), base...)
		for j := range 4 {
			m[off+j] = 0xFF
		}
		out = append(out, m)
	}
	for off := 0; off+8 <= len(base) && off < 64; off += 4 {
		m := append([]byte(nil), base...)
		for j := range 8 {
			m[off+j] = 0xFF
		}
		out = append(out, m)
	}
	return out
}

// TestRRSReaderMutationsNoPanic mutates a valid index a byte/window at a time and
// asserts the reader never panics (a clean error is the expected outcome).
func TestRRSReaderMutationsNoPanic(t *testing.T) {
	base := validRRSIndex(t)
	for i, m := range mutations(base) {
		if err := exerciseRRS(m); err != nil {
			t.Fatalf("mutation %d: %v", i, err)
		}
	}
}

// TestRRSRReaderMutationsNoPanic mutates a valid record-store index a byte/window
// at a time and asserts the reader never panics.
func TestRRSRReaderMutationsNoPanic(t *testing.T) {
	idx, bin := validRRSRStore(t)
	for i, m := range mutations(idx) {
		if err := exerciseRRSR(m, bin); err != nil {
			t.Fatalf("idx mutation %d: %v", i, err)
		}
	}
}

// FuzzRRSReader is a native fuzz target over the RRS reader parse path.
func FuzzRRSReader(f *testing.F) {
	f.Add(validRRSIndex(f))
	f.Add([]byte(Magic))
	f.Fuzz(func(t *testing.T, data []byte) {
		if err := exerciseRRS(data); err != nil {
			t.Fatal(err)
		}
	})
}

// FuzzRRSRecordStore is a native fuzz target over the record-store index parse
// path (the blob is held fixed; the parse vulnerabilities live in the index).
func FuzzRRSRecordStore(f *testing.F) {
	idx, bin := validRRSRStore(f)
	f.Add(idx)
	f.Add([]byte(MagicRecord))
	f.Fuzz(func(t *testing.T, data []byte) {
		if err := exerciseRRSR(data, bin); err != nil {
			t.Fatal(err)
		}
	})
}
