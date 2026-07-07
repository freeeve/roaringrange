package main

import (
	"maps"
	"os"
	"strconv"

	"github.com/RoaringBitmap/roaring/v2"
	rr "github.com/freeeve/roaringrange"
)

// cmdGet looks up a single key and prints its posting or value as JSON. The lookup
// kind is chosen by which flag is set: --key (RRSI n-gram / RRSF facet), --term
// (RRTI), --id (RRIL identifier), or --head-off (RRSB impacts).
func cmdGet(args []string) {
	fs := newFlagSet("get")
	key := fs.String("key", "", "n-gram/facet key (uint64) — RRSI or RRSF")
	term := fs.String("term", "", "term — RRTI")
	id := fs.String("id", "", "identifier — RRIL")
	headOff := fs.String("head-off", "", "posting head_off (uint64) — RRSB")
	limit := fs.Int("limit", 0, "max doc ids to print (0 = all)")
	pos := parse(fs, args, 1, "get <file> [--key K | --term T | --id S | --head-off N]")

	f := openFile(pos[0])
	defer f.Close()
	format, err := rr.DetectFormat(f)
	if err != nil {
		fail("%v", err)
	}

	switch {
	case *key != "":
		printJSON(getByKey(f, format.Magic, parseU64(*key), *limit))
	case *term != "":
		printJSON(getTerm(f, *term, *limit))
	case *id != "":
		printJSON(getID(f, *id))
	case *headOff != "":
		printJSON(getHeadOff(f, parseU64(*headOff)))
	default:
		fail("get needs one of --key / --term / --id / --head-off")
	}
}

func parseU64(s string) uint64 {
	v, err := strconv.ParseUint(s, 10, 64)
	if err != nil {
		fail("bad uint64 %q: %v", s, err)
	}
	return v
}

// bitmapResult renders a bitmap as a JSON object with its cardinality and a capped
// doc-id list.
func bitmapResult(bm *roaring.Bitmap, limit int) map[string]any {
	docs := bm.ToArray()
	trunc := false
	if limit > 0 && len(docs) > limit {
		docs, trunc = docs[:limit], true
	}
	return map[string]any{"cardinality": bm.GetCardinality(), "docs": docs, "docsTruncated": trunc}
}

// getByKey looks up an n-gram key in an RRSI index or a facet key in an RRSF sidecar.
func getByKey(f *os.File, magic string, key uint64, limit int) any {
	switch magic {
	case "RRSI":
		idx, err := rr.Open(f)
		if err != nil {
			fail("%v", err)
		}
		raw, ok, err := idx.Posting(key)
		if err != nil {
			fail("%v", err)
		}
		if !ok {
			return map[string]any{"key": key, "found": false}
		}
		bm := roaring.New()
		if _, err := bm.FromBuffer(append([]byte(nil), raw...)); err != nil {
			fail("decode posting: %v", err)
		}
		return merge(map[string]any{"key": key, "found": true}, bitmapResult(bm, limit))
	case "RRSF":
		fi, err := rr.OpenFacets(f)
		if err != nil {
			fail("%v", err)
		}
		bm, ok, err := fi.Posting(key)
		if err != nil {
			fail("%v", err)
		}
		if !ok {
			return map[string]any{"key": key, "found": false}
		}
		return merge(map[string]any{"key": key, "found": true}, bitmapResult(bm, limit))
	default:
		fail("--key applies to RRSI or RRSF, not %s", magic)
		return nil
	}
}

func getTerm(f *os.File, term string, limit int) any {
	ti, err := rr.OpenTermIndex(f)
	if err != nil {
		fail("%v", err)
	}
	bm, ok, err := ti.Posting(term)
	if err != nil {
		fail("%v", err)
	}
	if !ok {
		return map[string]any{"term": term, "found": false}
	}
	return merge(map[string]any{"term": term, "found": true}, bitmapResult(bm, limit))
}

func getID(f *os.File, id string) any {
	l, err := rr.OpenLookup(f)
	if err != nil {
		fail("%v", err)
	}
	docs, err := l.Lookup(id)
	if err != nil {
		fail("%v", err)
	}
	return map[string]any{"id": id, "found": docs != nil, "docs": docs}
}

func getHeadOff(f *os.File, headOff uint64) any {
	b, err := rr.OpenImpacts(f)
	if err != nil {
		fail("%v", err)
	}
	imp, ok, err := b.Impacts(headOff)
	if err != nil {
		fail("%v", err)
	}
	return map[string]any{"headOff": headOff, "found": ok, "card": len(imp), "impacts": bytesToInts(imp)}
}

// merge copies b's keys into a and returns a.
func merge(a, b map[string]any) map[string]any {
	maps.Copy(a, b)
	return a
}
