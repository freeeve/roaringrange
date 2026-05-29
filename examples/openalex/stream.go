package main

import (
	"bufio"
	"compress/gzip"
	"encoding/binary"
	"encoding/json"
	"log"
	"os"
	"sort"
	"strconv"
	"strings"

	"github.com/RoaringBitmap/roaring/v2"
	rr "github.com/freeeve/roaringrange"
	rs "github.com/freeeve/roaringsearch"
)

// streamBuild builds the RRS index, RRSF facet sidecar, and record store from
// inputs too large to hold in memory, by streaming the works twice:
//
//	pass 1: decode only (id, display_name, cited_by_count) for every work, then
//	        sort by cited_by_count desc to assign doc IDs (rank 0 = most cited),
//	        producing an id -> docID lookup (~12 B/work).
//	pass 2: re-stream each work, resolve its docID via the lookup, add its text
//	        to the in-memory trigram index and its values to the facet bitmaps,
//	        and append its record JSON to a temp file in input order (recording
//	        docID + offset).
//
// Finally the records are rewritten in doc-ID order from the temp file, the
// index is transcoded to RRS, and the facets are written. Peak memory is the
// trigram index + facet bitmaps + ~16 B/work of bookkeeping — it never holds the
// works' text, so it scales far past the load-everything path. (The index still
// builds in RAM, so it suits corpora whose *index* fits memory; doc-ID-range
// chunking is the next layer for indexes larger than RAM.)
func streamBuild(files []string, rrsPath, facetsPath, binP, idxP, ftsr, tmpRec string, limit int) error {
	lookup, err := rankPass(files, limit)
	if err != nil {
		return err
	}
	log.Printf("pass1: ranked %d works (rank 0 = most cited)", len(lookup))
	docOf := func(id uint64) (uint32, bool) {
		i := sort.Search(len(lookup), func(i int) bool { return lookup[i].id >= id })
		if i < len(lookup) && lookup[i].id == id {
			return lookup[i].doc, true
		}
		return 0, false
	}

	idx := rs.NewIndex(3)
	fa := newFacetAccum()
	tf, err := os.Create(tmpRec)
	if err != nil {
		return err
	}
	tw := bufio.NewWriterSize(tf, 1<<20)
	metas := make([]recMeta, 0, len(lookup))
	var off uint64
	var seen int
	err = streamLines(files, func(line []byte) bool {
		var w work
		if json.Unmarshal(line, &w) != nil || w.DisplayName == "" {
			return true
		}
		id, ok := parseWID(w.ID)
		if !ok {
			return true
		}
		doc, ok := docOf(id)
		if !ok {
			return true // not in the pass-1 set
		}
		idx.Add(doc, buildText(&w))
		fa.add(doc, &w)
		rec := buildRecord(&w)
		tw.Write(rec)
		metas = append(metas, recMeta{doc: doc, off: off, ln: uint32(len(rec))})
		off += uint64(len(rec))
		if seen++; seen%1_000_000 == 0 {
			log.Printf("  pass2 indexed %dM works", seen/1_000_000)
		}
		return true
	})
	if err != nil {
		tw.Flush()
		tf.Close()
		return err
	}
	tw.Flush()
	tf.Close()
	log.Printf("pass2: indexed %d works", len(metas))

	if err := writeRecordsOrdered(tmpRec, metas, binP, idxP); err != nil {
		return err
	}
	os.Remove(tmpRec)

	if err := idx.SaveToFile(ftsr); err != nil {
		return err
	}
	src, err := os.Open(ftsr)
	if err != nil {
		return err
	}
	dst, err := os.Create(rrsPath)
	if err != nil {
		src.Close()
		return err
	}
	if err := rr.Transcode(src, dst); err != nil {
		src.Close()
		dst.Close()
		return err
	}
	src.Close()
	dst.Close()
	os.Remove(ftsr)
	if fi, _ := os.Stat(rrsPath); fi != nil {
		log.Printf("wrote RRS %s (%d bytes)", rrsPath, fi.Size())
	}

	return fa.write(facetsPath)
}

