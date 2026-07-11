//! L2 test harness: a lexeme tokenizer and per-database schema loader shared by
//! the L2 soundness and precision lanes.
//!
//! The shipped core has no model tokenizer (a host supplies one). To exercise the
//! schema overlay the way a host would — narrowing a *token* mask at each step —
//! these lanes build a per-database vocabulary at **lexeme granularity**: each
//! identifier/classpath, string/number/date literal, and operator is one token,
//! exactly the granularity [`purecard::schema`]'s scope machine classifies. The
//! tokenizer partitions a gold `pure_text` byte-for-byte (no byte is dropped), so
//! concatenating the tokens reproduces the gold exactly — a stream M1 already
//! proves streams soundly through the byte-PDA.
//!
//! Shared via `#[path]` by both L2 lanes; each uses a different subset of the
//! helpers, so module-level `dead_code` is allowed here.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use purecard::{Schema, Vocab};

/// The database ids that have a committed schema fixture (`tests/fixtures/schemas`).
/// The five arm-C pilot contexts plus the three out-of-sample (OOS) held-out
/// schemas — 269 in-scope gold queries in total (256 arm-A / 13 arm-C).
pub const FIXTURE_DBS: &[&str] = &[
    "battle_death",
    "car_1",
    "concert_singer",
    "employee_hire_evaluation",
    "pets_1",
    "dog_kennels",
    "student_transcripts_tracking",
    "world_1",
];

/// The three OOS held-out schemas — never used to author a rule, so passing them
/// proves the overlay generalizes to unseen schemas (§8.3, G5).
pub const OOS_DBS: &[&str] = &["dog_kennels", "student_transcripts_tracking", "world_1"];

/// Load the committed JSON schema fixture for `db_id` and parse it through the
/// shipped `Schema::from_json` (so the §6.3 ingress is itself under test).
#[must_use]
pub fn load_schema(db_id: &str) -> Schema {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/schemas")
        .join(format!("{db_id}.json"));
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read schema fixture {}: {err}", path.display()));
    Schema::from_json(&json).unwrap_or_else(|err| panic!("parse schema fixture {db_id}: {err}"))
}

/// Split `text` into lexeme tokens (as owned byte strings), partitioning it
/// exactly — the concatenation of the returned tokens equals `text`'s bytes.
#[must_use]
pub fn lex(text: &str) -> Vec<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        } else if two_byte_op(bytes, i) {
            i += 2;
        } else if b == b'\'' {
            i = scan_string(bytes, i);
        } else if b == b'%' {
            i += 1;
            while i < bytes.len() && is_date_char(bytes[i]) {
                i += 1;
            }
        } else if b.is_ascii_digit() || (b == b'-' && next_is_digit(bytes, i)) {
            i = scan_number(bytes, i);
        } else if is_ident_start(b) {
            i = scan_ident(bytes, i);
        } else {
            // Any single structural byte: `. $ | , ; : ( ) [ ] { } ! + - * / = & < >`.
            i += 1;
        }
        tokens.push(bytes[start..i].to_vec());
    }
    tokens
}

/// Whether a two-byte operator starts at `i` (`-> :: == != <= >= && ||`).
fn two_byte_op(bytes: &[u8], i: usize) -> bool {
    matches!(
        (bytes.get(i), bytes.get(i + 1)),
        (Some(b'-'), Some(b'>'))
            | (Some(b'='), Some(b'='))
            | (Some(b'!'), Some(b'='))
            | (Some(b'<'), Some(b'='))
            | (Some(b'>'), Some(b'='))
            | (Some(b'&'), Some(b'&'))
            | (Some(b'|'), Some(b'|'))
    )
}

fn next_is_digit(bytes: &[u8], i: usize) -> bool {
    bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
}

/// Scan a single-quoted string, honouring `''` doubling (§5.5).
fn scan_string(bytes: &[u8], mut i: usize) -> usize {
    i += 1; // opening quote
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2; // a doubled quote stays inside the string
            } else {
                return i + 1; // closing quote
            }
        } else {
            i += 1;
        }
    }
    i
}

/// Scan a numeric literal: optional leading `-`, integer digits, optional
/// `.`-fraction.
fn scan_number(bytes: &[u8], mut i: usize) -> usize {
    if bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    i
}

/// Scan an identifier or `::`-joined classpath.
fn scan_ident(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if is_ident_tail(bytes[i]) {
            i += 1;
        } else if bytes[i] == b':'
            && bytes.get(i + 1) == Some(&b':')
            && bytes.get(i + 2).is_some_and(|&b| is_ident_start(b))
        {
            i += 3; // `::` plus the first char of the next segment
        } else {
            break;
        }
    }
    i
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_tail(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_date_char(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'-' | b'T' | b':')
}

/// A per-database vocabulary: distinct token byte-strings mapped to dense ids,
/// with the reserved EOS bit one past the last id.
pub struct TokenVocab {
    ids: BTreeMap<Vec<u8>, u32>,
    vocab: Vocab,
}

impl TokenVocab {
    /// Build a vocabulary from `extra` token byte-strings plus every token of
    /// every query in `queries`. `extra` lets a precision lane inject phantom
    /// identifiers/literals so they have ids to assert masked.
    #[must_use]
    pub fn build(queries: &[&str], extra: &[Vec<u8>]) -> Self {
        let mut ids = BTreeMap::new();
        let mut tokens: Vec<Vec<u8>> = Vec::new();
        let add = |tok: Vec<u8>, ids: &mut BTreeMap<Vec<u8>, u32>, tokens: &mut Vec<Vec<u8>>| {
            if !ids.contains_key(&tok) {
                ids.insert(tok.clone(), tokens.len() as u32);
                tokens.push(tok);
            }
        };
        for q in queries {
            for tok in lex(q) {
                add(tok, &mut ids, &mut tokens);
            }
        }
        for tok in extra {
            add(tok.clone(), &mut ids, &mut tokens);
        }
        let eos = tokens.len() as u32;
        Self {
            ids,
            vocab: Vocab::from_byte_tokens(tokens, eos),
        }
    }

    /// The built [`Vocab`].
    #[must_use]
    pub fn vocab(&self) -> Vocab {
        self.vocab.clone()
    }

    /// The id of a token's exact bytes, if present.
    #[must_use]
    pub fn id_of(&self, token: &[u8]) -> Option<u32> {
        self.ids.get(token).copied()
    }

    /// The reserved EOS bit id.
    #[must_use]
    pub fn eos(&self) -> u32 {
        self.vocab.len() as u32
    }
}
