package roaringrange

// The Go build-side writer for a v3 trigram RRS monolith — one ordinary RRSI index over a
// whole corpus, the single-index sibling of the split-set builder (splitsetbuild.go) with
// no byte-cap tiering. WriteIndex is the byte-for-byte mirror of the Rust build::write_index
// primitive; TrigramMonolithBuilder is the in-memory corpus -> monolith convenience (the
// simple equivalent of the Rust build_trigram_monolith example's chunked partial->merge path,
// which exists only to bound peak memory on a 100+ GB build). See rust/src/build.rs.

import (
	"io"
	"sort"

	"github.com/RoaringBitmap/roaring/v2"
)

// IndexEntry is one v3 RRS dictionary entry for WriteIndex: a trigram key and its single
// serialized portable-RoaringBitmap posting. The public mirror of the internal indexEntry.
type IndexEntry struct {
	Key     uint64
	Posting []byte
}

// WriteIndex writes a v3 RRS (RRSI) trigram index over key->posting entries to dst — the
// byte-for-byte Go mirror of the Rust build::write_index. Entries are sorted by key here (so
// the caller need not pre-sort), and a stride of zero or less becomes DefaultStride. Each
// posting must be one portable RoaringBitmap (see roaring.Bitmap.ToBytes). See FORMAT.md.
func WriteIndex(dst io.Writer, gramSize uint16, stride int, entries []IndexEntry) error {
	if stride <= 0 {
		stride = DefaultStride
	}
	priv := make([]indexEntry, len(entries))
	for i, e := range entries {
		priv[i] = indexEntry{key: e.Key, posting: e.Posting}
	}
	sort.Slice(priv, func(i, j int) bool { return priv[i].key < priv[j].key })
	return writeIndex(dst, gramSize, stride, priv)
}

// TrigramMonolithBuilder accumulates trigram postings over a whole corpus into a single v3
// RRS index. Docs are added in ascending doc-ID order — each Add returns the doc's id, and an
// empty doc (no trigrams) still consumes an id so the doc-ID space stays dense and aligned
// with the records / facet / lookup sidecars. Write seals the accumulated postings to one
// RRSI index, byte-identical to a split set's single split over the same docs. All postings
// are held in memory; for a 100+ GB corpus use the chunked Rust builder instead.
type TrigramMonolithBuilder struct {
	gramSize uint16
	stride   int
	open     map[uint64]*roaring.Bitmap
	nextID   uint32
}

// NewTrigramMonolithBuilder opens a monolith builder for the given trigram size and sparse
// stride. A gramSize of zero defaults to 3; a stride of zero or less to DefaultStride.
func NewTrigramMonolithBuilder(gramSize uint16, stride int) *TrigramMonolithBuilder {
	if gramSize == 0 {
		gramSize = 3
	}
	if stride <= 0 {
		stride = DefaultStride
	}
	return &TrigramMonolithBuilder{
		gramSize: gramSize,
		stride:   stride,
		open:     make(map[uint64]*roaring.Bitmap),
	}
}

// AddText tokenizes text into gramSize-gram trigram keys and indexes them under the next doc
// ID, returning that ID. Mirrors SplitSetBuilder.AddText.
func (b *TrigramMonolithBuilder) AddText(text string) uint32 {
	return b.AddKeys(NgramKeys(text, int(b.gramSize)))
}

// AddKeys indexes the given trigram keys under the next doc ID and returns that ID. An empty
// keys slice still advances the doc-ID space (the doc is indexed as having no trigrams).
func (b *TrigramMonolithBuilder) AddKeys(keys []uint64) uint32 {
	id := b.nextID
	for _, k := range keys {
		bm := b.open[k]
		if bm == nil {
			bm = roaring.New()
			b.open[k] = bm
		}
		bm.Add(id)
	}
	b.nextID++
	return id
}

// DocCount returns the number of docs added so far (the next doc ID).
func (b *TrigramMonolithBuilder) DocCount() uint32 { return b.nextID }

// Write seals the accumulated postings into one v3 RRS index on dst — each trigram's bitmap
// serialized as one portable posting, laid out key-sorted via WriteIndex.
func (b *TrigramMonolithBuilder) Write(dst io.Writer) error {
	entries := make([]IndexEntry, 0, len(b.open))
	for k, bm := range b.open {
		posting, err := bm.ToBytes()
		if err != nil {
			return err
		}
		entries = append(entries, IndexEntry{Key: k, Posting: posting})
	}
	return WriteIndex(dst, b.gramSize, b.stride, entries)
}
