package roaringrange

import (
	"io"
	"math"

	ivfpq "github.com/freeeve/go-ivfpq"
)

// The read side of the RRVI IVFPQ similarity index and its RRVR bf16 re-rank
// sidecar (the inverse of WriteRRVI / WriteRerank in vector.go). The header, coarse
// centroids, PQ codebooks, optional OPQ rotation, and cluster directory are read up
// front; each cluster's [ids][codes] is range-read on demand. go-ivfpq has no format
// reader, so the layout is decoded here from VECTORS.md. Mirrors rust/src/vector.rs.

func init() {
	register(Format{Magic: rrviMagic, Name: "vector", Ext: ".rrvi", Describe: describeVectors})
	register(Format{Magic: rrvrMagic, Name: "rerank", Ext: ".rrvr", Describe: describeRerank})
}

// VectorHeader is the RRVI index shape.
type VectorHeader struct {
	Dim, Nlist, M int
	Nbits         uint8
	Metric        ivfpq.Metric
	N             uint64
	HasOPQ        bool
}

// clusterDir is one parsed cluster directory entry.
type clusterDir struct {
	off   uint64
	count uint32
}

// VectorIndex is a reference reader over an RRVI index accessed by byte range. The
// centroids, codebooks, optional OPQ, and directory are resident after Open; each
// cluster's ids/codes are fetched per Cluster call.
type VectorIndex struct {
	r         io.ReaderAt
	hdr       VectorHeader
	opq       []float32
	centroids []float32
	codebooks []float32
	dir       []clusterDir
}

// readF32 reads count little-endian f32 values at off.
func readF32(r io.ReaderAt, off int64, count int) ([]float32, error) {
	buf, err := boundedRead(r, off, uint64(count)*4)
	if err != nil {
		return nil, err
	}
	out := make([]float32, count)
	for i := range out {
		out[i] = math.Float32frombits(u32(buf[i*4:]))
	}
	return out, nil
}

