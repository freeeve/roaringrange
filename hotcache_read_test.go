package roaringrange

import (
	"bytes"
	"reflect"
	"testing"
)

// TestOpenHotcacheGolden decodes the Rust-authored RRHC golden and checks each
// member rehydrates: inlined boots carry their bytes, the referenced one does not.
func TestOpenHotcacheGolden(t *testing.T) {
	rep := func(b byte, n int) []byte {
		s := make([]byte, n)
		for i := range s {
			s[i] = b
		}
		return s
	}
	hc, err := OpenHotcache(bytes.NewReader(loadGoldenBytes(t, "rrhc")))
	if err != nil {
		t.Fatalf("OpenHotcache(golden): %v", err)
	}
	members := hc.Members()
	if len(members) != 4 {
		t.Fatalf("members = %d, want 4", len(members))
	}
	want := []MemberSpec{
		{MemberRrs, "a.rrs", 16, 8, rep(0xA0, 8)},
		{MemberRrti, "terms.rrt", 16, 16, rep(0xB1, 16)},
		{MemberRrvi, "vec.rrvi", 48, 40, nil}, // referenced: BootBytes not stored
		{MemberRrsrIdx, "records.idx", 0, 4, rep(0xD3, 4)},
	}
	if !reflect.DeepEqual(members, want) {
		t.Errorf("members = %+v\nwant %+v", members, want)
	}
	if hc.Boot(2) != nil {
		t.Errorf("referenced member Boot(2) should be nil")
	}
}

// TestOpenHotcacheRoundTrip round-trips an all-inlined bundle (a high threshold so
// every boot is stored inline and thus recoverable byte-for-byte).
func TestOpenHotcacheRoundTrip(t *testing.T) {
	members := []MemberSpec{
		{MemberRrs, "a.rrs", 16, 3, []byte{1, 2, 3}},
		{MemberRril, "ids.rril", 0, 2, []byte{9, 9}},
	}
	var buf bytes.Buffer
	if err := WriteHotcache(&buf, members, 1024); err != nil {
		t.Fatalf("WriteHotcache: %v", err)
	}
	hc, err := OpenHotcache(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenHotcache: %v", err)
	}
	var re bytes.Buffer
	if err := WriteHotcache(&re, hc.Members(), 1024); err != nil {
		t.Fatalf("re-WriteHotcache: %v", err)
	}
	if !bytes.Equal(re.Bytes(), buf.Bytes()) {
		t.Errorf("round-trip drifted")
	}
}
