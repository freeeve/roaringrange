package main

import (
	"flag"
	"fmt"
	"os"
)

// newFlagSet builds a subcommand flag set that prints a usage hint on error.
func newFlagSet(name string) *flag.FlagSet {
	fs := flag.NewFlagSet(name, flag.ExitOnError)
	return fs
}

// parse parses args allowing flags and positional arguments in any order (the
// stdlib flag package otherwise stops at the first positional), requires at least
// min positionals, and returns them. It fails with the usage string otherwise.
func parse(fs *flag.FlagSet, args []string, min int, usageLine string) []string {
	var pos []string
	rest := args
	for len(rest) > 0 {
		if err := fs.Parse(rest); err != nil {
			os.Exit(2)
		}
		if fs.NArg() == 0 {
			break
		}
		pos = append(pos, fs.Arg(0))
		rest = fs.Args()[1:]
	}
	if len(pos) < min {
		fmt.Fprintf(os.Stderr, "roaringrange: usage: roaringrange %s\n", usageLine)
		os.Exit(2)
	}
	return pos
}

// bytesToInts widens a byte slice to []int so JSON renders it as a number array
// rather than base64 (the impact bytes are more useful as numbers).
func bytesToInts(b []byte) []int {
	out := make([]int, len(b))
	for i, x := range b {
		out[i] = int(x)
	}
	return out
}

// pageBounds clamps a [offset, offset+limit) window to [0, n). A limit of 0 means
// "to the end". Returns the half-open [lo, hi).
func pageBounds(n, offset, limit int) (lo, hi int) {
	if offset < 0 {
		offset = 0
	}
	lo = min(offset, n)
	if limit <= 0 {
		return lo, n
	}
	return lo, min(lo+limit, n)
}
