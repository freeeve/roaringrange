// The conformance module is intentionally separate from the roaringrange core
// module so the core has no dependency on roaringsearch. It exists only to
// verify that an index BUILT by roaringsearch, transcoded by roaringrange, and
// READ by the roaringrange reference reader returns identical results to
// roaringsearch's own search — locking the FTSR format + n-gram key derivation
// across the two libraries.
module github.com/freeeve/roaringrange/conformance

go 1.25

require (
	github.com/RoaringBitmap/roaring/v2 v2.14.4
	github.com/freeeve/roaringrange v0.0.0
	github.com/freeeve/roaringsearch v0.5.9
)

require (
	github.com/bits-and-blooms/bitset v1.24.2 // indirect
	github.com/freeeve/msgpck v0.3.2 // indirect
	github.com/mschoch/smat v0.2.0 // indirect
)

replace github.com/freeeve/roaringrange => ../
