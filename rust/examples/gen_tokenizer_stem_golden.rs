//! Generates the per-language tokenizer/stemmer conformance golden for the Go term
//! tokenizer. For each Snowball language byte (1..=18) it tokenizes a
//! representative inflected word through the Rust `Tokenizer::with(Some(lang), stem=true,
//! stopwords=false, case_fold=true)` and prints one TSV line
//!
//!   <lang_byte>\t<input>\t<tok0> <tok1> ...
//!
//! saved to `go/testdata/tokenizer_stem_golden.txt` and asserted by `go/terms_stem_test.go`
//! (`TestTokenizerStemMatchesRustGolden`), which builds the Go `NewTermTokenizerFull` for the
//! same byte and must reproduce the tokens byte-for-byte--proving the Go TermLanguage ->
//! go-stemmers mapping matches the Rust Language -> rust-stemmers mapping for every language.
//!
//!   cargo run --release --features terms --example gen_tokenizer_stem_golden
//!
//! The words are chosen to stem non-trivially so a wrong language mapping is caught; the Rust
//! output is authoritative regardless.

use roaringrange::{Language, Tokenizer};

/// (on-disk language byte, representative inflected word)--one per Snowball language.
const CASES: [(u8, &str); 18] = [
    (1, "running"),     // English
    (2, "corriendo"),   // Spanish
    (3, "الكتاب"),      // Arabic (the book)
    (4, "løbende"),     // Danish
    (5, "lopende"),     // Dutch
    (6, "juokseminen"), // Finnish
    (7, "chats"),       // French
    (8, "laufen"),      // German
    (9, "τρέχοντας"),   // Greek
    (10, "futásokban"), // Hungarian
    (11, "ragazzi"),    // Italian (boys)
    (12, "løpende"),    // Norwegian
    (13, "meninas"),    // Portuguese (girls)
    (14, "alergând"),   // Romanian
    (15, "бегущий"),    // Russian
    (16, "springande"), // Swedish
    (17, "ஓடுகிறது"),   // Tamil
    (18, "koşuyorlar"), // Turkish
];

fn main() {
    for (byte, word) in CASES {
        let lang = Language::from_u8(byte).expect("known language byte");
        let toks = Tokenizer::with(Some(lang), true, false, true).tokenize(word);
        println!("{byte}\t{word}\t{}", toks.join(" "));
    }
}
