package roaringrange

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math"
	"slices"

	"github.com/RoaringBitmap/roaring/v2"
)

// The Go byte-capped split-set builder — the build-side mirror of the Rust
// splitset_build::SplitSetBuilder. It reproduces the greedy seal (same size
// estimate), the local-0-based doc IDs with docIdLo as the global base, the rank
// tier per split, and the per-split term Bloom filter, so a split set built in Go
// is byte-for-byte identical to one built in Rust (proven by the shared golden in
// splitsetbuild_test.go). See SPLITSET.md.

const (
	// perNewKeyBytes / perElementBytes are the upper-bound size charges the seal
	// estimate uses; they must match the Rust builder exactly or the split
	// boundaries (and thus the bytes) diverge.
	perNewKeyBytes  = 64
	perElementBytes = 2
	// summaryTagBloom is the summary TLV tag for a term Bloom filter (== the
	// reader's SUMMARY_TAG_BLOOM).
	summaryTagBloom = 1
	// summaryTagFacet is the summary TLV tag for a facet-presence list (== the
	// reader's SUMMARY_TAG_FACET).
	summaryTagFacet = 2
)

// SplitBuildConfig is the build-time configuration for a SplitSetBuilder.
type SplitBuildConfig struct {
	Policy          int          // PolicyTiered | PolicyStableKey
	ByteCap         uint64       // per-split seal target (the FIRST tier's cap when CapMax > 0)
	CapMax          uint64       // geometric tiering: tier i's cap = min(ByteCap << i, CapMax); 0 = flat
	GramSize        uint16       // n-gram window (e.g. 3)
	HeadBoundary    uint32       // split head/tail boundary; 0 -> 65536
	Stride          uint32       // sparse-index stride; 0 -> DefaultStride
	NamePrefix      string       // split filenames: ‹prefix›-s00000.rrs, ...
	SortCol         *SortColSpec // stable-key rank source, or nil
	BloomBitsPerKey uint32       // per-split term Bloom bits/key (0 disables)
	// CaseSensitive builds a case-sensitive split set: n-gram and facet keys are NOT
	// lowercased (v4 RRS splits, case-sensitive RRSF keys, the manifest's case-sensitive
	// flag). The zero value (false) is the default case-folding behavior, byte-identical to
	// before. (This is the inverse of the Rust SplitBuildConfig.case_normalization field; Go
	// uses the zero-value-safe inverse so existing configs are unaffected.)
	CaseSensitive bool
}

// capFor is the byte cap for the split at seal index tier: the flat ByteCap when
// capMax is 0, else doubling per tier and clamped to capMax. Mirrors the Rust
// cap_for exactly — the geometric conformance golden pins the boundary placement.
func capFor(byteCap, capMax uint64, tier int) uint64 {
	if capMax == 0 {
		return byteCap
	}
	shift := min(uint(tier), 63)
	shifted := byteCap << shift
	if shift != 0 && shifted>>shift != byteCap { // overflow
		shifted = ^uint64(0)
	}
	return min(shifted, max(capMax, byteCap))
}

// NamedSplit is one emitted split: its filename and RRS bytes.
type NamedSplit struct {
	Name  string
	Bytes []byte
}

// BuiltSplitSet is the output of SplitSetBuilder.Finish: the manifest, each split's
// (filename, RRS bytes), and — for a faceted build — each split's (filename, RRSF bytes)
// facet sidecar. The caller writes them out; nothing is written here.
type BuiltSplitSet struct {
	Manifest []byte
	Splits   []NamedSplit
	Facets   []NamedSplit
}

// SplitSetBuilder accumulates documents and seals them into byte-capped RRS splits.
type SplitSetBuilder struct {
	cfg          SplitBuildConfig
	headBoundary uint32
	stride       int
	open         map[uint64]*roaring.Bitmap
	openCount    uint32
	globalBase   uint32
	nextGlobalID uint32
	postingsUp   uint64
	// openFacets holds the open split's facet postings: field -> category -> bitmap of local ids.
	openFacets map[string]map[string]*roaring.Bitmap
	hasFacets  bool
	specs      []SplitSpec
	blobs      []NamedSplit
	facetBlobs []NamedSplit
}