// OpenRRVI reads and validates the RRVI header and resident regions (OPQ,
// centroids, codebooks, cluster directory). Cluster ids/codes are read lazily.
func OpenRRVI(r io.ReaderAt) (*VectorIndex, error) {
	h, err := readHeader(r, rrviMagic, rrviHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != rrviVersion {
		return nil, ErrVersion
	}
	hdr := VectorHeader{
		Dim:    int(u32(h[8:12])),
		Nlist:  int(u32(h[12:16])),
		M:      int(u32(h[16:20])),
		Nbits:  h[20],
		Metric: ivfpq.Metric(h[6]),
		N:      u64(h[24:32]),
		HasOPQ: h[7]&rrviFlagOPQ != 0,
	}
	if hdr.M == 0 || hdr.Dim%hdr.M != 0 {
		return nil, ErrTruncated
	}
	dsub := hdr.Dim / hdr.M
	ksub := 1 << hdr.Nbits

	off := int64(rrviHeaderSize)
	vi := &VectorIndex{r: r, hdr: hdr}
	if hdr.HasOPQ {
		if vi.opq, err = readF32(r, off, hdr.Dim*hdr.Dim); err != nil {
			return nil, err
		}
		off += int64(hdr.Dim) * int64(hdr.Dim) * 4
	}
	if vi.centroids, err = readF32(r, off, hdr.Nlist*hdr.Dim); err != nil {
		return nil, err
	}
	off += int64(hdr.Nlist) * int64(hdr.Dim) * 4
	if vi.codebooks, err = readF32(r, off, hdr.M*ksub*dsub); err != nil {
		return nil, err
	}
	off += int64(hdr.M) * int64(ksub) * int64(dsub) * 4

	dirBuf, err := boundedRead(r, off, uint64(hdr.Nlist)*12)
	if err != nil {
		return nil, err
	}
	vi.dir = make([]clusterDir, hdr.Nlist)
	for c := range vi.dir {
		e := dirBuf[c*12:]
		vi.dir[c] = clusterDir{off: u64(e[0:8]), count: u32(e[8:12])}
	}
	return vi, nil
}

// Header returns the index shape.
func (v *VectorIndex) Header() VectorHeader { return v.hdr }

// Cluster reads the c-th cluster's doc IDs and packed PQ codes (M bytes per vector).
func (v *VectorIndex) Cluster(c int) (ids []uint32, codes []byte, err error) {
	if c < 0 || c >= len(v.dir) {
		return nil, nil, ErrTruncated
	}
	d := v.dir[c]
	n := int(d.count)
	buf, err := boundedRead(v.r, int64(d.off), uint64(n)*4+uint64(n)*uint64(v.hdr.M))
	if err != nil {
		return nil, nil, err
	}
	ids = make([]uint32, n)
	for i := range ids {
		ids[i] = u32(buf[i*4:])
	}
	codes = buf[n*4:]
	return ids, codes, nil
}

// ToModel reconstructs the in-memory ivfpq.Model (reading every cluster), so
// WriteRRVI(ToModel(x)) reproduces x byte-for-byte.
func (v *VectorIndex) ToModel() (*ivfpq.Model, error) {
	m := &ivfpq.Model{
		Dim: v.hdr.Dim, Nlist: v.hdr.Nlist, M: v.hdr.M, Nbits: v.hdr.Nbits,
		Metric: v.hdr.Metric, N: v.hdr.N, OPQ: v.opq,
		Centroids: v.centroids, Codebooks: v.codebooks,
		ListIDs:   make([][]uint32, v.hdr.Nlist),
		ListCodes: make([][]byte, v.hdr.Nlist),
	}
	for c := range v.dir {
		ids, codes, err := v.Cluster(c)
		if err != nil {
			return nil, err
		}
		m.ListIDs[c] = ids
		m.ListCodes[c] = codes
	}
	return m, nil
}

// metricName maps a metric code to its display name.
func metricName(m ivfpq.Metric) string {
	if m == ivfpq.L2 {
		return "l2"
	}
	return "inner_product"
}

// describeVectors reads only the RRVI header for `info`.
func describeVectors(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, rrviMagic, rrviHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != rrviVersion {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: rrviMagic, Name: "vector", Ext: ".rrvi", Version: u16(h[4:6]),
		Fields: []Field{
			{"vectors", u64(h[24:32])},
			{"dim", u32(h[8:12])},
			{"nlist", u32(h[12:16])},
			{"m", u32(h[16:20])},
			{"nbits", h[20]},
			{"metric", metricName(ivfpq.Metric(h[6]))},
			{"opq", h[7]&rrviFlagOPQ != 0},
		},
	}, nil
}

// RerankStore is a reference reader over an RRVR bf16 sidecar accessed by byte
// range: dense bf16 vectors keyed by doc ID (index == doc ID).
type RerankStore struct {
	r   io.ReaderAt
	Dim int
	N   uint64
}

// OpenRerank reads and validates the RRVR header.
func OpenRerank(r io.ReaderAt) (*RerankStore, error) {
	h, err := readHeader(r, rrvrMagic, rrvrHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != rrvrVersion {
		return nil, ErrVersion
	}
	return &RerankStore{r: r, Dim: int(u32(h[8:12])), N: u64(h[12:20])}, nil
}

// Vector decodes the bf16 vector stored for a doc ID back to f32. ok is false when
// doc is out of range.
func (s *RerankStore) Vector(doc uint32) ([]float32, bool, error) {
	if uint64(doc) >= s.N {
		return nil, false, nil
	}
	off := int64(rrvrHeaderSize) + int64(doc)*int64(s.Dim)*2
	buf, err := boundedRead(s.r, off, uint64(s.Dim)*2)
	if err != nil {
		return nil, false, err
	}
	out := make([]float32, s.Dim)
	for i := range out {
		out[i] = math.Float32frombits(uint32(u16(buf[i*2:])) << 16)
	}
	return out, true, nil
}

// describeRerank reads only the RRVR header for `info`.
func describeRerank(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, rrvrMagic, rrvrHeaderSize)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != rrvrVersion {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: rrvrMagic, Name: "rerank", Ext: ".rrvr", Version: u16(h[4:6]),
		Fields: []Field{{"vectors", u64(h[12:20])}, {"dim", u32(h[8:12])}},
	}, nil
}
