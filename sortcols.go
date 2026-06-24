package roaringrange

// The Go build-side writer for the RRSC sort-column store (.rrsc) — the mirror of the
// Rust build::write_sortcols / write_perm, byte-for-byte. Columns are dense
// fixed-width arrays indexed by doc ID (u16/u32/i32/f32), laid out contiguously after
// a name string blob; every column holds one value per doc. A secondary
// (date-descending) full index uses write_perm — a one-column u32 store named
// "primary" mapping its doc-ID space back to the primary one. See rust/src/build.rs
// and rust/src/sortcols.rs.

import (
	"encoding/binary"
	"fmt"
	"io"
	"math"
)

const (
	sortcolsMagic     = "RRSC"
	sortcolsHeaderLen = 16
	sortcolsColEntry  = 24
)

// ColumnValues is one sort column's dense values in doc-ID order. The concrete type
// (U16Column / U32Column / I32Column / F32Column) selects the on-disk value type.
type ColumnValues interface {
	Len() int
	typeCode() byte
	width() int
	writeTo(w io.Writer) error
}

// U16Column / U32Column / I32Column / F32Column are the four on-disk value types.
type (
	U16Column []uint16
	U32Column []uint32
	I32Column []int32
	F32Column []float32
)

func (c U16Column) Len() int       { return len(c) }
func (c U16Column) typeCode() byte { return 1 }
func (c U16Column) width() int     { return 2 }
func (c U32Column) Len() int       { return len(c) }
func (c U32Column) typeCode() byte { return 2 }
func (c U32Column) width() int     { return 4 }
func (c I32Column) Len() int       { return len(c) }
func (c I32Column) typeCode() byte { return 3 }
func (c I32Column) width() int     { return 4 }
func (c F32Column) Len() int       { return len(c) }
func (c F32Column) typeCode() byte { return 4 }
func (c F32Column) width() int     { return 4 }

func (c U16Column) writeTo(w io.Writer) error {
	b := make([]byte, 2*len(c))
	for i, x := range c {
		binary.LittleEndian.PutUint16(b[i*2:], x)
	}
	_, err := w.Write(b)
	return err
}

// writeU32s emits the 4-byte little-endian words for any 32-bit column (u32/i32/f32
// share the byte layout once reinterpreted).
func writeU32s(w io.Writer, words func(i int) uint32, n int) error {
	b := make([]byte, 4*n)
	for i := range n {
		binary.LittleEndian.PutUint32(b[i*4:], words(i))
	}
	_, err := w.Write(b)
	return err
}

func (c U32Column) writeTo(w io.Writer) error {
	return writeU32s(w, func(i int) uint32 { return c[i] }, len(c))
}
func (c I32Column) writeTo(w io.Writer) error {
	return writeU32s(w, func(i int) uint32 { return uint32(c[i]) }, len(c))
}
func (c F32Column) writeTo(w io.Writer) error {
	return writeU32s(w, func(i int) uint32 { return math.Float32bits(c[i]) }, len(c))
}

// SortColumn is one named sort column: a display name plus its dense values.
type SortColumn struct {
	Name   string
	Values ColumnValues
}

// WriteSortcols writes the RRSC store for cols to dst — byte-for-byte with the Rust
// write_sortcols. Every column must hold the same number of values (one per doc).
func WriteSortcols(dst io.Writer, cols []SortColumn) error {
	rows := 0
	if len(cols) > 0 {
		rows = cols[0].Values.Len()
	}
	for _, c := range cols {
		if c.Values.Len() != rows {
			return fmt.Errorf("sortcols columns must all have the same length")
		}
	}

	var blob []byte
	type span struct {
		off uint32
		ln  uint16
	}
	spans := make([]span, len(cols))
	for i, c := range cols {
		spans[i] = span{off: uint32(len(blob)), ln: uint16(len(c.Name))}
		blob = append(blob, c.Name...)
	}
	strBlobOff := sortcolsHeaderLen + len(cols)*sortcolsColEntry
	dataStart := uint64(strBlobOff + len(blob))

	header := make([]byte, sortcolsHeaderLen)
	copy(header[0:4], sortcolsMagic)
	binary.LittleEndian.PutUint16(header[4:6], 1) // version
	binary.LittleEndian.PutUint16(header[6:8], uint16(len(cols)))
	binary.LittleEndian.PutUint32(header[8:12], uint32(rows))
	binary.LittleEndian.PutUint32(header[12:16], uint32(len(blob)))
	if _, err := dst.Write(header); err != nil {
		return err
	}

	off := dataStart
	e := make([]byte, sortcolsColEntry)
	for i, c := range cols {
		binary.LittleEndian.PutUint32(e[0:4], spans[i].off)
		binary.LittleEndian.PutUint16(e[4:6], spans[i].ln)
		e[6] = c.Values.typeCode()
		e[7] = 0 // pad
		binary.LittleEndian.PutUint64(e[8:16], off)
		binary.LittleEndian.PutUint32(e[16:20], uint32(rows))
		binary.LittleEndian.PutUint32(e[20:24], 0) // reserved
		if _, err := dst.Write(e); err != nil {
			return err
		}
		off += uint64(rows * c.Values.width())
	}

	if _, err := dst.Write(blob); err != nil {
		return err
	}
	for _, c := range cols {
		if err := c.Values.writeTo(dst); err != nil {
			return err
		}
	}
	return nil
}

// WritePerm writes a one-column u32 RRSC store named "primary" mapping a secondary
// doc-ID space back to the primary one (primaryOfSecondary[secondary] = primary) —
// byte-for-byte with the Rust write_perm.
func WritePerm(dst io.Writer, primaryOfSecondary []uint32) error {
	return WriteSortcols(dst, []SortColumn{{Name: "primary", Values: U32Column(primaryOfSecondary)}})
}
