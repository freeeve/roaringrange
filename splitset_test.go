package roaringrange

import (
	"bytes"
	"encoding/hex"
	"testing"
)

// goldenSplitSet is the cross-language conformance golden: the exact bytes the
// Rust writer (splitset_build::conformance_golden) emits for the same inputs. The
// Rust side asserts these bytes too (splitset_build::tests), so a match here proves
// the Go and Rust RRSS manifest writers agree byte-for-byte.
const goldenSplitSet = "525253530100080001000200030000000200000039000000090000000000000000000002000000002e0000000b00030001030000000000000000000000000000000000000f0000006400000000000000630000000000000000100000000000000000000000000000000000000000000000000000000000000f0000000f0001003200000064000000950000000000000000080000000000000000000000000000000000000000000003000000000000001e0000001000000005000000e8030000ec030000010000000002000000000000070000000000000003000000000000000600000000000000626173652d7330303030302e727273626173652d7330303030312e72727364656c74612d6430303030302e727273636f727075732e727273630102030401000000ff"

// TestWriteSplitSetMatchesRustGolden builds the field-exhaustive conformance
// manifest and asserts its bytes equal the Rust writer's golden.
func TestWriteSplitSetMatchesRustGolden(t *testing.T) {
	splits := []SplitSpec{
		{DataFile: "base-s00000.rrs", Tier: 0, DocCount: 100, DocIDLo: 0, DocIDHi: 99, Epoch: 0, ByteSize: 4096, Flags: 0, Summary: nil},
		{DataFile: "base-s00001.rrs", Tier: 1, DocCount: 50, DocIDLo: 100, DocIDHi: 149, Epoch: 0, ByteSize: 2048, Flags: 0, Summary: []byte{0x01, 0x02, 0x03}},
		{DataFile: "delta-d00000.rrs", Tier: 0, DocCount: 5, DocIDLo: 1000, DocIDHi: 1004, Epoch: 7, ByteSize: 512, Flags: SplitFlagHasTombstone, Summary: []byte{0x04, 0x01, 0x00, 0x00, 0x00, 0xff}},
	}
	config := SplitSetConfig{
		Policy:    PolicyStableKey,
		TierCount: 2,
		BaseCount: 2,
		ByteCap:   32 << 20,
		GramSize:  3,
		SortCol:   &SortColSpec{Name: "corpus.rrsc", Column: 3, Descending: true},
		Flags:     SplitSetFlagTombstones,
	}

	var buf bytes.Buffer
	if err := WriteSplitSet(&buf, splits, config); err != nil {
		t.Fatalf("WriteSplitSet: %v", err)
	}
	got := hex.EncodeToString(buf.Bytes())
	if got != goldenSplitSet {
		t.Fatalf("RRSS manifest bytes differ from the Rust golden:\n got: %s\nwant: %s", got, goldenSplitSet)
	}
}

// TestWriteSplitSetRejectsBadBaseCount checks the base/delta boundary guard.
func TestWriteSplitSetRejectsBadBaseCount(t *testing.T) {
	var buf bytes.Buffer
	err := WriteSplitSet(&buf, []SplitSpec{{DataFile: "a.rrs"}}, SplitSetConfig{BaseCount: 2})
	if err == nil {
		t.Fatal("expected an error when base_count exceeds the split count")
	}
}
