package roaringrange

// The Go build-side writer (and dictionary-backed reader) for the version-2
// zstd-compressed RRSR record store — the functional mirror of the Rust
// build::write_records_zstd / RecordStore::open_with_dict. Unlike the other Go
// build-side writers this one is NOT byte-for-byte with the Rust output: the
// frame bytes come from klauspost/compress (pure Go) rather than libzstd, so the
// two encoders pick different match/entropy choices. The store FORMAT is
// identical — [tag][payload] frames, version-2 index, shared dictionary — so a
// frame written by either side decodes through the other (proven both directions
// by the cross-language fixtures: Rust ruzstd reads the Go store, this reader
// reads the Rust store). Conformance is round-trip correctness, not golden bytes.
// See rust/src/build.rs and rust/src/records.rs.

import (
	"encoding/binary"
	"io"

	"github.com/klauspost/compress/zstd"
)

// WriteRecordsZstd writes a version-2 (framed) RRSR record store that compresses
// each record against the shared dict, in doc-ID order — the klauspost
// counterpart of WriteRecords. Each record is framed [tag][payload]: the smaller
// of a zstd frame (tag 1) and the raw bytes (tag 0), so a record never grows; a
// zero-length record stays zero-length (no tag), matching the version-1
// convention. The same dict must be shipped to the reader as the *.dict sidecar
// and passed to OpenRecordStoreWithDict. The frame bytes differ from the Rust
// libzstd builder but read back identically through either reader.
func WriteRecordsZstd(bin, idx io.Writer, records [][]byte, dict []byte) error {
	enc, err := zstd.NewWriter(nil,
		zstd.WithEncoderDict(dict),
		zstd.WithEncoderLevel(zstd.SpeedBestCompression),
	)
	if err != nil {
		return err
	}
	defer enc.Close()

	header := make([]byte, recordHeaderSize)
	copy(header[0:4], MagicRecord)
	binary.LittleEndian.PutUint16(header[4:6], versionRecordFramed)
	binary.LittleEndian.PutUint32(header[8:12], uint32(len(records)))
	if _, err := idx.Write(header); err != nil {
		return err
	}
	var off uint64
	var offBuf [8]byte // off[0] = 0
	if _, err := idx.Write(offBuf[:]); err != nil {
		return err
	}
	for _, rec := range records {
		framed := frameRecordZstd(enc, rec)
		if _, err := bin.Write(framed); err != nil {
			return err
		}
		off += uint64(len(framed))
		binary.LittleEndian.PutUint64(offBuf[:], off)
		if _, err := idx.Write(offBuf[:]); err != nil {
			return err
		}
	}
	return nil
}

// frameRecordZstd mirrors the Rust RecordWriter::frame_zstd decision: an empty
// record stays empty (no tag); otherwise the record is compressed and the smaller
// of [tag 1][zstd frame] and [tag 0][raw bytes] is kept (both pay the same 1-byte
// tag, so the payload sizes decide).
func frameRecordZstd(enc *zstd.Encoder, rec []byte) []byte {
	if len(rec) == 0 {
		return nil
	}
	compressed := enc.EncodeAll(rec, nil)
	if len(compressed) < len(rec) {
		return append([]byte{recordTagZstd}, compressed...)
	}
	return append([]byte{recordTagRaw}, rec...)
}

// klauspostDecompressor inflates a version-2 zstd frame against the shared
// dictionary using a klauspost decoder. The decoder is safe for the store's
// serial Get access; it is never Closed because a RecordStore has no teardown
// hook, which is fine for a reference reader (the goroutines it holds are
// released when the process exits).
type klauspostDecompressor struct {
	dec *zstd.Decoder
}

func (d *klauspostDecompressor) decompress(frame []byte) ([]byte, error) {
	return d.dec.DecodeAll(frame, nil)
}

// OpenRecordStoreWithDict boots an RRSR store like OpenRecordStore but attaches a
// dictionary-backed zstd decoder, so version-2 zstd frames (tag 1) inflate at Get
// — the mirror of the Rust RecordStore::open_with_dict. A version-1 (raw) store
// ignores the dictionary. The dict must be the *.dict sidecar the store was built
// against (Rust libzstd or the Go WriteRecordsZstd builder — either reads here).
func OpenRecordStoreWithDict(idx, bin io.ReaderAt, dict []byte) (*RecordStore, error) {
	s, err := OpenRecordStore(idx, bin)
	if err != nil {
		return nil, err
	}
	dec, err := zstd.NewReader(nil, zstd.WithDecoderDicts(dict))
	if err != nil {
		return nil, err
	}
	s.decomp = &klauspostDecompressor{dec: dec}
	return s, nil
}
