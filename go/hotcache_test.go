package roaringrange

import (
	"bytes"
	"testing"
)

// TestHotcacheBuilderMatchesRustGolden asserts the Go RRHC builder is byte-for-byte
// with the Rust write_hotcache via the shared golden.
func TestHotcacheBuilderMatchesRustGolden(t *testing.T) {
	rep := func(b byte, n int) []byte {
		s := make([]byte, n)
		for i := range s {
			s[i] = b
		}
		return s
	}
	members := []MemberSpec{
		{MemberRrs, "a.rrs", 16, 8, rep(0xA0, 8)},          // inlined
		{MemberRrti, "terms.rrt", 16, 16, rep(0xB1, 16)},   // inlined (== threshold)
		{MemberRrvi, "vec.rrvi", 48, 40, rep(0xC2, 40)},    // referenced (> threshold)
		{MemberRrsrIdx, "records.idx", 0, 4, rep(0xD3, 4)}, // inlined
	}
	var buf bytes.Buffer
	if err := WriteHotcache(&buf, members, 16); err != nil {
		t.Fatalf("WriteHotcache: %v", err)
	}
	if want := loadGoldenBytes(t, "rrhc"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRHC drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}
