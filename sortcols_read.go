package roaringrange

import (
	"encoding/binary"
	"fmt"
	"io"
	"math"
)

// The read side of the RRSC sort-column store (the inverse of WriteSortcols in
// sortcols.go). Columns are dense fixed-width arrays indexed by doc ID; the reader
// keeps the small column directory + name blob resident and range-reads a column's
// values on demand. Mirrors rust/src/sortcols.rs. See SORTCOLS.md.

func init() {
	register(Format{Magic: sortcolsMagic, Name: "sortcols", Ext: ".rrsc", Describe: describeSortcols})
}

// colMeta is one parsed RRSC column directory entry.
type colMeta struct {
	name     string
	typeCode byte
	dataOff  uint64
	rows     uint32
}

// SortcolStore is a reference reader over an RRSC store accessed by byte range. It
// reads the header, column directory, and name blob up front; each column's dense
// values are fetched per Column call.
type SortcolStore struct {
	r    io.ReaderAt
	rows uint32
	cols []colMeta
}

// ColumnMeta is the public view of one column's shape for `info`/`dump`.
type ColumnMeta struct {
	Name string
	Type string
	Rows uint32
}

// typeName maps an on-disk type code to its display name (see ColumnValues).
func typeName(code byte) string {
	switch code {
	case 1:
		return "u16"
	case 2:
		return "u32"
	case 3:
		return "i32"
	case 4:
		return "f32"
	}
	return "unknown"
}

// typeWidth maps an on-disk type code to its value width in bytes.
func typeWidth(code byte) (int, error) {
	switch code {
	case 1:
		return 2, nil
	case 2, 3, 4:
		return 4, nil
	}
	return 0, fmt.Errorf("sortcols: unknown column type code %d", code)
}

// OpenSortcols reads and validates the RRSC header, column directory, and name
// blob. Column values are read lazily.
func OpenSortcols(r io.ReaderAt) (*SortcolStore, error) {
	h, err := readHeader(r, sortcolsMagic, sortcolsHeaderLen)
	if err != nil {
		return nil, err
	}
	if u16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	colCount := int(u16(h[6:8]))
	rows := u32(h[8:12])
	strLen := u32(h[12:16])

	entries, err := boundedRead(r, sortcolsHeaderLen, uint64(colCount)*sortcolsColEntry)
	if err != nil {
		return nil, err
	}
	strOff := int64(sortcolsHeaderLen) + int64(colCount)*sortcolsColEntry
	blob, err := boundedRead(r, strOff, uint64(strLen))
	if err != nil {
		return nil, err
	}

	cols := make([]colMeta, colCount)
	for i := range cols {
		e := entries[i*sortcolsColEntry:]
		nameOff := u32(e[0:4])
		nameLen := u16(e[4:6])
		if uint64(nameOff)+uint64(nameLen) > uint64(len(blob)) {
			return nil, ErrTruncated
		}
		cols[i] = colMeta{
			name:     string(blob[nameOff : uint32(nameOff)+uint32(nameLen)]),
			typeCode: e[6],
			dataOff:  u64(e[8:16]),
			rows:     u32(e[16:20]),
		}
	}
	return &SortcolStore{r: r, rows: rows, cols: cols}, nil
}

// Rows reports the per-column value count (one per doc).
func (s *SortcolStore) Rows() int { return int(s.rows) }

// Columns returns the shape of every column without reading its values.
func (s *SortcolStore) Columns() []ColumnMeta {
	out := make([]ColumnMeta, len(s.cols))
	for i, c := range s.cols {
		out[i] = ColumnMeta{Name: c.name, Type: typeName(c.typeCode), Rows: c.rows}
	}
	return out
}

// Column reads and decodes the i-th column's dense values into a typed
// ColumnValues (the same type WriteSortcols consumes).
func (s *SortcolStore) Column(i int) (SortColumn, error) {
	if i < 0 || i >= len(s.cols) {
		return SortColumn{}, fmt.Errorf("sortcols: column %d out of range", i)
	}
	c := s.cols[i]
	w, err := typeWidth(c.typeCode)
	if err != nil {
		return SortColumn{}, err
	}
	buf, err := boundedRead(s.r, int64(c.dataOff), uint64(c.rows)*uint64(w))
	if err != nil {
		return SortColumn{}, err
	}
	n := int(c.rows)
	var vals ColumnValues
	switch c.typeCode {
	case 1:
		col := make(U16Column, n)
		for j := range col {
			col[j] = u16(buf[j*2:])
		}
		vals = col
	case 2:
		col := make(U32Column, n)
		for j := range col {
			col[j] = u32(buf[j*4:])
		}
		vals = col
	case 3:
		col := make(I32Column, n)
		for j := range col {
			col[j] = int32(u32(buf[j*4:]))
		}
		vals = col
	case 4:
		col := make(F32Column, n)
		for j := range col {
			col[j] = math.Float32frombits(u32(buf[j*4:]))
		}
		vals = col
	}
	return SortColumn{Name: c.name, Values: vals}, nil
}

// ReadAll reads every column into the SortColumn slice WriteSortcols consumes, so
// WriteSortcols(ReadAll(x)) reproduces x byte-for-byte.
func (s *SortcolStore) ReadAll() ([]SortColumn, error) {
	out := make([]SortColumn, len(s.cols))
	for i := range s.cols {
		c, err := s.Column(i)
		if err != nil {
			return nil, err
		}
		out[i] = c
	}
	return out, nil
}

// describeSortcols reads only the RRSC header for `info`.
func describeSortcols(r io.ReaderAt) (*FileInfo, error) {
	h, err := readHeader(r, sortcolsMagic, sortcolsHeaderLen)
	if err != nil {
		return nil, err
	}
	if binary.LittleEndian.Uint16(h[4:6]) != 1 {
		return nil, ErrVersion
	}
	return &FileInfo{
		Magic: sortcolsMagic, Name: "sortcols", Ext: ".rrsc", Version: u16(h[4:6]),
		Fields: []Field{{"columns", u16(h[6:8])}, {"rows", u32(h[8:12])}},
	}, nil
}