// NewSplitSetBuilder creates a builder. A HeadBoundary/Stride of 0 take the RRS defaults.
func NewSplitSetBuilder(cfg SplitBuildConfig) *SplitSetBuilder {
	hb := cfg.HeadBoundary
	if hb == 0 {
		hb = headLimit
	}
	st := int(cfg.Stride)
	if st == 0 {
		st = DefaultStride
	}
	return &SplitSetBuilder{
		cfg:          cfg,
		headBoundary: hb,
		stride:       st,
		open:         make(map[uint64]*roaring.Bitmap),
		openFacets:   make(map[string]map[string]*roaring.Bitmap),
	}
}

// AddText tokenizes text into n-gram keys and appends it as one document, returning
// its global doc id.
func (b *SplitSetBuilder) AddText(text string) (uint32, error) {
	return b.addInner(NgramKeysWith(text, int(b.cfg.GramSize), !b.cfg.CaseSensitive), nil)
}

// AddKeys appends one document by its (deduplicated) n-gram keys, returning its
// global doc id. A keyword-less document still consumes an id (dense id space).
func (b *SplitSetBuilder) AddKeys(keys []uint64) (uint32, error) {
	return b.addInner(keys, nil)
}

// AddFaceted tokenizes text and appends it as one document, recording its facet
// memberships (each field mapped to the categories this document belongs to). Each
// sealed split then gets its own RRSF facet sidecar plus a facet-presence summary, so a
// facet-filtered query can skip a split lacking a selected category. Returns the global id.
func (b *SplitSetBuilder) AddFaceted(text string, facets map[string][]string) (uint32, error) {
	return b.addInner(NgramKeysWith(text, int(b.cfg.GramSize), !b.cfg.CaseSensitive), facets)
}

// addInner is the shared add path: the seal decision (text estimate only — facets live in a
// separate RRSF), then it records the n-gram keys and the document's facets under one local id.
func (b *SplitSetBuilder) addInner(keys []uint64, facets map[string][]string) (uint32, error) {
	var newKeys uint64
	for _, k := range keys {
		if _, ok := b.open[k]; !ok {
			newKeys++
		}
	}
	marginal := newKeys*perNewKeyBytes + uint64(len(keys))*perElementBytes
	if b.openCount > 0 && b.estimate()+marginal > capFor(b.cfg.ByteCap, b.cfg.CapMax, len(b.specs)) {
		if err := b.seal(); err != nil {
			return 0, err
		}
	}
	local := b.openCount
	for _, k := range keys {
		bm, ok := b.open[k]
		if !ok {
			bm = roaring.New()
			b.open[k] = bm
		}
		if bm.CheckedAdd(local) {
			b.postingsUp += perElementBytes
		}
	}
	for field, cats := range facets {
		fm, ok := b.openFacets[field]
		if !ok {
			fm = make(map[string]*roaring.Bitmap)
			b.openFacets[field] = fm
		}
		for _, cat := range cats {
			cb, ok := fm[cat]
			if !ok {
				cb = roaring.New()
				fm[cat] = cb
			}
			cb.Add(local)
			b.hasFacets = true
		}
	}
	b.openCount++
	id := b.nextGlobalID
	b.nextGlobalID++
	return id, nil
}

// DocCount is the number of documents added so far.
func (b *SplitSetBuilder) DocCount() uint32 { return b.nextGlobalID }

// estimate is the upper-bound estimate of the open split's serialized RRS size —
// header + per-key dictionary/posting base + sparse index + accumulated elements.
func (b *SplitSetBuilder) estimate() uint64 {
	nkeys := uint64(len(b.open))
	sparse := ((nkeys + uint64(b.stride) - 1) / uint64(b.stride)) * 8
	return 20 + nkeys*perNewKeyBytes + sparse + b.postingsUp
}

