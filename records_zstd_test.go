package roaringrange

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

// loadFixtureCorpus reads the length-prefixed records_corpus.bin shared with the
// Rust gen_records_zstd_fixture example: u32 count, then count×(u32 len + bytes).
func loadFixtureCorpus(t *testing.T) [][]byte {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join("testdata", "records_corpus.bin"))
	if err != nil {
		t.Fatalf("read corpus fixture: %v", err)
	}
	if len(raw) < 4 {
		t.Fatal("corpus fixture too short")
	}
	n := binary.LittleEndian.Uint32(raw[0:4])
	off := 4
	recs := make([][]byte, 0, n)
	for range n {
		ln := int(binary.LittleEndian.Uint32(raw[off:]))
		off += 4
		recs = append(recs, append([]byte(nil), raw[off:off+ln]...))
		off += ln
	}
	return recs
}

func loadFixtureDict(t *testing.T) []byte {
	t.Helper()
	d, err := os.ReadFile(filepath.Join("testdata", "records.dict"))
	if err != nil {
		t.Fatalf("read dict fixture: %v", err)
	}
	return d
}

// assertRoundTrip opens the store with the dictionary and asserts every record
// decodes back to the original corpus.
func assertRoundTrip(t *testing.T, idx, bin, dict []byte, want [][]byte) {
	t.Helper()
	store, err := OpenRecordStoreWithDict(bytes.NewReader(idx), bytes.NewReader(bin), dict)
	if err != nil {
		t.Fatalf("OpenRecordStoreWithDict: %v", err)
	}
	if int(store.Len()) != len(want) {
		t.Fatalf("store length = %d, want %d", store.Len(), len(want))
	}
	for d, rec := range want {
		got, ok, err := store.Get(uint32(d))
		if err != nil || !ok {
			t.Fatalf("Get(%d): ok=%v err=%v", d, ok, err)
		}
		if !bytes.Equal(got, rec) {
			t.Errorf("record %d round-trip mismatch:\n got %q\nwant %q", d, got, rec)
		}
	}
}

// TestWriteRecordsZstdRoundTrip builds the fixture corpus with the klauspost
// WriteRecordsZstd, then reads it back through OpenRecordStoreWithDict and asserts
// every record (including the zero-length and the tiny raw-kept one) survives the
// round trip. Also asserts the store is version-2 and actually exercises the zstd
// path (at least one record framed as tag 1). With RR_UPDATE_FIXTURES=1 it
// (re)writes the committed Go-built store that the Rust ruzstd cross-language test
// reads.
func TestWriteRecordsZstdRoundTrip(t *testing.T) {
	recs := loadFixtureCorpus(t)
	dict := loadFixtureDict(t)

	var bin, idx bytes.Buffer
	if err := WriteRecordsZstd(&bin, &idx, recs, dict); err != nil {
		t.Fatalf("WriteRecordsZstd: %v", err)
	}
	if got := binary.LittleEndian.Uint16(idx.Bytes()[4:6]); got != versionRecordFramed {
		t.Fatalf("idx version = %d, want %d", got, versionRecordFramed)
	}
	assertHasZstdFrame(t, idx.Bytes(), bin.Bytes(), recs)
	assertRoundTrip(t, idx.Bytes(), bin.Bytes(), dict, recs)

	if os.Getenv("RR_UPDATE_FIXTURES") == "1" {
		if err := os.WriteFile(filepath.Join("testdata", "records_go_zstd.bin"), bin.Bytes(), 0o644); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join("testdata", "records_go_zstd.idx"), idx.Bytes(), 0o644); err != nil {
			t.Fatal(err)
		}
		t.Logf("wrote Go-built store fixture (%d-byte bin)", bin.Len())
	}
}

// TestOpenRecordStoreWithDictReadsRustStore proves the cross-encoder direction the
// other way: the Go reader inflates the store built by the native libzstd
// write_records_zstd (committed by gen_records_zstd_fixture).
func TestOpenRecordStoreWithDictReadsRustStore(t *testing.T) {
	recs := loadFixtureCorpus(t)
	dict := loadFixtureDict(t)
	idx, err := os.ReadFile(filepath.Join("testdata", "records_rust_zstd.idx"))
	if err != nil {
		t.Fatalf("read rust store idx: %v", err)
	}
	bin, err := os.ReadFile(filepath.Join("testdata", "records_rust_zstd.bin"))
	if err != nil {
		t.Fatalf("read rust store bin: %v", err)
	}
	assertRoundTrip(t, idx, bin, dict, recs)
}

// TestOpenRecordStoreCompressedWithoutDict checks a tag-1 record errors cleanly
// (never panics) when the store is opened without a dictionary.
func TestOpenRecordStoreCompressedWithoutDict(t *testing.T) {
	recs := loadFixtureCorpus(t)
	dict := loadFixtureDict(t)
	var bin, idx bytes.Buffer
	if err := WriteRecordsZstd(&bin, &idx, recs, dict); err != nil {
		t.Fatal(err)
	}
	store, err := OpenRecordStore(bytes.NewReader(idx.Bytes()), bytes.NewReader(bin.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	// Record 0 is non-trivial JSON, so it was compressed (tag 1).
	if _, _, err := store.Get(0); err != ErrCompressedRecord {
		t.Errorf("Get(0) without dict err = %v, want %v", err, ErrCompressedRecord)
	}
}

// assertHasZstdFrame walks the index and fails if no record was stored as a zstd
// frame (tag 1) — otherwise the corpus/dict would not exercise the compressed
// path the test means to cover.
func assertHasZstdFrame(t *testing.T, idx, bin []byte, recs [][]byte) {
	t.Helper()
	for d := range recs {
		base := recordHeaderSize + d*8
		start := binary.LittleEndian.Uint64(idx[base:])
		end := binary.LittleEndian.Uint64(idx[base+8:])
		if end > start && bin[start] == recordTagZstd {
			return
		}
	}
	t.Fatal("no record stored as a zstd frame; corpus/dict do not exercise the compressed path")
}
