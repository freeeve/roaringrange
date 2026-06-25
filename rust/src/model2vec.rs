//! In-browser model2vec query embedder (vector-search mode 2): tokenize → gather
//! static token embeddings → mean-pool → L2-normalize, with **no backend**. The
//! same recipe model2vec runs in Python, reimplemented to be byte-compatible (the
//! tokenizer is BERT WordPiece, which is small enough to port). Reads the `RRM2`
//! artifact emitted by `python/scripts/model2vec_export.py` (vocab + an
//! int8-per-row quantized embedding matrix + the BertNormalizer flags).
//!
//! Behind the `vector` feature; wasm-safe. Pairs with [`crate::vector::VectorIndex`]:
//! `index.search(model2vec.embed(query), k, nprobe)`.

use crate::index::{read_u16, read_u32, IndexError};
use std::collections::HashMap;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

/// `RRM2` magic.
const MAGIC: &[u8; 4] = b"RRM2";
/// Fixed header size in bytes.
const HEADER_SIZE: usize = 32;
/// WordPiece continuing-subword prefix (the BERT standard).
const CONT_PREFIX: &str = "##";
/// WordPiece word-length cap (BERT's `max_input_chars_per_word`).
const MAX_WORD_CHARS: usize = 100;

/// A static model2vec embedder: a WordPiece vocab + an int8-quantized
/// token-embedding matrix + the normalizer flags, all held in memory.
pub struct Model2vec {
    dim: usize,
    unk_id: u32,
    lowercase: bool,
    strip_accents: bool,
    handle_chinese: bool,
    clean_text: bool,
    /// Token string → id (for WordPiece lookup).
    vocab: HashMap<String, u32>,
    /// Per-row dequant scale, `vocab` entries.
    scales: Vec<f32>,
    /// int8 codes, `vocab × dim` row-major (`row = code * scale`).
    codes: Vec<i8>,
}

impl Model2vec {
    /// Parses an `RRM2` artifact (the whole file, downloaded once).
    pub fn from_bytes(b: &[u8]) -> Result<Self, IndexError> {
        if b.len() < HEADER_SIZE || &b[0..4] != MAGIC {
            return Err(IndexError::Malformed("RRM2 bad magic/size"));
        }
        let version = read_u16(b, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let dim = read_u32(b, 6) as usize;
        let vocab_size = read_u32(b, 10) as usize;
        let quant = b[14];
        let flags = b[15];
        let unk_id = read_u32(b, 16);
        if dim == 0 || vocab_size == 0 || quant != 0 {
            return Err(IndexError::Malformed("RRM2 dim/vocab/quant invalid"));
        }
        // unk_id indexes scales/codes unchecked on every OOV token, so an
        // out-of-range value must fail here at open — not as a panic (a wasm
        // abort killing the page's reader) on the first unmatched word.
        if unk_id as usize >= vocab_size {
            return Err(IndexError::Malformed("RRM2 unk_id out of vocab range"));
        }

        // `vocab_size`/`dim` are header fields; compute the section offsets with
        // checked arithmetic so a crafted header overflows to an error instead of
        // wrapping past the `b.len()` bound below (which would then slice — and
        // `HashMap::with_capacity(vocab_size)` allocate — out of bounds).
        let scales_off = HEADER_SIZE;
        let codes_off = vocab_size
            .checked_mul(4)
            .and_then(|x| x.checked_add(scales_off))
            .ok_or(IndexError::Malformed("RRM2 size overflow"))?;
        let vocab_off = vocab_size
            .checked_mul(dim)
            .and_then(|x| x.checked_add(codes_off))
            .ok_or(IndexError::Malformed("RRM2 size overflow"))?;
        if b.len() < vocab_off {
            return Err(IndexError::Malformed("RRM2 truncated"));
        }
        let scales: Vec<f32> = b[scales_off..codes_off]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let codes: Vec<i8> = b[codes_off..vocab_off].iter().map(|&x| x as i8).collect();

        // Vocab strings in id order: u16 len + UTF-8 bytes.
        let mut vocab = HashMap::with_capacity(vocab_size);
        let mut off = vocab_off;
        for id in 0..vocab_size {
            if off + 2 > b.len() {
                return Err(IndexError::Malformed("RRM2 vocab truncated"));
            }
            let len = read_u16(b, off) as usize;
            off += 2;
            if off + len > b.len() {
                return Err(IndexError::Malformed("RRM2 vocab string truncated"));
            }
            let tok = std::str::from_utf8(&b[off..off + len])
                .map_err(|_| IndexError::Malformed("RRM2 vocab not UTF-8"))?;
            vocab.insert(tok.to_string(), id as u32);
            off += len;
        }

        Ok(Self {
            dim,
            unk_id,
            lowercase: flags & 1 != 0,
            strip_accents: flags & 2 != 0,
            handle_chinese: flags & 4 != 0,
            clean_text: flags & 8 != 0,
            vocab,
            scales,
            codes,
        })
    }

    /// Embedding dimensionality.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embeds `text`: normalize → WordPiece → mean-pool the token vectors →
    /// L2-normalize. An empty/all-OOV input yields a zero vector.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let ids = self.tokenize(text);
        let mut acc = vec![0f32; self.dim];
        if ids.is_empty() {
            return acc;
        }
        for &id in &ids {
            let base = id as usize * self.dim;
            let scale = self.scales[id as usize];
            for (a, &c) in acc.iter_mut().zip(&self.codes[base..base + self.dim]) {
                *a += c as f32 * scale;
            }
        }
        let inv = 1.0 / ids.len() as f32;
        let mut norm = 0f32;
        for a in &mut acc {
            *a *= inv;
            norm += *a * *a;
        }
        let norm = norm.sqrt();
        if norm > 0.0 {
            for a in &mut acc {
                *a /= norm;
            }
        }
        acc
    }

