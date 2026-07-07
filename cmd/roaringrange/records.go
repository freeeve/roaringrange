package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	rr "github.com/freeeve/roaringrange"
)

// cmdRecords decodes records from an RRSR record store (a separate offset index and
// blob file, optionally with a zstd .dict). --id prints one record's raw bytes;
// --range prints a JSON array of records; with neither it prints the record count.
func cmdRecords(args []string) {
	fs := newFlagSet("records")
	dict := fs.String("dict", "", "zstd dictionary sidecar for a framed store")
	id := fs.Int("id", -1, "print a single record by doc id")
	rng := fs.String("range", "", "print records in the inclusive range A-B")
	asJSON := fs.Bool("json", false, "emit JSON")
	pos := parse(fs, args, 2, "records <idx> <bin> [--dict d.dict] [--id N | --range A-B]")

	idxF := openFile(pos[0])
	defer idxF.Close()
	binF := openFile(pos[1])
	defer binF.Close()

	store := openStore(idxF, binF, *dict)

	switch {
	case *id >= 0:
		data, ok, err := store.Get(uint32(*id))
		if err != nil {
			fail("%v", err)
		}
		if !ok {
			fail("record %d out of range (count %d)", *id, store.Len())
		}
		os.Stdout.Write(data)
		fmt.Println()
	case *rng != "":
		lo, hi := parseRange(*rng)
		recs := make([]map[string]any, 0, hi-lo+1)
		for d := lo; d <= hi && uint32(d) < store.Len(); d++ {
			data, ok, err := store.Get(uint32(d))
			if err != nil {
				fail("%v", err)
			}
			if ok {
				recs = append(recs, map[string]any{"id": d, "record": string(data)})
			}
		}
		if *asJSON {
			printJSON(recs)
		} else {
			for _, r := range recs {
				fmt.Printf("== id %v ==\n%s\n", r["id"], r["record"])
			}
		}
	default:
		if *asJSON {
			printJSON(map[string]any{"format": "RRSR", "count": store.Len()})
		} else {
			fmt.Printf("RRSR record store: %d records\n", store.Len())
		}
	}
}

// openStore opens the record store, attaching a zstd dictionary when dictPath is set
// (required for a framed/compressed store).
func openStore(idx, bin *os.File, dictPath string) *rr.RecordStore {
	if dictPath == "" {
		store, err := rr.OpenRecordStore(idx, bin)
		if err != nil {
			fail("%v", err)
		}
		return store
	}
	dictBytes, err := os.ReadFile(dictPath)
	if err != nil {
		fail("read dict: %v", err)
	}
	store, err := rr.OpenRecordStoreWithDict(idx, bin, dictBytes)
	if err != nil {
		fail("%v", err)
	}
	return store
}

// parseRange parses an inclusive "A-B" doc-id range.
func parseRange(s string) (lo, hi int) {
	a, b, ok := strings.Cut(s, "-")
	if !ok {
		fail("bad range %q, want A-B", s)
	}
	lo, err := strconv.Atoi(strings.TrimSpace(a))
	if err != nil {
		fail("bad range start: %v", err)
	}
	hi, err = strconv.Atoi(strings.TrimSpace(b))
	if err != nil {
		fail("bad range end: %v", err)
	}
	if hi < lo {
		fail("range end %d before start %d", hi, lo)
	}
	return lo, hi
}
