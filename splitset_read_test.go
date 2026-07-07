package roaringrange

import (
	"bufio"
	"bytes"
	"encoding/hex"
	"os"
	"reflect"
	"strings"
	"testing"
)

// loadGoldenLine reads the `<name> <hex>` line matching name from a multi-line
// golden bundle (e.g. the RRSS manifest golden, which lists the manifest plus every
// split body and facet sidecar).
func loadGoldenLine(t *testing.T, file, name string) []byte {
	t.Helper()
	f, err := os.Open("testdata/" + file)
	if err != nil {
		t.Fatalf("open %s: %v", file, err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<28)
	for sc.Scan() {
		n, h, ok := strings.Cut(strings.TrimSpace(sc.Text()), " ")
		if ok && n == name {
			b, err := hex.DecodeString(h)
			if err != nil {
				t.Fatalf("golden %s/%s: bad hex: %v", file, name, err)
			}
			return b
		}
	}
	if err := sc.Err(); err != nil {
		t.Fatalf("scan %s: %v", file, err)
	}
	t.Fatalf("golden %s: no line named %q", file, name)
	return nil
}

// splitsetFixture mirrors the RRSS golden inputs (splitset_test.go).
func splitsetFixture() ([]SplitSpec, SplitSetConfig) {
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
	return splits, config
}

// TestOpenSplitSetRoundTrip writes the fixture manifest, reopens it, and checks the
// config and splits rehydrate exactly and re-serialize byte-for-byte.
func TestOpenSplitSetRoundTrip(t *testing.T) {
	splits, config := splitsetFixture()
	var buf bytes.Buffer
	if err := WriteSplitSet(&buf, splits, config); err != nil {
		t.Fatalf("WriteSplitSet: %v", err)
	}
	ss, err := OpenSplitSet(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenSplitSet: %v", err)
	}
	if !reflect.DeepEqual(ss.Config, config) {
		t.Errorf("Config = %+v\nwant %+v", ss.Config, config)
	}
	if !reflect.DeepEqual(ss.Splits, splits) {
		t.Errorf("Splits = %+v\nwant %+v", ss.Splits, splits)
	}
	var re bytes.Buffer
	if err := WriteSplitSet(&re, ss.Splits, ss.Config); err != nil {
		t.Fatalf("re-WriteSplitSet: %v", err)
	}
	if !bytes.Equal(re.Bytes(), buf.Bytes()) {
		t.Errorf("round-trip drifted")
	}
}

// TestOpenSplitSetGolden decodes the Rust-authored RRSS golden and re-serializes it
// to the same bytes, pinning the reader against the reference implementation.
func TestOpenSplitSetGolden(t *testing.T) {
	golden := loadGoldenLine(t, "rrss_build_golden.txt", "manifest")
	ss, err := OpenSplitSet(bytes.NewReader(golden))
	if err != nil {
		t.Fatalf("OpenSplitSet(golden): %v", err)
	}
	var re bytes.Buffer
	if err := WriteSplitSet(&re, ss.Splits, ss.Config); err != nil {
		t.Fatalf("re-WriteSplitSet: %v", err)
	}
	if !bytes.Equal(re.Bytes(), golden) {
		t.Errorf("golden re-serialize drifted")
	}
}
