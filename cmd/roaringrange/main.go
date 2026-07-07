// Command roaringrange inspects roaringrange index files: it auto-detects the
// format from the 4-byte magic and prints a header summary (info), dumps the full
// structural contents as JSON (dump), decodes a record store (records), or looks up
// a single key (get). It is a thin driver over the library's readers.
package main

import (
	"encoding/json"
	"fmt"
	"os"

	rr "github.com/freeeve/roaringrange"
)

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	args := os.Args[2:]
	switch os.Args[1] {
	case "info":
		cmdInfo(args)
	case "dump":
		cmdDump(args)
	case "records":
		cmdRecords(args)
	case "get":
		cmdGet(args)
	case "-h", "--help", "help":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "roaringrange: unknown command %q\n\n", os.Args[1])
		usage()
		os.Exit(2)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `roaringrange — inspect roaringrange index files

usage:
  roaringrange info <file> [--json]
        auto-detect the format and print a header summary
  roaringrange dump <file> [--json] [--limit N] [--offset N] [--postings]
        dump the full structural contents (JSON by default)
  roaringrange records <idx> <bin> [--dict d.dict] [--id N | --range A-B] [--json]
        decode records from an RRSR record store
  roaringrange get <file> [--key K | --term T | --id S | --head-off N] [--limit N]
        look up a single key and print its posting / value

recognized formats: RRSI RRSR RRTI RRVI RRVR RRSF RRSB RRSS RRHC RRSC RRIL
`)
}

// fail prints an error to stderr and exits non-zero.
func fail(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "roaringrange: "+format+"\n", a...)
	os.Exit(1)
}

// openFile opens path for range reads, failing fatally on error.
func openFile(path string) *os.File {
	f, err := os.Open(path)
	if err != nil {
		fail("%v", err)
	}
	return f
}

// printJSON marshals v as indented JSON to stdout.
func printJSON(v any) {
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		fail("encode json: %v", err)
	}
}

func cmdInfo(args []string) {
	fs := newFlagSet("info")
	asJSON := fs.Bool("json", false, "emit JSON")
	pos := parse(fs, args, 1, "info <file>")
	f := openFile(pos[0])
	defer f.Close()

	fi, err := rr.OpenHeader(f)
	if err != nil {
		fail("%v", err)
	}
	if *asJSON {
		printJSON(infoMap(fi))
		return
	}
	fmt.Printf("%s  %s  v%d  (%s)\n", fi.Magic, fi.Name, fi.Version, fi.Ext)
	for _, fld := range fi.Fields {
		fmt.Printf("  %-14s %v\n", fld.Key+":", fld.Val)
	}
}

// infoMap renders a FileInfo as an ordered-ish JSON object (fields nested under
// "fields", preserving the reader's field order via a slice of pairs).
func infoMap(fi *rr.FileInfo) any {
	fields := make([]map[string]any, len(fi.Fields))
	for i, f := range fi.Fields {
		fields[i] = map[string]any{f.Key: f.Val}
	}
	return map[string]any{
		"magic":   fi.Magic,
		"name":    fi.Name,
		"version": fi.Version,
		"ext":     fi.Ext,
		"fields":  fields,
	}
}
