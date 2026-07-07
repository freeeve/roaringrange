package roaringrange

import (
	"encoding/binary"
	"io"
	"sort"
)

// This file is the read-side dispatch layer shared by every RRXX reader: a
// magic->format registry (each reader self-registers in its own init), a bounded
// read helper mirroring the maxReadBytes discipline of reader.go, and a uniform
// FileInfo the CLI `info` command prints without opening a file's body. See the
// per-format Open* / describe* functions in the matching <format>_read.go files.

// Field is one decoded header value, kept in an ordered slice so text output is
// deterministic (a map would randomize key order).
type Field struct {
	Key string
	Val any
}

// FileInfo is the format-agnostic header view produced by a Describe function: the
// magic, human name, extension, version, and the format-specific decoded header
// fields (counts, sizes, and decoded flag bits). It is what `roaringrange info`
// renders, in text or JSON, for any recognized file.
type FileInfo struct {
	Magic   string
	Name    string
	Ext     string
	Version uint16
	Fields  []Field
}

// Format describes one recognized on-disk format: its 4-byte magic, a human name,
// the conventional extension, and a Describe that reads only the fixed header (not
// the body) and returns a FileInfo. Formats self-register via register in init.
type Format struct {
	Magic    string
	Name     string
	Ext      string
	Describe func(io.ReaderAt) (*FileInfo, error)
}

// registry maps a 4-byte magic to its Format. Populated by each reader's init via
// register, so format.go carries no compile-time dependency on the readers and
// they can be added independently.
var registry = map[string]Format{}

// register records a Format under its magic. Called from each reader's init.
func register(f Format) { registry[f.Magic] = f }

// Formats returns the registered formats sorted by magic, for help text and tests.
func Formats() []Format {
	out := make([]Format, 0, len(registry))
	for _, f := range registry {
		out = append(out, f)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Magic < out[j].Magic })
	return out
}

// DetectFormat reads the leading 4-byte magic and returns the matching Format.
// Returns ErrMagic when the magic is not a recognized roaringrange format (this
// includes the sidecar .dict, which has no magic of its own).
func DetectFormat(r io.ReaderAt) (Format, error) {
	var magic [4]byte
	if _, err := r.ReadAt(magic[:], 0); err != nil {
		return Format{}, err
	}
	f, ok := registry[string(magic[:])]
	if !ok {
		return Format{}, ErrMagic
	}
	return f, nil
}

// OpenHeader detects the format and returns its FileInfo, reading only the fixed
// header. It is the entry point for `roaringrange info` and works on every
// registered format without opening postings, vectors, or other bodies.
func OpenHeader(r io.ReaderAt) (*FileInfo, error) {
	f, err := DetectFormat(r)
	if err != nil {
		return nil, err
	}
	return f.Describe(r)
}

// boundedRead reads exactly n bytes at off from r, first checking n against
// maxReadBytes so a crafted or corrupt header length yields ErrTruncated rather
// than a multi-GB make() (an OOM abort) or, on a 32-bit build, a negative length.
// Every reader routes untrusted-length reads through this, mirroring reader.go.
func boundedRead(r io.ReaderAt, off int64, n uint64) ([]byte, error) {
	if n > maxReadBytes {
		return nil, ErrTruncated
	}
	buf := make([]byte, n)
	if _, err := r.ReadAt(buf, off); err != nil {
		return nil, err
	}
	return buf, nil
}

// readHeader reads the fixed-size leading header of a file, validating the magic
// and returning the raw header bytes. size must be the format's header length.
func readHeader(r io.ReaderAt, magic string, size int) ([]byte, error) {
	h := make([]byte, size)
	if _, err := r.ReadAt(h, 0); err != nil {
		return nil, err
	}
	if string(h[0:4]) != magic {
		return nil, ErrMagic
	}
	return h, nil
}

// u16/u32/u64 are little-endian header field readers, aliasing encoding/binary for
// terse call sites in the readers.
func u16(b []byte) uint16 { return binary.LittleEndian.Uint16(b) }
func u32(b []byte) uint32 { return binary.LittleEndian.Uint32(b) }
func u64(b []byte) uint64 { return binary.LittleEndian.Uint64(b) }
