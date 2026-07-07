package roaringrange

import "io"

// The read side of the RRHC catalog-hotcache bundle (the inverse of WriteHotcache
// in hotcache.go). The manifest is small, so it is parsed whole: the member
// directory, name blob, and inlined-boot blob are all read up front and rehydrated
// into []MemberSpec. Referenced (non-inlined) boots live in the member's own data
// file, so their BootBytes stay nil. Mirrors rust/src/hotcache.rs. See HOTCACHE.md.

func init() {
	register(Format{Magic: hotcacheMagic, Name: "hotcache", Ext: ".rrhc", Describe: describeHotcache})
}

// Hotcache is a reference reader over an RRHC bundle. The whole manifest is
// resident after Open.
type Hotcache struct {
	members []MemberSpec
}

// OpenHotcache reads and validates the RRHC header and rehydrates every MemberSpec,
// inlining the boot bytes for members that were stored inline.
func OpenHotcache(r io.ReaderAt) (*Hotcache, error) {
	h, err := readHeader(r, hotcacheMagic, hotcacheHeaderLen)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	memberCount := int(u32(h[8:12]))
	strLen := u32(h[12:16])
	inlineLen := u64(h[16:24])

	entries, err := boundedRead(r, hotcacheHeaderLen, uint64(memberCount)*hotcacheEntryLen)
	if err != nil {
		return nil, err
	}
	strOff := int64(hotcacheHeaderLen) + int64(memberCount)*hotcacheEntryLen
	blob, err := boundedRead(r, strOff, uint64(strLen))
	if err != nil {
		return nil, err
	}
	inlineBlob, err := boundedRead(r, strOff+int64(strLen), inlineLen)
	if err != nil {
		return nil, err
	}

	members := make([]MemberSpec, memberCount)
	for i := range members {
		e := entries[i*hotcacheEntryLen:]
		flags := u16(e[2:4])
		nameOff := u32(e[4:8])
		nameLen := u16(e[8:10])
		if uint64(nameOff)+uint64(nameLen) > uint64(len(blob)) {
			return nil, ErrTruncated
		}
		m := MemberSpec{
			Tag:      MemberTag(u16(e[0:2])),
			DataFile: string(blob[nameOff : uint32(nameOff)+uint32(nameLen)]),
			BootOff:  u64(e[12:20]),
			BootLen:  u32(e[20:24]),
		}
		if flags&hotcacheFlagInlined != 0 {
			off := u64(e[24:32])
			ln := u32(e[32:36])
			if off+uint64(ln) > uint64(len(inlineBlob)) {
				return nil, ErrTruncated
			}
			m.BootBytes = inlineBlob[off : off+uint64(ln)]
		}
		members[i] = m
	}
	return &Hotcache{members: members}, nil
}

// Members returns every member spec. Inlined members carry their BootBytes;
// referenced members have BootBytes == nil (their boot lives in DataFile at
// [BootOff, BootOff+BootLen)).
func (h *Hotcache) Members() []MemberSpec { return h.members }

// Boot returns the i-th member's inlined boot bytes, or nil if the boot is
// referenced (not stored in the bundle) or i is out of range.
func (h *Hotcache) Boot(i int) []byte {
	if i < 0 || i >= len(h.members) {
		return nil
	}
	return h.members[i].BootBytes
}

// describeHotcache reads only the RRHC header for `info`.
func describeHotcache(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, hotcacheMagic, hotcacheHeaderLen)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: hotcacheMagic, Name: "hotcache", Ext: ".rrhc", Version: u16(h[4:6]),
		Fields: []Field{{"members", u32(h[8:12])}, {"inlineBlobLen", u64(h[16:24])}},
	}, nil
}
