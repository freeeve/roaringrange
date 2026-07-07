package main

import (
	"os"

	rr "github.com/freeeve/roaringrange"
)

// The per-format dumpers. Each opens the matching reader over f and returns a
// JSON-serializable structure mirroring the format's writer inputs. Postings and
// vectors are elided unless opt.postings is set.

func dumpLookup(f *os.File, opt dumpOpts) any {
	l, err := rr.OpenLookup(f)
	if err != nil {
		fail("%v", err)
	}
	entries, err := l.Entries()
	if err != nil {
		fail("%v", err)
	}
	lo, hi := pageBounds(len(entries), opt.offset, opt.limit)
	rows := make([]map[string]any, 0, hi-lo)
	for _, e := range entries[lo:hi] {
		rows = append(rows, map[string]any{"hash": e.Hash, "verify": e.Verify, "doc": e.Doc})
	}
	return map[string]any{"format": "RRIL", "count": l.Count(), "entries": rows}
}

func dumpSortcols(f *os.File, opt dumpOpts) any {
	s, err := rr.OpenSortcols(f)
	if err != nil {
		fail("%v", err)
	}
	cols := make([]map[string]any, 0, len(s.Columns()))
	for i, m := range s.Columns() {
		col := map[string]any{"name": m.Name, "type": m.Type, "rows": m.Rows}
		if opt.postings {
			sc, err := s.Column(i)
			if err != nil {
				fail("%v", err)
			}
			col["values"] = columnValues(sc.Values)
		}
		cols = append(cols, col)
	}
	return map[string]any{"format": "RRSC", "rows": s.Rows(), "columns": cols}
}

// columnValues converts a typed column to a plain slice for JSON.
func columnValues(v rr.ColumnValues) any {
	switch c := v.(type) {
	case rr.U16Column:
		return c
	case rr.U32Column:
		return c
	case rr.I32Column:
		return c
	case rr.F32Column:
		return c
	}
	return nil
}

func dumpHotcache(f *os.File) any {
	hc, err := rr.OpenHotcache(f)
	if err != nil {
		fail("%v", err)
	}
	members := make([]map[string]any, 0, len(hc.Members()))
	for _, m := range hc.Members() {
		members = append(members, map[string]any{
			"tag": uint16(m.Tag), "dataFile": m.DataFile,
			"bootOff": m.BootOff, "bootLen": m.BootLen, "inlined": m.BootBytes != nil,
		})
	}
	return map[string]any{"format": "RRHC", "members": members}
}

func dumpSplitSet(f *os.File) any {
	ss, err := rr.OpenSplitSet(f)
	if err != nil {
		fail("%v", err)
	}
	splits := make([]map[string]any, 0, len(ss.Splits))
	for _, s := range ss.Splits {
		splits = append(splits, map[string]any{
			"dataFile": s.DataFile, "tier": s.Tier, "docCount": s.DocCount,
			"docIDLo": s.DocIDLo, "docIDHi": s.DocIDHi, "epoch": s.Epoch,
			"byteSize": s.ByteSize, "flags": s.Flags, "summaryLen": len(s.Summary),
		})
	}
	cfg := map[string]any{
		"policy": ss.Config.Policy, "bodyKind": ss.Config.BodyKind,
		"tierCount": ss.Config.TierCount, "baseCount": ss.Config.BaseCount,
		"byteCap": ss.Config.ByteCap, "gramSize": ss.Config.GramSize,
		"flags": ss.Config.Flags,
	}
	if ss.Config.SortCol != nil {
		cfg["sortCol"] = map[string]any{
			"name": ss.Config.SortCol.Name, "column": ss.Config.SortCol.Column,
			"descending": ss.Config.SortCol.Descending,
		}
	}
	return map[string]any{"format": "RRSS", "config": cfg, "splits": splits}
}

func dumpFacets(f *os.File, opt dumpOpts) any {
	fi, err := rr.OpenFacets(f)
	if err != nil {
		fail("%v", err)
	}
	cats := fi.Categories()
	fields := make([]map[string]any, 0, len(fi.Fields()))
	for _, fm := range fi.Fields() {
		out := make([]map[string]any, 0, fm.CatCount)
		for j := uint32(0); j < fm.CatCount; j++ {
			c := cats[fm.CatStart+j]
			cat := map[string]any{"name": c.Name, "key": c.Key, "cardinality": c.Cardinality}
			if opt.postings {
				bm, ok, err := fi.Posting(c.Key)
				if err != nil {
					fail("%v", err)
				}
				if ok {
					docs, trunc := capDocs(bm.ToArray(), opt.docCap())
					cat["docs"] = docs
					cat["docsTruncated"] = trunc
				}
			}
			out = append(out, cat)
		}
		fields = append(fields, map[string]any{"name": fm.Name, "categories": out})
	}
	return map[string]any{"format": "RRSF", "caseSensitive": fi.CaseSensitive, "fields": fields}
}