    /// BertNormalizer → BertPreTokenizer → WordPiece, returning token ids (no
    /// special tokens, matching model2vec's pooling).
    fn tokenize(&self, text: &str) -> Vec<u32> {
        let norm = self.normalize(text);
        let mut ids = Vec::new();
        let mut word = String::new();
        let flush = |word: &mut String, me: &Self, ids: &mut Vec<u32>| {
            if !word.is_empty() {
                me.wordpiece(word, ids);
                word.clear();
            }
        };
        for ch in norm.chars() {
            if ch == ' ' || ch.is_whitespace() {
                flush(&mut word, self, &mut ids);
            } else if is_punctuation(ch) {
                flush(&mut word, self, &mut ids);
                self.wordpiece(&ch.to_string(), &mut ids); // punctuation is its own token
            } else {
                word.push(ch);
            }
        }
        flush(&mut word, self, &mut ids);
        ids
    }

    /// BertNormalizer: clean text, isolate CJK, strip accents (NFD − Mn),
    /// lowercase — each gated by the model's flags.
    fn normalize(&self, text: &str) -> String {
        let mut s: String = if self.clean_text {
            let mut out = String::with_capacity(text.len());
            for ch in text.chars() {
                let cp = ch as u32;
                if cp == 0 || cp == 0xFFFD || is_control(ch) {
                    continue;
                }
                out.push(if is_bert_whitespace(ch) { ' ' } else { ch });
            }
            out
        } else {
            text.to_string()
        };
        if self.handle_chinese {
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                if is_chinese(ch) {
                    out.push(' ');
                    out.push(ch);
                    out.push(' ');
                } else {
                    out.push(ch);
                }
            }
            s = out;
        }
        if self.strip_accents {
            s = s
                .nfd()
                .filter(|c| get_general_category(*c) != GeneralCategory::NonspacingMark)
                .collect();
        }
        if self.lowercase {
            s = s.chars().flat_map(|c| c.to_lowercase()).collect();
        }
        s
    }

    /// Greedy longest-match-first WordPiece for one word, appending ids to `out`.
    /// If any piece is unmatched, the whole word becomes `[UNK]` (BERT behavior).
    fn wordpiece(&self, word: &str, out: &mut Vec<u32>) {
        let chars: Vec<char> = word.chars().collect();
        if chars.is_empty() {
            return;
        }
        if chars.len() > MAX_WORD_CHARS {
            out.push(self.unk_id);
            return;
        }
        let mut sub = Vec::new();
        let mut start = 0;
        let n = chars.len();
        while start < n {
            let mut end = n;
            let mut found = None;
            while start < end {
                let mut piece: String = chars[start..end].iter().collect();
                if start > 0 {
                    piece.insert_str(0, CONT_PREFIX);
                }
                if let Some(&id) = self.vocab.get(&piece) {
                    found = Some(id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => {
                    sub.push(id);
                    start = end;
                }
                None => {
                    out.push(self.unk_id);
                    return;
                }
            }
        }
        out.extend(sub);
    }
}

/// BERT control-char test: any `C*` general category, except `\t \n \r`.
fn is_control(ch: char) -> bool {
    if ch == '\t' || ch == '\n' || ch == '\r' {
        return false;
    }
    matches!(
        get_general_category(ch),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
    )
}

/// BERT whitespace test: ASCII spaces/tabs/newlines or any `Zs`.
fn is_bert_whitespace(ch: char) -> bool {
    ch == ' '
        || ch == '\t'
        || ch == '\n'
        || ch == '\r'
        || get_general_category(ch) == GeneralCategory::SpaceSeparator
}

/// BERT punctuation test: ASCII punctuation ranges or any `P*` category.
fn is_punctuation(ch: char) -> bool {
    let cp = ch as u32;
    if (33..=47).contains(&cp)
        || (58..=64).contains(&cp)
        || (91..=96).contains(&cp)
        || (123..=126).contains(&cp)
    {
        return true;
    }
    matches!(
        get_general_category(ch),
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::OtherPunctuation
    )
}

/// BERT CJK test (the ranges `_is_chinese_char` covers).
fn is_chinese(ch: char) -> bool {
    let cp = ch as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0x2A700..=0x2B73F).contains(&cp)
        || (0x2B740..=0x2B81F).contains(&cp)
        || (0x2B820..=0x2CEAF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-builds a tiny `RRM2` (dim 2, lowercase-only) so the reader, tokenizer,
    /// and pooling are exercised without the 33 MB real artifact.
    fn tiny() -> Vec<u8> {
        let toks = ["[UNK]", "hello", "world", "!", "##ing"];
        // 2-D int8 rows; scale 1.0 so a row's value == its code.
        let codes: [[i8; 2]; 5] = [[10, 10], [127, 0], [0, 127], [40, 40], [-127, 0]];
        let dim = 2u32;
        let flags = 1u8; // lowercase only
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&1u16.to_le_bytes()); // version
        b.extend_from_slice(&dim.to_le_bytes());
        b.extend_from_slice(&(toks.len() as u32).to_le_bytes());
        b.push(0); // quant int8
        b.push(flags);
        b.extend_from_slice(&0u32.to_le_bytes()); // unk_id
        b.resize(HEADER_SIZE, 0);
        for _ in 0..toks.len() {
            b.extend_from_slice(&1.0f32.to_le_bytes()); // scales
        }
        for row in &codes {
            b.extend_from_slice(&[row[0] as u8, row[1] as u8]);
        }
        for t in toks {
            b.extend_from_slice(&(t.len() as u16).to_le_bytes());
            b.extend_from_slice(t.as_bytes());
        }
        b
    }

    fn norm2(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn embeds_and_tokenizes() {
        let m = Model2vec::from_bytes(&tiny()).unwrap();
        assert_eq!(m.dim(), 2);

        // "Hello World" -> lowercase -> [hello=[1,0], world=[0,1]] -> mean -> unit.
        let v = m.embed("Hello World");
        assert!((norm2(&v) - 1.0).abs() < 1e-5);
        assert!(
            (v[0] - v[1]).abs() < 1e-4,
            "expected ~[.707,.707], got {v:?}"
        );

        // single known token -> its (normalized) direction.
        let h = m.embed("hello");
        assert!(h[0] > 0.99 && h[1].abs() < 1e-4);

        // punctuation splits into its own token: "world!" -> [world, !].
        let w = m.embed("world!");
        // mean([0,127],[40,40]) = [20,83.5]; second component dominates.
        assert!(w[1] > w[0]);

        // OOV word -> [UNK] (id 0 = [10,10]) -> ~[.707,.707].
        let o = m.embed("zzz");
        assert!((o[0] - o[1]).abs() < 1e-4);

        // continuing subword: "testing" has no "test"... but "##ing" exists and
        // "hello"/"world" don't prefix it -> whole word OOV -> [UNK].
        let _ = m.embed("testing");
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(
            Model2vec::from_bytes(b"NOPE................................"),
            Err(IndexError::Malformed(_) | IndexError::BadVersion(_))
        ));
    }

    #[test]
    fn rejects_out_of_range_unk_id() {
        // unk_id is dereferenced unchecked on every OOV token; a malformed
        // artifact must fail at open, not panic in embed().
        let mut b = tiny();
        let bad_unk = 5u32; // == vocab_size, one past the last id
        b[16..20].copy_from_slice(&bad_unk.to_le_bytes());
        assert!(matches!(
            Model2vec::from_bytes(&b),
            Err(IndexError::Malformed(_))
        ));
    }
}
