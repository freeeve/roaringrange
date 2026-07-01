package roaringrange

// The Go term-bodied split-set builder — the build-side mirror of the Rust
// splitset_build::TermSplitSetBuilder: greedy byte-capped sealing, but each
// sealed split is an RRTI term index (WriteTermIndex) instead of a trigram RRS.
// Everything cross-split — tiering, doc-ID ranges, per-split RRSF facet sidecars,
// facet-presence summaries, the manifest (BodyKindTerm, gramSize 0) — matches the
// trigram builder; term Bloom summaries are deferred exactly as in Rust. Proven
// byte-for-byte by the shared golden in termsplitsetbuild_test.go.

import (
	"bytes"
	"fmt"
	"slices"

	"github.com/RoaringBitmap/roaring/v2"
)

const (
	// perNewTermBytes / perTermElementBytes / termIndexHeaderEst are the
	// upper-bound seal-estimate charges; they must match the Rust builder
	// exactly or the split boundaries (and thus every byte after) diverge.
	perNewTermBytes     = 24
	perTermElementBytes = 2
	termIndexHeaderEst  = 128
)

// TermSplitBuildConfig configures a TermSplitSetBuilder.
type TermSplitBuildConfig struct {
	Policy       int          // PolicyTiered | PolicyStableKey
	ByteCap      uint64       // per-split seal target (the FIRST tier's cap when CapMax > 0)
	CapMax       uint64       // geometric tiering: tier i's cap = min(ByteCap << i, CapMax); 0 = flat
	HeadBoundary uint32       // head/tail doc-ID split; 0 -> 65536
	NamePrefix   string       // split filenames: ‹prefix›-s00000.rrt, …
	SortCol      *SortColSpec // stable-key rank source, or nil
	Language     TermLanguage // index language, shared by Stem and Stopwords; required when either is set
	Stem         bool         // apply Snowball stemming in Language (independent of Stopwords)
	Stopwords    bool         // remove the language's stop words (and from queries); requires Language
	// CaseSensitive builds a case-sensitive term split set: terms and facet keys are NOT
	// lowercased (each split's RRTI carries the case-sensitive flag, the RRSF keys are
	// case-sensitive, and the manifest sets its case-sensitive flag). The zero value (false)
	// is the default case-folding behavior, byte-identical to before. Inverse of the Rust
	// TermSplitBuildConfig.case_normalization (Go uses the zero-value-safe inverse).
	CaseSensitive bool
}

// TermSplitSetBuilder accumulates documents and seals them into byte-capped RRTI
// splits with per-split facet sidecars.
type TermSplitSetBuilder struct {
	cfg          TermSplitBuildConfig
	headBoundary uint32
	tok          *TermTokenizer
	open         map[string]*roaring.Bitmap
	openCount    uint32
	globalBase   uint32
	nextGlobalID uint32
	bytesUpper   uint64
	openFacets   map[string]map[string]*roaring.Bitmap
	hasFacets    bool
	specs        []SplitSpec
	blobs        []NamedSplit
	facetBlobs   []NamedSplit
}

// NewTermSplitSetBuilder creates a builder. A HeadBoundary of 0 takes the RRS default.
func NewTermSplitSetBuilder(cfg TermSplitBuildConfig) *TermSplitSetBuilder {
	hb := cfg.HeadBoundary
	if hb == 0 {
		hb = headLimit
	}
	return &TermSplitSetBuilder{
		cfg:          cfg,
		headBoundary: hb,
		tok:          NewTermTokenizerFull(cfg.Language, cfg.Stem, cfg.Stopwords, !cfg.CaseSensitive),
		open:         make(map[string]*roaring.Bitmap),
		openFacets:   make(map[string]map[string]*roaring.Bitmap),
	}
}

// AddText tokenizes text and appends it as one document, returning its global doc
// id. A token-less document still consumes an id (dense id space).
func (b *TermSplitSetBuilder) AddText(text string) (uint32, error) {
	return b.AddFaceted(text, nil)
}

