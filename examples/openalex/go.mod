// Example loader for the OpenAlex Works snapshot. Its own module so the
// roaringrange core stays free of a roaringsearch dependency; roaringsearch is
// used here only to build the FTSR index that roaringrange transcodes.
module github.com/freeeve/roaringrange/examples/openalex

go 1.25

require (
	github.com/RoaringBitmap/roaring/v2 v2.14.4
	github.com/freeeve/roaringrange v0.0.0
	github.com/freeeve/roaringsearch v0.5.9
)

require (
	github.com/bits-and-blooms/bitset v1.24.2 // indirect
	github.com/freeeve/fst-go v0.1.0 // indirect
	github.com/freeeve/go-ivfpq v0.1.0 // indirect
	github.com/freeeve/go-stemmers v0.0.0-20260606195828-3c78df9017f5 // indirect
	github.com/freeeve/msgpck v0.3.2 // indirect
	github.com/klauspost/compress v1.18.6 // indirect
	github.com/mschoch/smat v0.2.0 // indirect
)

replace github.com/freeeve/roaringrange => ../..
