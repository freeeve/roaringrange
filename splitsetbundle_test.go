package roaringrange

import (
	"bytes"
	"io"
	"testing"
)

// The bundle over the shared split-set fixture must be byte-for-byte what the Rust
// write_splitset_bundle emits — asserted against testdata/rrhc_bundle_build_golden.txt,
// which rust/src/splitset_bundle.rs pins from the same fixture (task 082).
func TestWriteSplitsetBundleMatchesSharedGolden(t *testing.T) {
	built := conformanceBuild(t)
	var buf bytes.Buffer
	if err := WriteSplitsetBundle(&buf, built, 0, 1<<20); err != nil {
		t.Fatalf("WriteSplitsetBundle: %v", err)
	}
	if want := loadGoldenBytes(t, "rrhc_bundle"); !bytes.Equal(buf.Bytes(), want) {
		t.Fatalf("bundle bytes drift from the shared golden: got %d bytes, want %d",
			buf.Len(), len(want))
	}
}

// Every split's boot region is inlined by name in seal order, maxSplits caps the members,
// and a split whose bytes are not a valid RRS header errors instead of emitting a bogus
// member.
func TestWriteSplitsetBundleInlinesBootsCapsAndValidates(t *testing.T) {
	built := conformanceBuild(t)
	var buf bytes.Buffer
	if err := WriteSplitsetBundle(&buf, built, 0, 1<<20); err != nil {
		t.Fatalf("WriteSplitsetBundle: %v", err)
	}
	hc, err := OpenHotcache(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenHotcache: %v", err)
	}
	members := hc.Members()
	if len(members) != len(built.Splits) {
		t.Fatalf("got %d members, want one per split (%d)", len(members), len(built.Splits))
	}
	for i, m := range members {
		s := built.Splits[i]
		bootLen, err := rrsBootLen(s.Bytes)
		if err != nil {
			t.Fatalf("rrsBootLen(%s): %v", s.Name, err)
		}
		if m.Tag != MemberRrs || m.DataFile != s.Name || m.BootOff != 0 || uint64(m.BootLen) != bootLen {
			t.Fatalf("member %d = %+v, want RRS member %q boot [0, %d)", i, m, s.Name, bootLen)
		}
		if !bytes.Equal(m.BootBytes, s.Bytes[:bootLen]) {
			t.Fatalf("member %q inlined boot differs from the split's boot region", s.Name)
		}
	}

	// maxSplits inlines only the leading (top-tier) splits.
	var capped bytes.Buffer
	if err := WriteSplitsetBundle(&capped, built, 1, 1<<20); err != nil {
		t.Fatalf("WriteSplitsetBundle(maxSplits=1): %v", err)
	}
	hc2, err := OpenHotcache(bytes.NewReader(capped.Bytes()))
	if err != nil {
		t.Fatalf("OpenHotcache(capped): %v", err)
	}
	if got := hc2.Members(); len(got) != 1 || got[0].DataFile != built.Splits[0].Name {
		t.Fatalf("capped bundle members = %+v, want just %q", got, built.Splits[0].Name)
	}

	// A boot larger than inlineThreshold is referenced, not inlined: the member remains
	// (with its boot range) but carries no bytes, so the reader cold-opens that split.
	var refd bytes.Buffer
	if err := WriteSplitsetBundle(&refd, built, 0, 1); err != nil {
		t.Fatalf("WriteSplitsetBundle(threshold=1): %v", err)
	}
	hc3, err := OpenHotcache(bytes.NewReader(refd.Bytes()))
	if err != nil {
		t.Fatalf("OpenHotcache(referenced): %v", err)
	}
	for _, m := range hc3.Members() {
		if m.BootBytes != nil {
			t.Fatalf("member %q inlined despite a 1-byte threshold", m.DataFile)
		}
	}

	// Malformed split bytes must error.
	bad := &BuiltSplitSet{Splits: []NamedSplit{{Name: "x.rrs", Bytes: []byte("nope")}}}
	if err := WriteSplitsetBundle(io.Discard, bad, 0, 1<<20); err == nil {
		t.Fatal("malformed split header must error")
	}
}
