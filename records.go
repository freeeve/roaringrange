package roaringrange

import (
	"encoding/binary"
	"fmt"
	"io"
	"slices"
	"sort"
)

const (
	// MagicRecord is the RRSR record-store index magic.
	MagicRecord = "RRSR"
	// VersionRecord is the RRSR format version this package's writer emits
	// (untagged raw records).
	VersionRecord = 1
	// versionRecordFramed is the version-2 store: every record is framed as
	// [1-byte tag][payload] — tag 0 raw, tag 1 a zstd frame against a shared
	// dictionary (the chunked-zstd full-corpus format the Rust builder writes).
	versionRecordFramed = 2
	// recordTagRaw / recordTagZstd are the version-2 frame tags.
	recordTagRaw  = 0
	recordTagZstd = 1
	// recordHeaderSize is the fixed RRSR index header size in bytes:
	// magic(4) + version(2) + reserved(2) + count(4) + reserved2(4).
	recordHeaderSize = 16
	// maxRecordBytes bounds a single record's on-disk byte span (end-start from
	// the offset pair). The pair is untrusted, so a crafted end drives a multi-GB
	// make() before the backing read can fail; records are document metadata, so
	// 64 MiB is orders of magnitude above any legitimate record while bounding the
	// allocation to a recoverable error.
	maxRecordBytes = 64 << 20
	// maxDecompressedRecord bounds a single zstd frame's inflated size, guarding
	// against a decompression bomb (a few bytes inflating to gigabytes). Mirrors
	// the Rust MAX_DECOMPRESSED_RECORD; enforced by the dictionary decoder in
	// records_zstd.go.
	maxDecompressedRecord = 64 << 20
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
	count   uint32 // record total declared in the header
	written uint32 // number of records written so far
}

// NewRecordWriter opens a streaming record store for count records, writing the
// 16-byte RRSR header and the leading off[0]=0 to idx. Push each record with
// Write in ascending doc-ID order.
//
// PERFORMANCE: Write issues one bin write plus one 8-byte idx write per record, so
// pass BUFFERED writers (e.g. bufio.Writer) for bin and idx when they are files or
// sockets — otherwise a full-corpus build pays ~2 syscalls per record. The library
// does not buffer internally (so it never double-buffers a caller that already
// does); flush your buffers after the final Write / Finish.
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
	return &RecordWriter{bin: bin, idx: idx, count: count}, nil
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

// Finish verifies the writer produced exactly the record count declared in the
// header. The header commits to count up front and the offset table is sized for
// count+1 entries, so an under- or over-count leaves the store internally
// inconsistent — a reader Get of a high id would read past the written offsets.
// Call it after the last Write; it writes nothing (the output stays byte-identical
// for a correct writer) and only reports the mismatch. Streaming callers that
// cannot know the total in advance should size the writer to the count they will
// actually emit.
func (w *RecordWriter) Finish() error {
	if w.written != w.count {
		return fmt.Errorf("%w: wrote %d records, header declared %d", ErrTruncated, w.written, w.count)
	}
	return nil
}

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
	return w.Finish()
}

// recordDecompressor inflates a version-2 zstd frame (tag 1) against the shared
// dictionary. It is supplied by OpenRecordStoreWithDict (records_zstd.go) so the
// klauspost dependency stays isolated there — a plain OpenRecordStore leaves it
// nil and a compressed record errors with ErrCompressedRecord.
type recordDecompressor interface {
	decompress(frame []byte) ([]byte, error)
}

// RecordStore is a reference reader over an RRSR record store accessed by byte
// range: an offset index (idx) over a record blob (bin). It mirrors the browser
// reader's access pattern — one ranged read of the 16-byte offset pair in the
// index, one ranged read of the record slice in the blob. See RECORDS.md.
type RecordStore struct {
	idx     io.ReaderAt
	bin     io.ReaderAt
	count   uint32
	version uint16
	decomp  recordDecompressor // nil unless opened via OpenRecordStoreWithDict
}