// idDoc maps an OpenAlex work id (numeric W-tail) to its assigned doc ID.
type idDoc struct {
	id  uint64
	doc uint32
}

// recMeta records where a work's record JSON landed in the input-order temp file.
type recMeta struct {
	doc uint32
	off uint64
	ln  uint32
}

// rankPass streams the inputs reading only what is needed to rank works, then
// returns an id->docID lookup sorted by id for binary search. Works are ranked
// by cited_by_count descending (stable), so doc 0 is the most-cited.
func rankPass(files []string, limit int) ([]idDoc, error) {
	type entry struct {
		id    uint64
		cited int32
	}
	entries := make([]entry, 0, 1<<20)
	err := streamLines(files, func(line []byte) bool {
		var w struct {
			ID          string `json:"id"`
			DisplayName string `json:"display_name"`
			Cited       int    `json:"cited_by_count"`
		}
		if json.Unmarshal(line, &w) != nil || w.DisplayName == "" {
			return true
		}
		id, ok := parseWID(w.ID)
		if !ok {
			return true
		}
		entries = append(entries, entry{id: id, cited: int32(w.Cited)})
		if len(entries)%1_000_000 == 0 {
			log.Printf("  pass1 scanned %dM works", len(entries)/1_000_000)
		}
		return limit <= 0 || len(entries) < limit
	})
	if err != nil {
		return nil, err
	}
	sort.SliceStable(entries, func(i, j int) bool { return entries[i].cited > entries[j].cited })
	lookup := make([]idDoc, len(entries))
	for i, e := range entries {
		lookup[i] = idDoc{id: e.id, doc: uint32(i)}
	}
	sort.Slice(lookup, func(i, j int) bool { return lookup[i].id < lookup[j].id })
	return lookup, nil
}

// writeRecordsOrdered rewrites the input-order temp record file into the final
// doc-ID-ordered record store (blob + uint64 offset index of length numDocs+1).
func writeRecordsOrdered(tmpRec string, metas []recMeta, binP, idxP string) error {
	sort.Slice(metas, func(i, j int) bool { return metas[i].doc < metas[j].doc })
	src, err := os.Open(tmpRec)
	if err != nil {
		return err
	}
	defer src.Close()
	bin, err := os.Create(binP)
	if err != nil {
		return err
	}
	recIdx, err := os.Create(idxP)
	if err != nil {
		bin.Close()
		return err
	}
	bw := bufio.NewWriterSize(bin, 1<<20)
	iw := bufio.NewWriterSize(recIdx, 1<<20)
	var u [8]byte
	writeOff := func(v uint64) {
		binary.LittleEndian.PutUint64(u[:], v)
		iw.Write(u[:])
	}
	writeOff(0)
	var cum uint64
	buf := make([]byte, 0, 4096)
	for _, m := range metas {
		if cap(buf) < int(m.ln) {
			buf = make([]byte, m.ln)
		}
		buf = buf[:m.ln]
		if _, err := src.ReadAt(buf, int64(m.off)); err != nil {
			bw.Flush()
			iw.Flush()
			bin.Close()
			recIdx.Close()
			return err
		}
		bw.Write(buf)
		cum += uint64(m.ln)
		writeOff(cum)
	}
	bw.Flush()
	iw.Flush()
	bin.Close()
	recIdx.Close()
	return nil
}

// streamLines scans every line of every gzipped input file, invoking fn. fn
// returns false to stop early (used to honor a work limit).
func streamLines(files []string, fn func(line []byte) bool) error {
	for _, path := range files {
		f, err := os.Open(path)
		if err != nil {
			log.Printf("skip %s: %v", path, err)
			continue
		}
		gz, err := gzip.NewReader(f)
		if err != nil {
			f.Close()
			log.Printf("skip %s: %v", path, err)
			continue
		}
		sc := bufio.NewScanner(gz)
		sc.Buffer(make([]byte, 1<<20), 1<<24)
		for sc.Scan() {
			line := sc.Bytes()
			if len(line) == 0 {
				continue
			}
			if !fn(line) {
				gz.Close()
				f.Close()
				return nil
			}
		}
		gz.Close()
		f.Close()
	}
	return nil
}