func dumpImpacts(f *os.File, opt dumpOpts) any {
	b, err := rr.OpenImpacts(f)
	if err != nil {
		fail("%v", err)
	}
	h := b.Header()
	lo, hi := pageBounds(b.Len(), opt.offset, opt.limit)
	rows := make([]map[string]any, 0, hi-lo)
	for i := lo; i < hi; i++ {
		headOff, _, card, err := b.EntryAt(i)
		if err != nil {
			fail("%v", err)
		}
		row := map[string]any{"headOff": headOff, "card": card}
		if opt.postings {
			imp, ok, err := b.Impacts(headOff)
			if err != nil {
				fail("%v", err)
			}
			if ok {
				row["impacts"] = bytesToInts(imp)
			}
		}
		rows = append(rows, row)
	}
	return map[string]any{
		"format": "RRSB", "terms": h.TermCount, "docs": h.DocCount,
		"k1": h.K1, "b": h.B, "avgdl": h.AvgDL, "entries": rows,
	}
}

func dumpVectors(f *os.File, opt dumpOpts) any {
	vi, err := rr.OpenRRVI(f)
	if err != nil {
		fail("%v", err)
	}
	h := vi.Header()
	out := map[string]any{
		"format": "RRVI", "dim": h.Dim, "nlist": h.Nlist, "m": h.M,
		"nbits": h.Nbits, "metric": uint8(h.Metric), "vectors": h.N, "opq": h.HasOPQ,
	}
	if opt.postings {
		lo, hi := pageBounds(h.Nlist, opt.offset, opt.limit)
		clusters := make([]map[string]any, 0, hi-lo)
		for c := lo; c < hi; c++ {
			ids, _, err := vi.Cluster(c)
			if err != nil {
				fail("%v", err)
			}
			clusters = append(clusters, map[string]any{"cluster": c, "count": len(ids), "ids": ids})
		}
		out["clusters"] = clusters
	}
	return out
}

func dumpRerank(f *os.File, opt dumpOpts) any {
	s, err := rr.OpenRerank(f)
	if err != nil {
		fail("%v", err)
	}
	out := map[string]any{"format": "RRVR", "dim": s.Dim, "vectors": s.N}
	if opt.postings {
		lo, hi := pageBounds(int(s.N), opt.offset, opt.limit)
		vecs := make([][]float32, 0, hi-lo)
		for d := lo; d < hi; d++ {
			v, _, err := s.Vector(uint32(d))
			if err != nil {
				fail("%v", err)
			}
			vecs = append(vecs, v)
		}
		out["values"] = vecs
	}
	return out
}

func dumpTerms(f *os.File, opt dumpOpts) any {
	ti, err := rr.OpenTermIndex(f)
	if err != nil {
		fail("%v", err)
	}
	h := ti.Header()
	rows := make([]map[string]any, 0)
	i := 0
	lo, hi := opt.offset, opt.offset+opt.limit
	for term, headOff := range ti.Terms() {
		if i >= lo && (opt.limit <= 0 || i < hi) {
			row := map[string]any{"term": term, "headOff": headOff}
			if opt.postings {
				bm, ok, err := ti.LookupTerm(term)
				if err != nil {
					fail("%v", err)
				}
				if ok {
					docs, trunc := capDocs(bm.ToArray(), opt.docCap())
					row["docs"] = docs
					row["docsTruncated"] = trunc
				}
			}
			rows = append(rows, row)
		}
		i++
		if opt.limit > 0 && i >= hi {
			break
		}
	}
	return map[string]any{
		"format": "RRTI", "terms": h.TermCount, "language": uint8(h.Language),
		"stemmed": h.Stemmed, "stopwords": h.Stopwords, "caseSensitive": h.CaseSensitive,
		"dict": rows,
	}
}

func dumpIndex(f *os.File) any {
	idx, err := rr.Open(f)
	if err != nil {
		fail("%v", err)
	}
	return map[string]any{
		"format": "RRSI", "gramSize": idx.GramSize,
		"caseFold": idx.CaseFold, "ngrams": idx.NgramCount(),
	}
}