// OpenRecordStore boots the store: reads the 16-byte index header and validates
// magic and version. idx addresses the offset index, bin the record blob.
// Accepts version 1 (untagged raw records) and version 2 ([tag][payload]-framed
// records, matching the Rust reader); a version-2 zstd-compressed frame (tag 1)
// errors at Get — open with OpenRecordStoreWithDict (records_zstd.go) to attach a
// dictionary-backed zstd decoder.
func OpenRecordStore(idx, bin io.ReaderAt) (*RecordStore, error) {
	header := make([]byte, recordHeaderSize)
	if _, err := idx.ReadAt(header, 0); err != nil {
		return nil, err
	}
	if string(header[0:4]) != MagicRecord {
		return nil, ErrMagic
	}
	version := binary.LittleEndian.Uint16(header[4:6])
	if version != VersionRecord && version != versionRecordFramed {
		return nil, ErrVersion
	}
	return &RecordStore{
		idx:     idx,
		bin:     bin,
		count:   binary.LittleEndian.Uint32(header[8:12]),
		version: version,
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
	// The offset pair is untrusted: bound the record span before allocating, so a
	// crafted end can't drive a multi-GB make() ahead of the backing read.
	span := end - start
	if span > maxRecordBytes {
		return nil, false, ErrTruncated
	}
	buf := make([]byte, span)
	if len(buf) > 0 {
		if _, err := s.bin.ReadAt(buf, int64(start)); err != nil {
			return nil, false, err
		}
	}
	return s.decode(buf)
}

// recordCoalesceGap is the largest gap (bytes) GetMany bridges between two nearby
// ranged reads: over-reading at most this much beats another round trip. Matches
// the Rust reader's 16 KiB coalescer.
const recordCoalesceGap int64 = 16 << 10

// byteRange is a ranged read request tagged with the caller's result index, so a
// coalesced batch can slice its merged reads back out in the original order.
type byteRange struct {
	idx  int
	off  int64
	size int
}

// readCoalesced reads the given ranges from r, merging ranges whose gap is
// <= gap bytes into a single ReadAt and copying the results back out aligned with
// each range's idx. Every merged span is bounded before allocating, so a crafted
// offset pair degrades to an error rather than a multi-GB make(). Ranges need not
// be sorted. Mirrors the Rust read_coalesced.
func readCoalesced(r io.ReaderAt, ranges []byteRange, gap int64) ([][]byte, error) {
	out := make([][]byte, len(ranges))
	if len(ranges) == 0 {
		return out, nil
	}
	order := make([]int, len(ranges))
	for i := range order {
		order[i] = i
	}
	sort.Slice(order, func(a, b int) bool { return ranges[order[a]].off < ranges[order[b]].off })
	for i := 0; i < len(order); {
		first := ranges[order[i]]
		spanStart := first.off
		spanEnd := first.off + int64(first.size)
		j := i + 1
		for j < len(order) {
			rr := ranges[order[j]]
			if rr.off > spanEnd+gap {
				break
			}
			if e := rr.off + int64(rr.size); e > spanEnd {
				spanEnd = e
			}
			j++
		}
		spanLen := spanEnd - spanStart
		if spanLen < 0 || spanLen > maxReadBytes {
			return nil, ErrTruncated
		}
		buf := make([]byte, spanLen)
		if spanLen > 0 {
			if _, err := r.ReadAt(buf, spanStart); err != nil {
				return nil, err
			}
		}
		for k := i; k < j; k++ {
			rr := ranges[order[k]]
			rel := int(rr.off - spanStart)
			seg := make([]byte, rr.size)
			copy(seg, buf[rel:rel+rr.size])
			out[rr.idx] = seg
		}
		i = j
	}
	return out, nil
}

// GetMany returns the decoded record bytes for each in-range id, keyed by id. Doc
// IDs are rank-ordered by construction, so a top-k page's ids are frequently
// near-contiguous: GetMany reads the offset-table entries in coalesced waves and
// then the record blobs in coalesced waves (bridging gaps <= recordCoalesceGap),
// so ~20 near-contiguous records cost a handful of ranged reads rather than ~2 per
// id. Out-of-range ids are omitted; a zero-length record maps to empty bytes.
// Results are identical to calling Get on each id.
func (s *RecordStore) GetMany(ids []uint32) (map[uint32][]byte, error) {
	out := make(map[uint32][]byte, len(ids))
	if len(ids) == 0 {
		return out, nil
	}
	// Dedup + sort the in-range ids ascending (== rank order); out-of-range omitted.
	seen := make(map[uint32]struct{}, len(ids))
	sorted := make([]uint32, 0, len(ids))
	for _, id := range ids {
		if id >= s.count {
			continue
		}
		if _, dup := seen[id]; dup {
			continue
		}
		seen[id] = struct{}{}
		sorted = append(sorted, id)
	}
	if len(sorted) == 0 {
		return out, nil
	}
	slices.Sort(sorted)

	// Wave 1: the 16-byte offset pair per id, coalesced over the offset table
	// (consecutive ids' pairs overlap, so a run of them is one read).
	pairRanges := make([]byteRange, len(sorted))
	for i, id := range sorted {
		pairRanges[i] = byteRange{idx: i, off: int64(recordHeaderSize) + int64(id)*8, size: 16}
	}
	pairs, err := readCoalesced(s.idx, pairRanges, recordCoalesceGap)
	if err != nil {
		return nil, err
	}

	// Wave 2: each record's blob, coalesced (adjacent blobs within the gap merge).
	blobRanges := make([]byteRange, len(sorted))
	for i := range sorted {
		start := binary.LittleEndian.Uint64(pairs[i][0:8])
		end := binary.LittleEndian.Uint64(pairs[i][8:16])
		if end < start {
			return nil, ErrTruncated
		}
		// The offset pair is untrusted: bound the record span before allocating.
		span := end - start
		if span > maxRecordBytes {
			return nil, ErrTruncated
		}
		blobRanges[i] = byteRange{idx: i, off: int64(start), size: int(span)}
	}
	blobs, err := readCoalesced(s.bin, blobRanges, recordCoalesceGap)
	if err != nil {
		return nil, err
	}
	for i, id := range sorted {
		data, ok, err := s.decode(blobs[i])
		if err != nil {
			return nil, err
		}
		if ok {
			out[id] = data
		}
	}
	return out, nil
}

// decode unwraps a version-2 [tag][payload] frame; a version-1 store (and the
// empty record) passes through raw, mirroring the Rust reader's decode. A zstd
// frame (tag 1) inflates through the dictionary-backed decompressor attached by
// OpenRecordStoreWithDict, or errors with ErrCompressedRecord when none is set.
func (s *RecordStore) decode(raw []byte) (data []byte, ok bool, err error) {
	if s.version == VersionRecord || len(raw) == 0 {
		return raw, true, nil
	}
	switch raw[0] {
	case recordTagRaw:
		return raw[1:], true, nil
	case recordTagZstd:
		if s.decomp == nil {
			return nil, false, ErrCompressedRecord
		}
		out, derr := s.decomp.decompress(raw[1:])
		if derr != nil {
			return nil, false, derr
		}
		return out, true, nil
	default:
		return nil, false, ErrTruncated
	}
}
