package roaringrange

import (
	"strings"
	"unicode"
)

// NgramKeys derives the deduplicated n-gram keys for a query. The query is split
// on whitespace and each word is keyed independently (normalize: keep Unicode
// letters/digits, lowercase; then key each gramSize-rune window), unioning keys
// in first-seen order. Per-word keying avoids cross-word boundary trigrams — a
// query like "legends travis" must not require "dst" from legend·s·t·ravis.
// Mirrors roaringsearch's per-word query matching. See FORMAT.md.
//
// The key derivation here must stay byte-compatible with roaringsearch's builder
// (and the Rust reader's port); the cross-library test in ./conformance is the
// guard that enforces it.
func NgramKeys(query string, gramSize int) []uint64 {
	return NgramKeysWith(query, gramSize, true)
}

// NgramKeysWith is NgramKeys with an explicit case-fold flag. When caseFold is false
// the kept letters/digits are not lowercased, so a case-sensitive trigram index keys on
// the original case. The builder and reader must agree; the choice is recorded in the RRSI
// header (a v4 flags field) so the reader derives the same caseFold. See FORMAT.md.
func NgramKeysWith(query string, gramSize int, caseFold bool) []uint64 {
	// A fresh keyer per call reproduces the old allocate-per-call behavior exactly;
	// its buffer backs the returned slice, so it stays valid after the keyer is dropped.
	var k NgramKeyer
	return k.Keys(query, gramSize, caseFold)
}

// NgramKeyer derives n-gram keys while reusing its scratch buffers across calls, for
// the per-document ingest hot path where NgramKeysWith's fresh map + rune slice per
// call cost hundreds of millions of allocations over a full 484M-doc build. Not safe
// for concurrent use; hold one per builder.
type NgramKeyer struct {
	seen  map[uint64]struct{}
	runes []rune
	keys  []uint64
}

// Keys returns the deduplicated n-gram keys for query, byte-identical to
// NgramKeysWith but reusing internal buffers. The returned slice aliases the keyer's
// buffer and is valid only until the next Keys call -- copy it if it must outlive that.
func (k *NgramKeyer) Keys(query string, gramSize int, caseFold bool) []uint64 {
	k.keys = k.keys[:0]
	if gramSize <= 0 {
		return k.keys
	}
	if k.seen == nil {
		k.seen = make(map[uint64]struct{})
	}
	clear(k.seen)
	// FieldsSeq iterates whitespace-split fields without allocating a []string.
	for field := range strings.FieldsSeq(query) {
		k.runes = normalizeInto(k.runes[:0], field, caseFold)
		if len(k.runes) < gramSize {
			continue
		}
		for i := 0; i+gramSize <= len(k.runes); i++ {
			key := runeNgramKey(k.runes[i : i+gramSize])
			if _, ok := k.seen[key]; ok {
				continue
			}
			k.seen[key] = struct{}{}
			k.keys = append(k.keys, key)
		}
	}
	return k.keys
}

// normalizeInto appends the kept (letter/digit, optionally lowercased) runes of s to
// out and returns the extended slice, so a caller can reuse one buffer across words.
func normalizeInto(out []rune, s string, caseFold bool) []rune {
	for _, r := range s {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			if caseFold {
				out = append(out, unicode.ToLower(r))
			} else {
				out = append(out, r)
			}
		}
	}
	return out
}

// runeNgramKey mirrors roaringsearch ngram.go: 32-bit packing for n<=2,
// 8-bit packing for ASCII n in 3..8, FNV-1a hash otherwise.
func runeNgramKey(runes []rune) uint64 {
	n := len(runes)
	if n <= 2 {
		var key uint64
		for _, r := range runes {
			key = (key << 32) | uint64(r)
		}
		return key
	}
	if n <= 8 {
		var key uint64
		for _, r := range runes {
			if r > 127 {
				return hashRunes(runes)
			}
			key = (key << 8) | uint64(r)
		}
		return key
	}
	return hashRunes(runes)
}

// hashRunes is FNV-1a over each rune's 4 little-endian bytes.
func hashRunes(runes []rune) uint64 {
	const (
		offset64 = 14695981039346656037
		prime64  = 1099511628211
	)
	h := uint64(offset64)
	for _, r := range runes {
		h ^= uint64(r & 0xFF)
		h *= prime64
		h ^= uint64((r >> 8) & 0xFF)
		h *= prime64
		h ^= uint64((r >> 16) & 0xFF)
		h *= prime64
		h ^= uint64((r >> 24) & 0xFF)
		h *= prime64
	}
	return h
}