// AddFaceted is AddText plus the document's facet memberships, recorded into the
// open split's RRSF sidecar and facet-presence summary.
func (b *TermSplitSetBuilder) AddFaceted(text string, facets map[string][]string) (uint32, error) {
	terms := b.tok.Tokenize(text)
	slices.Sort(terms)
	terms = slices.Compact(terms)

	// Marginal cost of this doc against the open split: a new term costs the
	// posting/dict base plus its bytes; every occurrence costs a roaring element.
	var marginal uint64
	for _, t := range terms {
		if _, ok := b.open[t]; !ok {
			marginal += perNewTermBytes + uint64(len(t))
		}
		marginal += perTermElementBytes
	}
	if b.openCount > 0 && b.estimate()+marginal > capFor(b.cfg.ByteCap, b.cfg.CapMax, len(b.specs)) {
		if err := b.seal(); err != nil {
			return 0, err
		}
	}

	local := b.openCount
	for _, t := range terms {
		bm, ok := b.open[t]
		if !ok {
			bm = roaring.New()
			b.open[t] = bm
		}
		if bm.CheckedAdd(local) {
			b.bytesUpper += perTermElementBytes
			if !ok {
				b.bytesUpper += perNewTermBytes + uint64(len(t))
			}
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
func (b *TermSplitSetBuilder) DocCount() uint32 { return b.nextGlobalID }

// estimate is the running upper bound on the open split's serialized RRTI size.
func (b *TermSplitSetBuilder) estimate() uint64 {
	return termIndexHeaderEst + b.bytesUpper
}

// seal serializes the open split into an immutable RRTI blob + a manifest entry,
// then resets the open state with globalBase advanced. A no-op when empty.
func (b *TermSplitSetBuilder) seal() error {
	if b.openCount == 0 {
		return nil
	}
	var buf bytes.Buffer
	if err := WriteTermIndexFull(&buf, b.open, b.headBoundary, b.cfg.Language, b.cfg.Stem, b.cfg.Stopwords, !b.cfg.CaseSensitive, 0); err != nil {
		return err
	}

	idx := len(b.specs)
	name := fmt.Sprintf("%s-s%05d.rrt", b.cfg.NamePrefix, idx)
	var tier uint16
	if b.cfg.Policy == PolicyTiered {
		if idx > int(^uint16(0)) {
			tier = ^uint16(0)
		} else {
			tier = uint16(idx)
		}
	}
	// Summary = facet-presence (tag 2) only; the term Bloom (tag 1) is deferred
	// for term bodies, matching Rust.
	var summary []byte
	if len(b.openFacets) > 0 {
		summary = tlvRecord(summaryTagFacet, facetPresence(b.openFacets, !b.cfg.CaseSensitive))
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

	b.open = make(map[string]*roaring.Bitmap)
	b.openFacets = make(map[string]map[string]*roaring.Bitmap)
	b.openCount = 0
	b.globalBase = b.nextGlobalID
	b.bytesUpper = 0
	return nil
}

// Finish seals the final open split and serializes the manifest (BodyKindTerm,
// gramSize 0), returning the manifest and every split's bytes. Errors if any
// single document's postings alone exceed the byte cap.
func (b *TermSplitSetBuilder) Finish() (*BuiltSplitSet, error) {
	if err := b.seal(); err != nil {
		return nil, err
	}
	for i, s := range b.specs {
		cap := capFor(b.cfg.ByteCap, b.cfg.CapMax, i)
		if s.DocCount == 1 && s.ByteSize > cap {
			return nil, fmt.Errorf(
				"RRSS term split %q: a single document's postings (%d B) exceed the byte cap (%d B)",
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
		GramSize:  0, // term-bodied: no n-grams
		BodyKind:  BodyKindTerm,
		SortCol:   b.cfg.SortCol,
		Flags:     flags,
	}
	var manifest bytes.Buffer
	if err := WriteSplitSet(&manifest, b.specs, config); err != nil {
		return nil, err
	}
	return &BuiltSplitSet{Manifest: manifest.Bytes(), Splits: b.blobs, Facets: b.facetBlobs}, nil
}
