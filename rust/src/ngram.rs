//! N-gram key derivation, ported byte-for-byte from roaringsearch `ngram.go`.
//!
//! The reader must reproduce the builder's tokenizer exactly so a query resolves
//! to the same dictionary keys. See `FORMAT.md` for the frozen contract.

use unicode_general_category::{get_general_category, GeneralCategory};

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET64: u64 = 14695981039346656037;
/// FNV-1a 64-bit prime.
const FNV_PRIME64: u64 = 1099511628211;

/// Derives the deduplicated n-gram keys for a query.
///
/// Splits the query on whitespace and keys each word independently (normalize:
/// keep Unicode letters/digits, lowercase; then each `gram_size`-char window),
/// unioning keys in first-seen order. Per-word keying means a multi-word query
/// never manufactures cross-word boundary trigrams — e.g. `"legends travis"`
/// must not require `"dst"` from `legend·s·t·ravis`. Mirrors roaringsearch's
/// per-word query matching. Empty when `gram_size` is zero or no word reaches it.
pub fn ngram_keys(query: &str, gram_size: usize) -> Vec<u64> {
    if gram_size == 0 {
        return Vec::new();
    }
    let mut keys = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for field in query.split_whitespace() {
        let runes = normalize(field);
        if runes.len() < gram_size {
            continue;
        }
        for window in runes.windows(gram_size) {
            let k = rune_ngram_key(window);
            if seen.insert(k) {
                keys.push(k);
            }
        }
    }
    keys
}

/// Keeps Unicode letters/digits and lowercases each char.
///
/// Mirrors Go's `unicode.IsLetter || unicode.IsDigit` filter and
/// `unicode.ToLower`. Go lowercases a single rune to a single rune, so we take
/// the first char of Rust's (potentially multi-char) lowercase mapping, which
/// matches for every case where Go's simple mapping is one rune.
fn normalize(s: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if is_letter_or_digit(c) {
            out.push(to_lower_rune(c));
        }
    }
    out
}

/// Reports whether `c` is a Unicode letter (general category L*) or decimal
/// digit (Nd), matching Go's `unicode.IsLetter || unicode.IsDigit`. Std's
/// `is_alphabetic`/`is_numeric` are broader — they also keep letter-numbers and
/// other-numbers (Nl/No, e.g. Ⅰ, ², ①) that the Go builder drops — so we test
/// the general category directly to stay byte-compatible with the builder.
fn is_letter_or_digit(c: char) -> bool {
    use GeneralCategory::{
        DecimalNumber, LowercaseLetter, ModifierLetter, OtherLetter, TitlecaseLetter,
        UppercaseLetter,
    };
    matches!(
        get_general_category(c),
        UppercaseLetter
            | LowercaseLetter
            | TitlecaseLetter
            | ModifierLetter
            | OtherLetter
            | DecimalNumber
    )
}

