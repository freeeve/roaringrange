package roaringrange

import (
	"bytes"
	"math"
	"testing"

	ivfpq "github.com/freeeve/go-ivfpq"
)

// vectorFixtureParts mirrors the gen_rrvi_golden Rust fixture exactly (exact-in-f32
// literals so the serialization is byte-identical across languages).
func vectorFixtureParts() ivfpq.Parts {
	return ivfpq.Parts{
		Dim: 4, Nlist: 2, M: 2, Nbits: 2, Metric: ivfpq.L2,
		Centroids: []float32{0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5},
		Codebooks: []float32{
			0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 1.0, -1.0,
			1.25, -1.25, 1.5, -1.5, 1.75, -1.75, 2.0, -2.0,
		},
		OPQ:         []float32{1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1},
		IDs:         []uint32{10, 20, 30, 40, 50},
		Assignments: []uint32{0, 1, 0, 1, 0},
		Codes:       []byte{1, 2, 0, 3, 2, 1, 3, 0, 1, 1},
	}
}

// TestWriteRRVIMatchesRustGolden asserts the Go RRVI serializer is byte-for-byte with
// the Rust Ivfpq::write over the same (deterministically assembled) model.
func TestWriteRRVIMatchesRustGolden(t *testing.T) {
	model, err := ivfpq.FromParts(vectorFixtureParts())
	if err != nil {
		t.Fatalf("FromParts: %v", err)
	}
	var buf bytes.Buffer
	if err := WriteRRVI(&buf, model); err != nil {
		t.Fatalf("WriteRRVI: %v", err)
	}
	if want := loadGoldenBytes(t, "rrvi"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRVI drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}

// TestWriteRerankMatchesRustGolden asserts the Go RRVR bf16 serializer is byte-for-byte
// with the Rust write_rerank (l2Normalize=false — normalization is FMA-fragile, so the
// golden checks only the deterministic bf16 rounding).
func TestWriteRerankMatchesRustGolden(t *testing.T) {
	vectors := [][]float32{
		{1.1, -2.2, 0.0, 3.5},
		{100.25, -0.125, 42.0, 7.7},
		{0.0, 0.0, 0.0, 0.0},
	}
	var buf bytes.Buffer
	if err := WriteRerank(&buf, 4, vectors, false); err != nil {
		t.Fatalf("WriteRerank: %v", err)
	}
	if want := loadGoldenBytes(t, "rrvr"); !bytes.Equal(buf.Bytes(), want) {
		t.Errorf("RRVR drifted from the Rust golden:\n got %x\nwant %x", buf.Bytes(), want)
	}
}

// TestWriteRerankDimMismatch checks a wrong-length vector is rejected.
func TestWriteRerankDimMismatch(t *testing.T) {
	if err := WriteRerank(&bytes.Buffer{}, 4, [][]float32{{1, 2, 3}}, false); err == nil {
		t.Error("WriteRerank accepted a vector whose length != dim")
	}
}

// TestF32ToBF16 checks the round-to-nearest-even bf16 conversion on representative
// values (exact truncation, round-up, and a NaN staying NaN).
func TestF32ToBF16(t *testing.T) {
	cases := []struct {
		in   float32
		want uint16
	}{
		{1.0, 0x3F80},  // exact, low bits zero
		{1.1, 0x3F8D},  // 0x3F8CCCCD rounds up
		{-2.0, 0xC000}, // exact
		{0.0, 0x0000},  // zero
	}
	for _, c := range cases {
		if got := f32ToBF16(c.in); got != c.want {
			t.Errorf("f32ToBF16(%g) = %04x, want %04x", c.in, got, c.want)
		}
	}
	if got := f32ToBF16(float32(math.NaN())); got&0x0040 == 0 {
		t.Errorf("f32ToBF16(NaN) = %04x, want a quiet-NaN pattern (0x0040 bit set)", got)
	}
}
