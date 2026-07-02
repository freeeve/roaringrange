// Package roaringrange turns a roaringsearch (FTSR) index into a
// range-fetchable static index (RRS, see FORMAT.md) that a browser can query
// over HTTP Range requests with no backend.
//
// The layout puts the whole n-gram dictionary in one contiguous, key-sorted
// block so a reader loads a sparse view in a single ranged GET, then fetches
// each posting by absolute byte offset. Postings are byte-identical portable
// RoaringBitmaps, so transcoding copies them verbatim and the Rust/WASM reader
// deserializes them with the `roaring` crate. Optional faceting is layered on
// via a companion sidecar (see FACETS.md).
package roaringrange

import "errors"

// srcMagic is the roaringsearch on-disk magic (the transcode input).
const srcMagic = "FTSR"

var (
	// ErrSrcMagic is returned when the source is not a roaringsearch (FTSR) index.
	ErrSrcMagic = errors.New("source is not a roaringsearch (FTSR) index")
	// ErrMagic is returned when an index does not start with the RRS magic.
	ErrMagic = errors.New("bad RRS magic")
	// ErrVersion is returned when an index's format version is unsupported. The RRS
	// reader is v3-only, matching the Rust reference reader; a v2 file has a different
	// header and dictionary layout and would otherwise misparse silently.
	ErrVersion = errors.New("unsupported RRS format version")
	// ErrTruncated is returned when an index ends before its declared structure.
	ErrTruncated = errors.New("truncated index")
	// ErrCompressedRecord is returned when a version-2 record store holds a
	// zstd-compressed frame (tag 1) but the store was opened without a dictionary
	// decoder — open it with OpenRecordStoreWithDict to inflate compressed records.
	ErrCompressedRecord = errors.New("compressed record (zstd frame) requires opening the store with OpenRecordStoreWithDict")
)
