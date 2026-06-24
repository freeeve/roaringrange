package roaringrange

// The Go build-side writers for the RRVI similarity index (.rrvi) and its RRVR bf16
// re-rank sidecar — byte-for-byte mirrors of the Rust vector_build Ivfpq::write and
// write_rerank. The trained model comes from the standalone github.com/freeeve/go-ivfpq
// trainer (the à la carte split: that library trains, this serializes the format). See
// rust/src/vector.rs / vector_build.rs and VECTORS.md.

import (
	"encoding/binary"
	"fmt"
	"io"
	"math"

	ivfpq "github.com/freeeve/go-ivfpq"
)

const (
	rrviMagic      = "RRVI"
	rrviVersion    = 1
	rrviHeaderSize = 48
	rrviFlagOPQ    = 1

	rrvrMagic      = "RRVR"
	rrvrVersion    = 1
	rrvrHeaderSize = 20
	rerankBF16     = 0
)

// WriteRRVI serializes a trained IVFPQ model to dst in the RRVI byte layout the Rust
// VectorIndex (and the wasm reader) read over HTTP Range — the byte-for-byte mirror of
// the Rust Ivfpq::write. Layout (all little-endian): a 48-byte header, an optional OPQ
// rotation, the coarse centroids, the PQ codebooks, an nlist×12 cluster directory
// (absolute list offset + count), then each cluster's [ids][codes]. See VECTORS.md.
func WriteRRVI(dst io.Writer, m *ivfpq.Model) error {
	ksub := 1 << m.Nbits
	dsub := m.Dim / m.M
	opqSize := 0
	if m.OPQ != nil {
		opqSize = m.Dim * m.Dim * 4
	}
	centroidsSize := m.Nlist * m.Dim * 4
	codebooksSize := m.M * ksub * dsub * 4
	dirSize := m.Nlist * 12
	listsOff := uint64(rrviHeaderSize + opqSize + centroidsSize + codebooksSize + dirSize)

	header := make([]byte, rrviHeaderSize)
	copy(header[0:4], rrviMagic)
	binary.LittleEndian.PutUint16(header[4:6], rrviVersion)
	header[6] = byte(m.Metric)
	if m.OPQ != nil {
		header[7] = rrviFlagOPQ
	}
	binary.LittleEndian.PutUint32(header[8:12], uint32(m.Dim))
	binary.LittleEndian.PutUint32(header[12:16], uint32(m.Nlist))
	binary.LittleEndian.PutUint32(header[16:20], uint32(m.M))
	header[20] = m.Nbits
	// header[21:24] pad = 0
	binary.LittleEndian.PutUint64(header[24:32], m.N)
	// header[32:48] reserved = 0
	if _, err := dst.Write(header); err != nil {
		return err
	}

	if m.OPQ != nil {
		if err := writeF32LE(dst, m.OPQ); err != nil {
			return err
		}
	}
	if err := writeF32LE(dst, m.Centroids); err != nil {
		return err
	}
	if err := writeF32LE(dst, m.Codebooks); err != nil {
		return err
	}

	// Cluster directory: absolute list offset + count.
	dir := make([]byte, dirSize)
	off := listsOff
	for c := range m.Nlist {
		e := dir[c*12:]
		binary.LittleEndian.PutUint64(e[0:8], off)
		binary.LittleEndian.PutUint32(e[8:12], uint32(len(m.ListIDs[c])))
		off += uint64(len(m.ListIDs[c])*4 + len(m.ListCodes[c]))
	}
	if _, err := dst.Write(dir); err != nil {
		return err
	}

	// Lists: per cluster, [ids u32 LE][codes bytes].
	for c := range m.Nlist {
		ids := m.ListIDs[c]
		buf := make([]byte, len(ids)*4)
		for i, id := range ids {
			binary.LittleEndian.PutUint32(buf[i*4:], id)
		}
		if _, err := dst.Write(buf); err != nil {
			return err
		}
		if _, err := dst.Write(m.ListCodes[c]); err != nil {
			return err
		}
	}
	return nil
}

// writeF32LE writes xs as little-endian f32 words, batched through one buffer.
func writeF32LE(dst io.Writer, xs []float32) error {
	buf := make([]byte, len(xs)*4)
	for i, x := range xs {
		binary.LittleEndian.PutUint32(buf[i*4:], math.Float32bits(x))
	}
	_, err := dst.Write(buf)
	return err
}

// WriteRerank writes the RRVR bf16 re-rank sidecar (read by the Rust RerankStore): a
// 20-byte header then a dense bf16 array of vectors keyed by doc ID (slice index == doc
// ID). Every vector must have length dim. Set l2Normalize for an inner-product index so
// the stored vectors match the unit-sphere space the index was built in. Byte-for-byte
// with the Rust write_rerank.
func WriteRerank(dst io.Writer, dim int, vectors [][]float32, l2Normalize bool) error {
	header := make([]byte, rrvrHeaderSize)
	copy(header[0:4], rrvrMagic)
	binary.LittleEndian.PutUint16(header[4:6], rrvrVersion)
	header[6] = rerankBF16
	// header[7] pad = 0
	binary.LittleEndian.PutUint32(header[8:12], uint32(dim))
	binary.LittleEndian.PutUint64(header[12:20], uint64(len(vectors)))
	if _, err := dst.Write(header); err != nil {
		return err
	}

	buf := make([]byte, 0, len(vectors)*dim*2)
	var tmp [2]byte
	for _, v := range vectors {
		if len(v) != dim {
			return fmt.Errorf("roaringrange: rerank vector length %d != dim %d", len(v), dim)
		}
		src := v
		if l2Normalize {
			src = rerankNormalize(v)
		}
		for _, x := range src {
			binary.LittleEndian.PutUint16(tmp[:], f32ToBF16(x))
			buf = append(buf, tmp[0], tmp[1])
		}
	}
	_, err := dst.Write(buf)
	return err
}

// f32ToBF16 rounds an f32 to bf16 (its high 16 bits) with round-to-nearest-even, matching
// the Rust f32_to_bf16. A NaN stays a (quiet) NaN; the unsigned add wraps mod 2^32 like
// Rust's wrapping_add.
func f32ToBF16(x float32) uint16 {
	bits := math.Float32bits(x)
	if x != x { // NaN
		return uint16(bits>>16) | 0x0040
	}
	roundingBias := ((bits >> 16) & 1) + 0x7fff
	return uint16((bits + roundingBias) >> 16)
}

// rerankNormalize returns v scaled to unit L2 norm; a zero vector is returned unchanged
// (mirrors the Rust normalize). Named to avoid the n-gram tokenizer's normalize.
func rerankNormalize(v []float32) []float32 {
	var sum float32
	for _, x := range v {
		sum += x * x
	}
	norm := float32(math.Sqrt(float64(sum)))
	out := make([]float32, len(v))
	if norm == 0 {
		copy(out, v)
		return out
	}
	for i, x := range v {
		out[i] = x / norm
	}
	return out
}
