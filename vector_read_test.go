package roaringrange

import (
	"bytes"
	"math"
	"testing"

	ivfpq "github.com/freeeve/go-ivfpq"
)

// TestOpenRRVIRoundTrip builds the RRVI fixture model, reopens the serialized
// bytes, reconstructs the model, and checks it re-serializes byte-for-byte.
func TestOpenRRVIRoundTrip(t *testing.T) {
	model, err := ivfpq.FromParts(vectorFixtureParts())
	if err != nil {
		t.Fatalf("FromParts: %v", err)
	}
	var buf bytes.Buffer
	if err := WriteRRVI(&buf, model); err != nil {
		t.Fatalf("WriteRRVI: %v", err)
	}
	orig := buf.Bytes()

	vi, err := OpenRRVI(bytes.NewReader(orig))
	if err != nil {
		t.Fatalf("OpenRRVI: %v", err)
	}
	h := vi.Header()
	if h.Dim != 4 || h.Nlist != 2 || h.M != 2 || h.Nbits != 2 || !h.HasOPQ || h.N != 5 {
		t.Errorf("header = %+v", h)
	}
	got, err := vi.ToModel()
	if err != nil {
		t.Fatalf("ToModel: %v", err)
	}
	var re bytes.Buffer
	if err := WriteRRVI(&re, got); err != nil {
		t.Fatalf("re-WriteRRVI: %v", err)
	}
	if !bytes.Equal(re.Bytes(), orig) {
		t.Errorf("round-trip drifted")
	}
}

// TestOpenRRVIGolden decodes the Rust-authored RRVI golden and re-serializes it to
// the same bytes.
func TestOpenRRVIGolden(t *testing.T) {
	golden := loadGoldenBytes(t, "rrvi")
	vi, err := OpenRRVI(bytes.NewReader(golden))
	if err != nil {
		t.Fatalf("OpenRRVI(golden): %v", err)
	}
	m, err := vi.ToModel()
	if err != nil {
		t.Fatalf("ToModel: %v", err)
	}
	var re bytes.Buffer
	if err := WriteRRVI(&re, m); err != nil {
		t.Fatalf("re-WriteRRVI: %v", err)
	}
	if !bytes.Equal(re.Bytes(), golden) {
		t.Errorf("golden re-serialize drifted")
	}
}

// TestOpenRerankRoundTrip writes bf16 vectors and checks each decodes back to the
// bf16-rounded original.
func TestOpenRerankRoundTrip(t *testing.T) {
	dim := 4
	vectors := [][]float32{
		{1.1, -2.2, 0.0, 3.5},
		{100.25, -0.125, 42.0, 7.7},
	}
	var buf bytes.Buffer
	if err := WriteRerank(&buf, dim, vectors, false); err != nil {
		t.Fatalf("WriteRerank: %v", err)
	}
	s, err := OpenRerank(bytes.NewReader(buf.Bytes()))
	if err != nil {
		t.Fatalf("OpenRerank: %v", err)
	}
	if s.Dim != dim || s.N != 2 {
		t.Fatalf("Dim/N = %d/%d, want 4/2", s.Dim, s.N)
	}
	bf16 := func(x float32) float32 {
		return math.Float32frombits(uint32(f32ToBF16(x)) << 16)
	}
	for d, want := range vectors {
		got, ok, err := s.Vector(uint32(d))
		if err != nil || !ok {
			t.Fatalf("Vector(%d): ok=%v err=%v", d, ok, err)
		}
		for i, x := range want {
			if got[i] != bf16(x) {
				t.Errorf("Vector(%d)[%d] = %v, want %v", d, i, got[i], bf16(x))
			}
		}
	}
	if _, ok, _ := s.Vector(99); ok {
		t.Errorf("out-of-range Vector should miss")
	}
}
