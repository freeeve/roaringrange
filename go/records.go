package roaringrange

import (
	"encoding/binary"
	"io"
)

const (
	// MagicRecord is the RRSR record-store index magic.
	MagicRecord = "RRSR"
	// VersionRecord is the RRSR format version number.
	VersionRecord = 1
	// recordHeaderSize is the fixed RRSR index header size in bytes:
	// magic(4) + version(2) + reserved(2) + count(4) + reserved2(4).
	recordHeaderSize = 16
)

// RecordWriter streams the RRSR record store: record bytes are pushed one at a
// time in doc-ID order, so a builder that produces records incrementally never
// has to hold them all in memory. The concatenated record bytes go to bin and a
// range-fetchable offset index to idx. Records are opaque to the library — the
// caller chooses the encoding (JSON, msgpack, …); the store only frames them for
// O(1) Range lookup by doc ID. Mirrors the Rust RecordWriter. See RECORDS.md.
//
// count is written into the header up front, so the caller must know the record
// total in advance and call Write exactly that many times (the offset table is
// sized for count+1 entries).
type RecordWriter struct {
	bin     io.Writer
	idx     io.Writer
	off     uint64 // cumulative end offset into bin (== bytes written so far)
	written uint32 // number of records written so far
}

// NewRecordWriter opens a streaming record store for count records, writing the
// 16-byte RRSR header and the leading off[0]=0 to idx. Push each record with
// Write in ascending doc-ID order.
func NewRecordWriter(bin, idx io.Writer, count uint32) (*RecordWriter, error) {
	header := make([]byte, recordHeaderSize)
	copy(header[0:4], MagicRecord)
	binary.LittleEndian.PutUint16(header[4:6], VersionRecord)
	binary.LittleEndian.PutUint32(header[8:12], count)
	if _, err := idx.Write(header); err != nil {
		return nil, err
	}
	var zero [8]byte // off[0] = 0
	if _, err := idx.Write(zero[:]); err != nil {
		return nil, err
	}
	return &RecordWriter{bin: bin, idx: idx}, nil
}

// Write appends one record's bytes to the blob and its cumulative end offset to
// the index. A zero-length record (a doc with no stored fields) stays
// addressable.
func (w *RecordWriter) Write(rec []byte) error {
	if _, err := w.bin.Write(rec); err != nil {
		return err
	}
	w.off += uint64(len(rec))
	var off [8]byte
	binary.LittleEndian.PutUint64(off[:], w.off)
	if _, err := w.idx.Write(off[:]); err != nil {
		return err
	}
	w.written++
	return nil
}

// Written returns the number of records written so far.
func (w *RecordWriter) Written() uint32 { return w.written }

// WriteRecords writes a record store from an in-memory slice of records, in
// doc-ID order — a convenience over RecordWriter for callers that already hold
// every record. See RECORDS.md.
func WriteRecords(bin, idx io.Writer, records [][]byte) error {
	w, err := NewRecordWriter(bin, idx, uint32(len(records)))
	if err != nil {
		return err
	}
	for _, rec := range records {
		if err := w.Write(rec); err != nil {
			return err
		}
	}
	return nil
}

// RecordStore is a reference reader over an RRSR record store accessed by byte
// range: an offset index (idx) over a record blob (bin). It mirrors the browser
// reader's access pattern — one ranged read of the 16-byte offset pair in the
// index, one ranged read of the record slice in the blob. See RECORDS.md.
type RecordStore struct {
	idx   io.ReaderAt
	bin   io.ReaderAt
	count uint32
}

// OpenRecordStore boots the store: reads the 16-byte index header and validates
// magic and version. idx addresses the offset index, bin the record blob.
func OpenRecordStore(idx, bin io.ReaderAt) (*RecordStore, error) {
	header := make([]byte, recordHeaderSize)
	if _, err := idx.ReadAt(header, 0); err != nil {
		return nil, err
	}
	if string(header[0:4]) != MagicRecord {
		return nil, ErrMagic
	}
	if binary.LittleEndian.Uint16(header[4:6]) != VersionRecord {
		return nil, ErrTruncated
	}
	return &RecordStore{
		idx:   idx,
		bin:   bin,
		count: binary.LittleEndian.Uint32(header[8:12]),
	}, nil
}

// Len returns the number of records (doc IDs 0..Len).
func (s *RecordStore) Len() uint32 { return s.count }

// Get returns the raw record bytes for doc id via one ranged index read (the
// 16-byte offset pair) and one ranged blob read, or ok=false if id is out of
// range. A zero-length record (a doc with no stored fields) returns ok=true with
// empty bytes.
func (s *RecordStore) Get(id uint32) (data []byte, ok bool, err error) {
	if id >= s.count {
		return nil, false, nil
	}
	pair := make([]byte, 16)
	if _, err := s.idx.ReadAt(pair, int64(recordHeaderSize)+int64(id)*8); err != nil {
		return nil, false, err
	}
	start := binary.LittleEndian.Uint64(pair[0:8])
	end := binary.LittleEndian.Uint64(pair[8:16])
	if end < start {
		return nil, false, ErrTruncated
	}
	buf := make([]byte, end-start)
	if len(buf) > 0 {
		if _, err := s.bin.ReadAt(buf, int64(start)); err != nil {
			return nil, false, err
		}
	}
	return buf, true, nil
}
