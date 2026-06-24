package roaringrange

// The Go build-side writer for the RRIL identifier exact-match index (.rril) — the
// mirror of the Rust build::write_lookup / write_lookup_streaming, byte-for-byte. It
// maps a normalized identifier (ISBN / ASIN / …) to the doc ID(s) carrying it: the
// identifier is FNV-hashed into a sorted [hash u64, verify u32, doc u32] table the
// reader range-binary-searches. The verify (second) hash makes the effective key 96
// bits. The build MUST reproduce the Rust normalize_id + fnv64a_basis exactly. See
// rust/src/lookup.rs (reader) and rust/src/build.rs (writer).

import (
	"encoding/binary"
	"io"
	"sort"
	"strings"
)

const (
	lookupMagic = "RRIL"
	// FNV-1a 64-bit offset basis (the primary hash) — matches lookup::FNV_OFFSET.
	lookupFNVOffset uint64 = 14695981039346656037
	// FNV-1a 64-bit prime — matches lookup::FNV_PRIME.
	lookupFNVPrime uint64 = 1099511628211
	// Distinct nonzero basis for the independent verify hash (golden-ratio
	// constant) — matches lookup::FNV_VERIFY_BASIS.
	lookupFNVVerify uint64 = 0x9E3779B97F4A7C15
)

// normalizeID keeps only ASCII letters/digits, uppercasing letters — byte-for-byte
// with the Rust lookup::normalize_id (which iterates s.bytes()).
func normalizeID(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z'):
			b.WriteByte(c)
		case c >= 'a' && c <= 'z':
			b.WriteByte(c - 32)
		}
	}
	return b.String()
}

// idFNV64a is FNV-1a over the bytes of s with the given offset basis (wrapping
// multiply) — byte-for-byte with the Rust lookup::fnv64a_basis.
func idFNV64a(s string, basis uint64) uint64 {
	h := basis
	for i := 0; i < len(s); i++ {
		h ^= uint64(s[i])
		h *= lookupFNVPrime // uint64 wraps in Go, matching wrapping_mul
	}
	return h
}

// LookupEntry is one (identifier, doc-ID) pair fed to WriteLookup.
type LookupEntry struct {
	ID  string
	Doc uint32
}

// WriteLookup writes the RRIL index over entries to dst — byte-for-byte with the Rust
// write_lookup. Identifiers are normalized and double-hashed, empties dropped, and
// the records sorted by (hash, doc) with a stable sort (matching Rust's stable
// sort_by on equal-hash, equal-doc ties).
func WriteLookup(dst io.Writer, entries []LookupEntry) error {
	type rec struct {
		hash   uint64
		verify uint32
		doc    uint32
	}
	recs := make([]rec, 0, len(entries))
	for _, e := range entries {
		n := normalizeID(e.ID)
		if n == "" {
			continue
		}
		recs = append(recs, rec{
			hash:   idFNV64a(n, lookupFNVOffset),
			verify: uint32(idFNV64a(n, lookupFNVVerify)),
			doc:    e.Doc,
		})
	}
	sort.SliceStable(recs, func(i, j int) bool {
		if recs[i].hash != recs[j].hash {
			return recs[i].hash < recs[j].hash
		}
		return recs[i].doc < recs[j].doc
	})

	header := make([]byte, 16)
	copy(header[0:4], lookupMagic)
	binary.LittleEndian.PutUint16(header[4:6], 1) // version
	// header[6:8] reserved
	binary.LittleEndian.PutUint32(header[8:12], uint32(len(recs))) // count
	// header[12:16] reserved
	if _, err := dst.Write(header); err != nil {
		return err
	}
	rb := make([]byte, 16)
	for _, r := range recs {
		binary.LittleEndian.PutUint64(rb[0:8], r.hash)
		binary.LittleEndian.PutUint32(rb[8:12], r.verify)
		binary.LittleEndian.PutUint32(rb[12:16], r.doc)
		if _, err := dst.Write(rb); err != nil {
			return err
		}
	}
	return nil
}
