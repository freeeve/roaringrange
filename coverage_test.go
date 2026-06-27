package roaringrange

import (
	"bytes"
	"encoding/binary"
	"io"
	"strings"
	"testing"
)

// TestTranscodeConvenience exercises the default-stride Transcode wrapper and a
// round-trip open.
func TestTranscodeConvenience(t *testing.T) {
	ftsr := buildFTSR(3, map[uint64][]byte{
		10: makeBitmap(t, 1, 2, 70000),
		20: makeBitmap(t, 3),
	})
	var rrs bytes.Buffer
	if err := Transcode(bytes.NewReader(ftsr), &rrs); err != nil {
		t.Fatalf("Transcode: %v", err)
	}
	raw := rrs.Bytes()
	if string(raw[0:4]) != Magic {
		t.Fatalf("magic = %q, want %q", raw[0:4], Magic)
	}
	idx, err := Open(bytes.NewReader(raw))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	if idx.NgramCount() != 2 {
		t.Fatalf("NgramCount = %d, want 2", idx.NgramCount())
	}
}

// TestTranscodeRejectsBadSource rejects input that is not a roaringsearch FTSR.
func TestTranscodeRejectsBadSource(t *testing.T) {
	if err := TranscodeStride(bytes.NewReader([]byte("NOPExxxxxxxx")), io.Discard, 2); err != ErrSrcMagic {
		t.Fatalf("bad magic: got %v, want ErrSrcMagic", err)
	}
	if err := TranscodeStride(bytes.NewReader([]byte("FT")), io.Discard, 2); err != ErrSrcMagic {
		t.Fatalf("too short: got %v, want ErrSrcMagic", err)
	}
}

// TestTranscodeRejectsTruncated rejects an FTSR whose body is shorter than its
// declared entry count.
func TestTranscodeRejectsTruncated(t *testing.T) {
	// "FTSR" + reserved(2) + gramSize=3 + count=1, then no entry bytes.
	hdrOnly := append([]byte(srcMagic), 0, 0, 3, 0, 1, 0, 0, 0)
	if err := TranscodeStride(bytes.NewReader(hdrOnly), io.Discard, 2); err != ErrTruncated {
		t.Fatalf("missing entry: got %v, want ErrTruncated", err)
	}
	// key + size header present but the declared payload bytes are missing.
	truncPayload := append([]byte(srcMagic), 0, 0, 3, 0, 1, 0, 0, 0)
	truncPayload = append(truncPayload, 5, 0, 0, 0, 0, 0, 0, 0) // key = 5
	truncPayload = append(truncPayload, 200, 0, 0, 0)           // size = 200, but no payload
	if err := TranscodeStride(bytes.NewReader(truncPayload), io.Discard, 2); err != ErrTruncated {
		t.Fatalf("missing payload: got %v, want ErrTruncated", err)
	}
}

// TestOpenRejectsBadMagic rejects a file that does not start with the RRS magic.
func TestOpenRejectsBadMagic(t *testing.T) {
	buf := make([]byte, headerSize)
	copy(buf[0:4], "XXXX")
	if _, err := Open(bytes.NewReader(buf)); err != ErrMagic {
		t.Fatalf("got %v, want ErrMagic", err)
	}
}

// TestOpenRejectsWrongVersion rejects any unsupported version. v3 (case-folding) and
// v4 (case-sensitive) are the only accepted versions; a v2 file shares the magic but has
// a 20-byte header and a 24-byte dictionary stride, so parsing it as v3 would silently
// return garbage postings rather than an error.
func TestOpenRejectsWrongVersion(t *testing.T) {
	for _, v := range []uint16{0, 1, 2, 5, 99} {
		buf := make([]byte, headerSize)
		copy(buf[0:4], Magic)
		binary.LittleEndian.PutUint16(buf[4:6], v)
		binary.LittleEndian.PutUint32(buf[12:16], 1) // valid stride so only the version trips
		if _, err := Open(bytes.NewReader(buf)); err != ErrVersion {
			t.Fatalf("version %d: got %v, want ErrVersion", v, err)
		}
	}
}

// TestSerializePostingRejectsGarbage surfaces a deserialize error for non-roaring
// posting bytes.
func TestSerializePostingRejectsGarbage(t *testing.T) {
	if _, err := serializePosting([]byte{0xde, 0xad, 0xbe, 0xef}); err == nil {
		t.Fatal("expected error for garbage posting bytes")
	}
}

// TestRuneNgramKeyBranches covers every key-derivation branch: 32-bit packing
// for n<=2, 8-bit packing for ASCII 3<=n<=8, and the FNV hash for non-ASCII or
// long windows.
func TestRuneNgramKeyBranches(t *testing.T) {
	if k := runeNgramKey([]rune{'a', 'b'}); k != (uint64('a')<<32 | uint64('b')) {
		t.Fatalf("bigram pack = %d", k)
	}
	if k := runeNgramKey([]rune{'a', 'b', 'c'}); k != 6382179 {
		t.Fatalf("ascii trigram pack = %d, want 6382179", k)
	}
	nonASCII := []rune{'a', 'f', 0xE9} // 'é'
	if runeNgramKey(nonASCII) != hashRunes(nonASCII) {
		t.Fatal("non-ASCII window must use the FNV hash")
	}
	long := []rune("abcdefghi") // 9 runes > 8
	if runeNgramKey(long) != hashRunes(long) {
		t.Fatal("n>8 window must use the FNV hash")
	}
}

// TestNgramKeysUnicode exercises the per-word + FNV path end to end.
func TestNgramKeysUnicode(t *testing.T) {
	keys := NgramKeys("café société", 3)
	if len(keys) == 0 {
		t.Fatal("expected keys for unicode query")
	}
}

// FuzzNgramKeys ensures tokenization never panics on arbitrary input/gram sizes, and that the
// case-sensitive path (caseFold=false) is consistent with the default: it never panics, the
// default NgramKeys equals NgramKeysWith(.., true), and folding an already-lowercase string
// yields the same keys whether folding is on or off (the only difference is case).
func FuzzNgramKeys(f *testing.F) {
	f.Add("legends & lattes", 3)
	f.Add("café société", 3)
	f.Add("Roaring RANGE Index", 3)
	f.Add("", 0)
	f.Fuzz(func(t *testing.T, s string, n int) {
		g := n % 10
		if g < 0 {
			g = -g
		}
		def := NgramKeys(s, g)
		if !equalU64(def, NgramKeysWith(s, g, true)) {
			t.Fatalf("NgramKeys != NgramKeysWith(.., true) for %q g=%d", s, g)
		}
		_ = NgramKeysWith(s, g, false) // must not panic
		// On already-lowercase input, case folding on/off must agree.
		low := strings.ToLower(s)
		if !equalU64(NgramKeysWith(low, g, true), NgramKeysWith(low, g, false)) {
			t.Fatalf("case-fold on/off disagree on lowercased %q g=%d", low, g)
		}
	})
}

// equalU64 reports whether two uint64 slices are equal (order-sensitive).
func equalU64(a, b []uint64) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