/// Lowercases a single char to a single char, matching Go's `unicode.ToLower`
/// simple (one-rune) case mapping.
fn to_lower_rune(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Computes the key for one window: 32-bit packing for `n <= 2`, 8-bit packing
/// for ASCII `n` in 3..=8, FNV-1a otherwise.
fn rune_ngram_key(runes: &[char]) -> u64 {
    let n = runes.len();
    if n <= 2 {
        let mut key: u64 = 0;
        for &r in runes {
            key = (key << 32) | (r as u64);
        }
        return key;
    }
    if n <= 8 {
        let mut key: u64 = 0;
        for &r in runes {
            if (r as u32) > 127 {
                return hash_runes(runes);
            }
            key = (key << 8) | (r as u64);
        }
        return key;
    }
    hash_runes(runes)
}

/// FNV-1a over each rune's 4 little-endian bytes.
fn hash_runes(runes: &[char]) -> u64 {
    let mut h = FNV_OFFSET64;
    for &c in runes {
        let r = c as u32;
        h ^= (r & 0xFF) as u64;
        h = h.wrapping_mul(FNV_PRIME64);
        h ^= ((r >> 8) & 0xFF) as u64;
        h = h.wrapping_mul(FNV_PRIME64);
        h ^= ((r >> 16) & 0xFF) as u64;
        h = h.wrapping_mul(FNV_PRIME64);
        h ^= ((r >> 24) & 0xFF) as u64;
        h = h.wrapping_mul(FNV_PRIME64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_trigram_packing_and_dedup() {
        // "abc" -> ('a'<<16)|('b'<<8)|'c' = 6382179
        assert_eq!(ngram_keys("abc", 3), vec![6382179]);
        // normalization drops punctuation/case: "A-b!C" == "abc"
        assert_eq!(ngram_keys("A-b!C", 3), vec![6382179]);
        // dedup: "aaaa" yields a single distinct trigram
        assert_eq!(ngram_keys("aaaa", 3).len(), 1);
        // too short for a trigram
        assert!(ngram_keys("ab", 3).is_empty());
    }

    #[test]
    fn splits_on_whitespace_no_boundary_trigrams() {
        // Each word keyed separately: "abc def" -> trigrams of "abc" and "def".
        let key = |s: &str| rune_ngram_key(&s.chars().collect::<Vec<_>>());
        assert_eq!(ngram_keys("abc def", 3), vec![key("abc"), key("def")]);
        // No cross-word trigram ("cde" from abc·def) is produced.
        assert!(!ngram_keys("abc def", 3).contains(&key("cde")));
        // Punctuation within a word is still stripped: "A-b!C" stays one word.
        assert_eq!(ngram_keys("A-b!C", 3), vec![6382179]);
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        // "abcabc": windows abc, bca, cab, abc(dup) -> [abc, bca, cab]
        let keys = ngram_keys("abcabc", 3);
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], 6382179); // "abc"
    }

    #[test]
    fn empty_and_short_inputs() {
        assert!(ngram_keys("", 3).is_empty());
        assert!(ngram_keys("a", 3).is_empty());
        assert!(ngram_keys("!!!", 3).is_empty()); // all punctuation -> normalized empty
        assert!(ngram_keys("abc", 0).is_empty());
    }

    #[test]
    fn bigram_uses_32_bit_packing() {
        // n=2: key = ('a'<<32)|'b' = 97<<32 | 98
        let keys = ngram_keys("ab", 2);
        assert_eq!(keys, vec![(97u64 << 32) | 98]);
    }

    #[test]
    fn non_ascii_window_uses_fnv() {
        // A trigram containing a non-ASCII rune must hash, not pack.
        let keys = ngram_keys("aé9", 3);
        assert_eq!(keys.len(), 1);
        let expected = {
            let runes: Vec<char> = "aé9".chars().collect();
            super::hash_runes(&runes)
        };
        assert_eq!(keys[0], expected);
        // and it must differ from the 8-bit packing of the ASCII bytes.
        assert_ne!(keys[0], (b'a' as u64) << 16);
    }

    #[test]
    fn drops_letter_numbers_and_other_numbers_like_go() {
        // The builder filters on Go's `IsLetter || IsDigit` (categories L* + Nd).
        // Std's `is_alphabetic`/`is_numeric` would also keep letter-numbers (Nl,
        // e.g. Ⅰ U+2160) and other-numbers (No, e.g. ² U+00B2, ① U+2460); the
        // reader must drop them to tokenize identically to the builder.
        assert!(normalize("Ⅰ②①").is_empty());
        // Ⅰ dropped -> "ab" (too short); ² and ① dropped -> "abc".
        assert!(ngram_keys("Ⅰab", 3).is_empty());
        assert_eq!(ngram_keys("a²b①c", 3), vec![6382179]);
        // Letters (any L*) and decimal digits (Nd) are kept and lowercased.
        assert_eq!(normalize("Ab9"), vec!['a', 'b', '9']);
    }
}