// parseWID extracts the numeric tail of an OpenAlex work id
// ("https://openalex.org/W2741809807" -> 2741809807) for use as a compact,
// stable key across the two passes.
func parseWID(idURL string) (uint64, bool) {
	s := trimOpenAlexID(idURL)
	if len(s) < 2 || (s[0] != 'W' && s[0] != 'w') {
		return 0, false
	}
	n, err := strconv.ParseUint(s[1:], 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// buildText assembles the indexed text for a work: title + abstract + authors +
// venue.
func buildText(w *work) string {
	var sb strings.Builder
	sb.WriteString(w.DisplayName)
	appendField(&sb, reconstructAbstract(w.AbstractIdx))
	appendField(&sb, authorNames(w))
	appendField(&sb, w.PrimaryLocation.Source.DisplayName)
	return sb.String()
}

// buildRecord marshals a work's stored record JSON.
func buildRecord(w *work) []byte {
	b, _ := json.Marshal(record{
		ID: trimOpenAlexID(w.ID),
		T:  w.DisplayName,
		A:  authorNames(w),
		Y:  w.PublicationYear,
		V:  w.PrimaryLocation.Source.DisplayName,
		C:  w.CitedByCount,
	})
	return b
}

// facetFields is the ordered set of facet fields emitted to the RRSF sidecar.
var facetFields = []string{"year", "type", "oa", "language", "topic"}

// facetValue returns a work's value for facet field fi (empty = omit).
func facetValue(w *work, fi int) string {
	switch fi {
	case 0:
		if w.PublicationYear != 0 {
			return strconv.Itoa(w.PublicationYear)
		}
		return ""
	case 1:
		return w.Type
	case 2:
		return w.OpenAccess.OAStatus
	case 3:
		return w.Language
	default:
		return topicName(w)
	}
}

// facetAccum accumulates one doc-ID posting per (field, category) as works stream
// by, preserving first-seen category order per field.
type facetAccum struct {
	cats  []map[string]*roaring.Bitmap
	order [][]string
}

func newFacetAccum() *facetAccum {
	fa := &facetAccum{
		cats:  make([]map[string]*roaring.Bitmap, len(facetFields)),
		order: make([][]string, len(facetFields)),
	}
	for i := range fa.cats {
		fa.cats[i] = make(map[string]*roaring.Bitmap)
	}
	return fa
}

func (fa *facetAccum) add(doc uint32, w *work) {
	for fi := range facetFields {
		v := facetValue(w, fi)
		if v == "" {
			continue
		}
		bm := fa.cats[fi][v]
		if bm == nil {
			bm = roaring.New()
			fa.cats[fi][v] = bm
			fa.order[fi] = append(fa.order[fi], v)
		}
		bm.Add(doc)
	}
}

func (fa *facetAccum) write(path string) error {
	fields := make([]rr.FacetField, 0, len(facetFields))
	for fi, name := range facetFields {
		ff := rr.FacetField{Name: name}
		for _, v := range fa.order[fi] {
			ff.Categories = append(ff.Categories, rr.FacetCategory{Name: v, Bitmap: fa.cats[fi][v]})
		}
		fields = append(fields, ff)
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	bw := bufio.NewWriterSize(f, 1<<20)
	if err := rr.WriteFacets(bw, fields); err != nil {
		return err
	}
	if err := bw.Flush(); err != nil {
		return err
	}
	fi, _ := os.Stat(path)
	log.Printf("wrote facets %s (%d bytes)", path, fi.Size())
	return nil
}