// seal serializes the open split into an immutable RRS blob + a manifest entry,
// then resets the open state with globalBase advanced. A no-op when empty.
func (b *SplitSetBuilder) seal() error {
	if b.openCount == 0 {
		return nil
	}
	keys := make([]uint64, 0, len(b.open))
	for k := range b.open {
		keys = append(keys, k)
	}
	slices.Sort(keys)

	entries := make([]indexEntry, 0, len(keys))
	for _, k := range keys {
		posting, err := b.open[k].ToBytes()
		if err != nil {
			return err
		}
		entries = append(entries, indexEntry{key: k, posting: posting})
	}
	var buf bytes.Buffer
	if err := writeIndexWith(&buf, b.cfg.GramSize, b.stride, entries, !b.cfg.CaseSensitive); err != nil {
		return err
	}

	idx := len(b.specs)
	name := fmt.Sprintf("%s-s%05d.rrs", b.cfg.NamePrefix, idx)
	var tier uint16
	if b.cfg.Policy == PolicyTiered {
		if idx > int(^uint16(0)) {
			tier = ^uint16(0)
		} else {
			tier = uint16(idx)
		}
	}
	var summary []byte
	if b.cfg.BloomBitsPerKey > 0 {
		summary = tlvRecord(summaryTagBloom, bloomBuild(keys, b.cfg.BloomBitsPerKey))
	}
	// Per-split facet sidecar (RRSF) + facet-presence summary (tag 2), when the split holds facets.
	// Order matches Rust: the Bloom record (tag 1) first, then the facet-presence record (tag 2).
	if len(b.openFacets) > 0 {
		summary = append(summary, tlvRecord(summaryTagFacet, facetPresence(b.openFacets, !b.cfg.CaseSensitive))...)
		var fbuf bytes.Buffer
		if err := WriteFacetsWith(&fbuf, openFacetFields(b.openFacets), !b.cfg.CaseSensitive); err != nil {
			return err
		}
		b.facetBlobs = append(b.facetBlobs, NamedSplit{
			Name:  fmt.Sprintf("%s-s%05d.rrf", b.cfg.NamePrefix, idx),
			Bytes: fbuf.Bytes(),
		})
	}
	blob := buf.Bytes()
	b.specs = append(b.specs, SplitSpec{
		DataFile: name,
		Tier:     tier,
		DocCount: b.openCount,
		DocIDLo:  b.globalBase,
		DocIDHi:  b.globalBase + b.openCount - 1,
		Epoch:    0,
		ByteSize: uint64(len(blob)),
		Flags:    0,
		Summary:  summary,
	})
	b.blobs = append(b.blobs, NamedSplit{Name: name, Bytes: blob})

	b.open = make(map[uint64]*roaring.Bitmap)
	b.openFacets = make(map[string]map[string]*roaring.Bitmap)
	b.openCount = 0
	b.globalBase = b.nextGlobalID
	b.postingsUp = 0
	return nil
}

// facetPresence builds the facet-presence summary payload: [count u32 LE][key u64 LE]*, the
// sorted, deduplicated FacetKeys of the categories present in the open split. Mirrors the Rust
// facet_presence.
func facetPresence(facets map[string]map[string]*roaring.Bitmap, caseFold bool) []byte {
	var keys []uint64
	for field, cats := range facets {
		for cat := range cats {
			keys = append(keys, FacetKeyWith(field, cat, caseFold))
		}
	}
	slices.Sort(keys)
	keys = slices.Compact(keys)
	out := make([]byte, 4+len(keys)*8)
	binary.LittleEndian.PutUint32(out[0:4], uint32(len(keys)))
	for i, k := range keys {
		binary.LittleEndian.PutUint64(out[4+i*8:], k)
	}
	return out
}

// openFacetFields converts the open facet postings into []FacetField for WriteFacets, with fields
// in sorted-name order (WriteFacets preserves field order and sorts categories by key, matching
// the Rust facet_fields BTreeMap order). Mirrors the Rust facet_fields.
func openFacetFields(facets map[string]map[string]*roaring.Bitmap) []FacetField {
	names := make([]string, 0, len(facets))
	for field := range facets {
		names = append(names, field)
	}
	slices.Sort(names)
	fields := make([]FacetField, 0, len(names))
	for _, name := range names {
		cats := facets[name]
		catNames := make([]string, 0, len(cats))
		for cat := range cats {
			catNames = append(catNames, cat)
		}
		slices.Sort(catNames)
		fcats := make([]FacetCategory, 0, len(catNames))
		for _, cn := range catNames {
			fcats = append(fcats, FacetCategory{Name: cn, Bitmap: cats[cn]})
		}
		fields = append(fields, FacetField{Name: name, Categories: fcats})
	}
	return fields
}

