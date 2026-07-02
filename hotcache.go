package roaringrange

// The Go build-side writer for the RRHC catalog-hotcache boot accelerator (.rrhc) —
// the mirror of the Rust hotcache_build::write_hotcache, byte-for-byte. Not an index:
// it front-loads the boot regions of a whole composition (trigram + term + facet +
// vector + records + lookup + embedder) so the composition boots in ONE ranged read.
// Small boots are inlined; large ones (e.g. RRVI centroids) are referenced by
// (BootOff, BootLen) in the member's own data file. See rust/src/hotcache.rs.

import (
	"encoding/binary"
	"fmt"
	"io"
)

const (
	hotcacheMagic       = "RRHC"
	hotcacheHeaderLen   = 32
	hotcacheEntryLen    = 40
	hotcacheFlagInlined = 1
)

// MemberTag is a hotcache member's format type (the on-disk u16 tag) — values match
// the Rust hotcache::MemberTag::to_u16.
type MemberTag uint16

const (
	MemberRrs      MemberTag = 1
	MemberRrti     MemberTag = 2
	MemberRrsf     MemberTag = 3
	MemberRrvi     MemberTag = 4
	MemberRrsrIdx  MemberTag = 5
	MemberRrsrBin  MemberTag = 6
	MemberRrsrDict MemberTag = 7
	MemberRril     MemberTag = 8
	MemberRrm2     MemberTag = 9
	MemberRrss     MemberTag = 10
)

// MemberSpec describes one hotcache member: its format tag, the data-file name its
// per-query reads go to, the boot byte-range within that file, and the boot bytes
// themselves (so the writer can inline or measure them). BootLen must equal
// len(BootBytes).
type MemberSpec struct {
	Tag       MemberTag
	DataFile  string
	BootOff   uint64
	BootLen   uint32
	BootBytes []byte
}

// WriteHotcache writes the RRHC bundle for members to dst — byte-for-byte with the
// Rust write_hotcache. A member whose boot fits inlineThreshold is copied into the
// inlined-boot blob (returned free with the single GET); larger boots are referenced
// by (BootOff, BootLen) in the member's own data file.
func WriteHotcache(dst io.Writer, members []MemberSpec, inlineThreshold uint32) error {
	var stringBlob, inlineBlob []byte
	type placement struct {
		nameOff   uint32
		nameLen   uint16
		inlined   bool
		inlineOff uint64
	}
	places := make([]placement, len(members))
	for i, m := range members {
		if uint64(len(m.BootBytes)) != uint64(m.BootLen) {
			return fmt.Errorf("member %q: BootLen %d disagrees with len(BootBytes) %d",
				m.DataFile, m.BootLen, len(m.BootBytes))
		}
		name := []byte(m.DataFile)
		if len(name) > 0xFFFF {
			return fmt.Errorf("data-file name %q exceeds the 16-bit length limit", m.DataFile)
		}
		// nameOff is a u32 into the string blob; reject an overflowing blob rather
		// than silently wrapping the offset.
		if uint64(len(stringBlob)) >= 1<<32 {
			return fmt.Errorf("RRHC string blob exceeds the 32-bit offset limit")
		}
		p := placement{nameOff: uint32(len(stringBlob)), nameLen: uint16(len(name))}
		stringBlob = append(stringBlob, name...)
		p.inlined = m.BootLen <= inlineThreshold
		p.inlineOff = uint64(len(inlineBlob))
		if p.inlined {
			inlineBlob = append(inlineBlob, m.BootBytes...)
		}
		places[i] = p
	}

	header := make([]byte, hotcacheHeaderLen)
	copy(header[0:4], hotcacheMagic)
	binary.LittleEndian.PutUint16(header[4:6], 1) // version
	// header[6:8] flags = 0
	binary.LittleEndian.PutUint32(header[8:12], uint32(len(members)))
	binary.LittleEndian.PutUint32(header[12:16], uint32(len(stringBlob)))
	binary.LittleEndian.PutUint64(header[16:24], uint64(len(inlineBlob)))
	// header[24:32] reserved
	if _, err := dst.Write(header); err != nil {
		return err
	}

	e := make([]byte, hotcacheEntryLen)
	for i, m := range members {
		p := places[i]
		var flags uint16
		var inlineLen uint32
		var inlineOff uint64
		if p.inlined {
			flags = hotcacheFlagInlined
			inlineLen = m.BootLen
			inlineOff = p.inlineOff
		}
		binary.LittleEndian.PutUint16(e[0:2], uint16(m.Tag))
		binary.LittleEndian.PutUint16(e[2:4], flags)
		binary.LittleEndian.PutUint32(e[4:8], p.nameOff)
		binary.LittleEndian.PutUint16(e[8:10], p.nameLen)
		binary.LittleEndian.PutUint16(e[10:12], 0) // pad
		binary.LittleEndian.PutUint64(e[12:20], m.BootOff)
		binary.LittleEndian.PutUint32(e[20:24], m.BootLen)
		binary.LittleEndian.PutUint64(e[24:32], inlineOff)
		binary.LittleEndian.PutUint32(e[32:36], inlineLen)
		binary.LittleEndian.PutUint32(e[36:40], 0) // reserved
		if _, err := dst.Write(e); err != nil {
			return err
		}
	}

	if _, err := dst.Write(stringBlob); err != nil {
		return err
	}
	_, err := dst.Write(inlineBlob)
	return err
}
