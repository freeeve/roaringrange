package main

import (
	"os"

	rr "github.com/freeeve/roaringrange"
)

// cmdDump dumps the full structural contents of a file as JSON, auto-detecting the
// format. Large tables are paged with --offset/--limit; --postings includes bitmap
// and vector contents (which are elided by default).
func cmdDump(args []string) {
	fs := newFlagSet("dump")
	// --json is accepted for symmetry; dump always emits JSON.
	fs.Bool("json", true, "emit JSON (always on for dump)")
	limit := fs.Int("limit", 0, "max table rows to emit (0 = all)")
	offset := fs.Int("offset", 0, "table row to start from")
	postings := fs.Bool("postings", false, "include bitmap/vector contents")
	pos := parse(fs, args, 1, "dump <file> [--limit N] [--offset N] [--postings]")

	f := openFile(pos[0])
	defer f.Close()

	format, err := rr.DetectFormat(f)
	if err != nil {
		fail("%v", err)
	}
	opt := dumpOpts{limit: *limit, offset: *offset, postings: *postings}
	printJSON(dumpByFormat(f, format.Magic, opt))
}

// dumpByFormat routes to the per-format dumper for magic.
func dumpByFormat(f *os.File, magic string, opt dumpOpts) any {
	switch magic {
	case "RRIL":
		return dumpLookup(f, opt)
	case "RRSC":
		return dumpSortcols(f, opt)
	case "RRHC":
		return dumpHotcache(f)
	case "RRSS":
		return dumpSplitSet(f)
	case "RRSF":
		return dumpFacets(f, opt)
	case "RRSB":
		return dumpImpacts(f, opt)
	case "RRVI":
		return dumpVectors(f, opt)
	case "RRVR":
		return dumpRerank(f, opt)
	case "RRTI":
		return dumpTerms(f, opt)
	case "RRSI":
		return dumpIndex(f)
	case "RRSR":
		fail("RRSR is a record store — use `roaringrange records <idx> <bin>`")
	default:
		fail("no dumper for format %s", magic)
	}
	return nil
}

// dumpOpts carries the paging/postings flags through the per-format dumpers.
type dumpOpts struct {
	limit, offset int
	postings      bool
}

// docCap bounds how many doc IDs a posting dump emits when --postings is set.
func (o dumpOpts) docCap() int {
	if o.limit > 0 {
		return o.limit
	}
	return 1000
}

// capDocs returns docs truncated to n, and whether it was truncated.
func capDocs(docs []uint32, n int) ([]uint32, bool) {
	if len(docs) > n {
		return docs[:n], true
	}
	return docs, false
}