// Finish seals the final open split and serializes the manifest, returning the
// manifest bytes and every split's (filename, RRS bytes). Errors if a single
// document's postings alone exceed the byte cap (a degenerate corpus).
func (b *SplitSetBuilder) Finish() (*BuiltSplitSet, error) {
	if err := b.seal(); err != nil {
		return nil, err
	}
	for i, s := range b.specs {
		cap := capFor(b.cfg.ByteCap, b.cfg.CapMax, i)
		if s.DocCount == 1 && s.ByteSize > cap {
			return nil, fmt.Errorf(
				"RRSS split %q: a single document's postings (%d B) exceed the byte cap (%d B)",
				s.DataFile, s.ByteSize, cap)
		}
	}
	var tierCount uint16
	if b.cfg.Policy == PolicyTiered {
		if n := len(b.specs); n > int(^uint16(0)) {
			tierCount = ^uint16(0)
		} else {
			tierCount = uint16(n)
		}
	}
	var flags uint16
	if b.cfg.BloomBitsPerKey > 0 {
		flags |= SplitSetFlagBloom
	}
	if b.hasFacets {
		flags |= SplitSetFlagFacet
	}
	if b.cfg.CaseSensitive {
		flags |= SplitSetFlagCaseSensitive
	}
	config := SplitSetConfig{
		Policy:    b.cfg.Policy,
		TierCount: tierCount,
		BaseCount: uint32(len(b.specs)),
		ByteCap:   b.cfg.ByteCap,
		GramSize:  b.cfg.GramSize,
		SortCol:   b.cfg.SortCol,
		Flags:     flags,
	}
	var manifest bytes.Buffer
	if err := WriteSplitSet(&manifest, b.specs, config); err != nil {
		return nil, err
	}
	return &BuiltSplitSet{Manifest: manifest.Bytes(), Splits: b.blobs, Facets: b.facetBlobs}, nil
}

// --- term Bloom filter (mirrors the Rust splitset bloom; deterministic) ---

// bloomK is the number of hash functions for bitsPerKey (≈ bitsPerKey·ln2, 1..=16).
func bloomK(bitsPerKey uint32) uint32 {
	k := uint32(math.Round(float64(bitsPerKey) * math.Ln2))
	return min(max(k, 1), 16)
}

// splitmix64 is the mixer the Bloom derives its two base hashes from.
func splitmix64(z uint64) uint64 {
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

// bloomBuild builds a term Bloom filter over keys at ~bitsPerKey bits per key,
// serialized as [k u32 LE][nbits u32 LE][⌈nbits/8⌉ bytes] — byte-identical to the
// Rust bloom_build.
func bloomBuild(keys []uint64, bitsPerKey uint32) []byte {
	n := max(uint64(len(keys)), 1)
	nbits := max(n*uint64(bitsPerKey), 64)
	if nbits%8 != 0 {
		nbits += 8 - nbits%8
	}
	// The serialized nbits field is u32: clamp to the largest 8-multiple that fits,
	// so a pathological vocabulary degrades to a higher false-positive rate instead
	// of a filter whose truncated stored modulus disagrees with the build modulus —
	// that disagreement yields FALSE NEGATIVES, which prune splits holding real
	// matches. (Mirrors the Rust builder's bloom_build byte-for-byte.)
	if nbits > 0xFFFF_FFF8 {
		nbits = 0xFFFF_FFF8
	}
	k := bloomK(bitsPerKey)
	bits := make([]byte, nbits/8)
	for _, key := range keys {
		h1 := splitmix64(key)
		h2 := splitmix64(key^0x9e3779b97f4a7c15) | 1
		for i := uint64(0); i < uint64(k); i++ {
			pos := (h1 + i*h2) % nbits
			bits[pos/8] |= 1 << (pos % 8)
		}
	}
	out := make([]byte, 8+len(bits))
	binary.LittleEndian.PutUint32(out[0:4], k)
	binary.LittleEndian.PutUint32(out[4:8], uint32(nbits))
	copy(out[8:], bits)
	return out
}

// tlvRecord frames payload as a [tag u8][len u32 LE][payload] summary TLV record.
func tlvRecord(tag byte, payload []byte) []byte {
	out := make([]byte, 5+len(payload))
	out[0] = tag
	binary.LittleEndian.PutUint32(out[1:5], uint32(len(payload)))
	copy(out[5:], payload)
	return out
}
