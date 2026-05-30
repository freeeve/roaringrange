package roaringrange

import (
	"bytes"
	"encoding/binary"
	"testing"
)

// sampleRecords mirror the Rust record_store tests: a JSON-ish record, a
// zero-length record (a doc with no stored fields), and a plain-bytes record.
func sampleRecords() [][]byte {
	return [][]byte{
		[]byte(`{"id":"A","c":9}`),
		{},
		[]byte("hello"),
	}
}

// TestWriteRecordsGoldenLayout asserts the exact RECORDS.md byte layout so the
// Go writer stays byte-compatible with the Rust RecordWriter/RecordStore. This
// is the cross-language guard: the offsets and header here are the same the Rust
// reader deserializes.
func TestWriteRecordsGoldenLayout(t *testing.T) {
	recs := sampleRecords()
	var bin, idx bytes.Buffer
	if err := WriteRecords(&bin, &idx, recs); err != nil {
		t.Fatalf("WriteRecords: %v", err)
	}

	ib := idx.Bytes()
	// Header: magic, version, count.
	if got := string(ib[0:4]); got != MagicRecord {
		t.Errorf("magic = %q, want %q", got, MagicRecord)
	}
	if got := binary.LittleEndian.Uint16(ib[4:6]); got != VersionRecord {
		t.Errorf("version = %d, want %d", got, VersionRecord)
	}
	if got := binary.LittleEndian.Uint32(ib[8:12]); got != uint32(len(recs)) {
		t.Errorf("count = %d, want %d", got, len(recs))
	}

	// Offset table: N+1 u64 LE, cumulative. For the sample records the
	// cumulative end offsets are 0,16,16,21 (16-byte JSON, empty, "hello").
	wantOff := []uint64{0, 16, 16, 21}
	if want := recordHeaderSize + (len(recs)+1)*8; len(ib) != want {
		t.Fatalf("idx length = %d, want %d", len(ib), want)
	}
	for d, want := range wantOff {
		got := binary.LittleEndian.Uint64(ib[recordHeaderSize+d*8:])
		if got != want {
			t.Errorf("off[%d] = %d, want %d", d, got, want)
		}
	}

	// bin is the records concatenated in doc-ID order.
	if !bytes.Equal(bin.Bytes(), []byte(`{"id":"A","c":9}hello`)) {
		t.Errorf("bin = %q", bin.Bytes())
	}

	// Each record d is bin[off[d]:off[d+1]].
	for d, rec := range recs {
		o0 := binary.LittleEndian.Uint64(ib[recordHeaderSize+d*8:])
		o1 := binary.LittleEndian.Uint64(ib[recordHeaderSize+(d+1)*8:])
		if got := bin.Bytes()[o0:o1]; !bytes.Equal(got, rec) {
			t.Errorf("record %d via offsets = %q, want %q", d, got, rec)
		}
	}
}

// TestRecordStoreRoundTrip writes a store and reads every record back by doc ID
// through the reference RecordStore, mirroring the Rust round-trip test.
func TestRecordStoreRoundTrip(t *testing.T) {
	recs := sampleRecords()
	var bin, idx bytes.Buffer
	if err := WriteRecords(&bin, &idx, recs); err != nil {
		t.Fatalf("WriteRecords: %v", err)
	}

	store, err := OpenRecordStore(bytes.NewReader(idx.Bytes()), bytes.NewReader(bin.Bytes()))
	if err != nil {
		t.Fatalf("OpenRecordStore: %v", err)
	}
	if store.Len() != uint32(len(recs)) {
		t.Fatalf("Len = %d, want %d", store.Len(), len(recs))
	}

	for d, want := range recs {
		got, ok, err := store.Get(uint32(d))
		if err != nil {
			t.Fatalf("Get(%d): %v", d, err)
		}
		if !ok {
			t.Fatalf("Get(%d): ok=false", d)
		}
		if !bytes.Equal(got, want) {
			t.Errorf("Get(%d) = %q, want %q", d, got, want)
		}
	}

	// Out-of-range doc ID yields ok=false.
	if _, ok, err := store.Get(uint32(len(recs))); err != nil || ok {
		t.Errorf("Get(out of range) = ok=%v err=%v, want ok=false err=nil", ok, err)
	}
}

// TestRecordWriterStreaming exercises the streaming path (push one at a time)
// and confirms it produces the same bytes as WriteRecords.
func TestRecordWriterStreaming(t *testing.T) {
	recs := sampleRecords()

	var binS, idxS bytes.Buffer
	w, err := NewRecordWriter(&binS, &idxS, uint32(len(recs)))
	if err != nil {
		t.Fatalf("NewRecordWriter: %v", err)
	}
	for _, r := range recs {
		if err := w.Write(r); err != nil {
			t.Fatalf("Write: %v", err)
		}
	}
	if w.Written() != uint32(len(recs)) {
		t.Errorf("Written = %d, want %d", w.Written(), len(recs))
	}

	var binB, idxB bytes.Buffer
	if err := WriteRecords(&binB, &idxB, recs); err != nil {
		t.Fatalf("WriteRecords: %v", err)
	}
	if !bytes.Equal(binS.Bytes(), binB.Bytes()) {
		t.Error("streaming bin differs from WriteRecords bin")
	}
	if !bytes.Equal(idxS.Bytes(), idxB.Bytes()) {
		t.Error("streaming idx differs from WriteRecords idx")
	}
}

// TestOpenRecordStoreBadMagic rejects a blob whose index lacks the RRSR magic.
func TestOpenRecordStoreBadMagic(t *testing.T) {
	bad := make([]byte, recordHeaderSize+8)
	copy(bad[0:4], "XXXX")
	if _, err := OpenRecordStore(bytes.NewReader(bad), bytes.NewReader(nil)); err != ErrMagic {
		t.Errorf("OpenRecordStore bad magic err = %v, want %v", err, ErrMagic)
	}
}
