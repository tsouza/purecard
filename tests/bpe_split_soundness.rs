//! Tier-1 synthetic BPE-split soundness lane (adversarial-review B1/B2).
//!
//! The shipped L2 lanes tokenize gold at **lexeme granularity** (`support/lex.rs`),
//! under which every identifier is exactly one token — so the whole-token
//! exact-match narrower trivially admits it. Real byte-level BPE (Qwen2.5-Coder)
//! fragments a schema identifier across several tokens: `countryName` arrives as
//! `country` + `Name`, a classpath source as many chunks, a column string
//! `'MaxRevenue'` as `'` / `Max` / `Revenue` / `'`. This lane reproduces that
//! fragmentation **without a tokenizer dependency** by splitting each gold
//! identifier / classpath / string lexeme into chunks and replaying the chunk-id
//! stream through the real session.
//!
//! It runs two lanes over the same split vocabulary:
//!
//! - **L1** (`DecoderSession::new`) — the pure syntactic mask. Byte-liveness keeps
//!   a partial identifier alive, so every gold chunk is admissible: this lane is
//!   sound today and must stay sound.
//! - **L1+L2** (`DecoderSession::with_schema`) — the schema overlay. A prefix-aware
//!   narrower keeps a token that can still *reach* a legal name; the leading chunk
//!   of every schema identifier must survive exactly as its whole-token form does.
//!
//! Soundness must hold at BPE granularity, not merely at lexeme granularity. If a
//! gold chunk reddens the L1+L2 lane, the narrower is masking a token the model
//! must emit — fix the narrower, never weaken the assertion (§8.6).
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[path = "support/bpe.rs"]
mod bpe;
#[path = "support/corpus.rs"]
mod corpus;
#[path = "support/error.rs"]
mod error;
#[path = "support/fixture_dbs.rs"]
mod fixture_dbs;
#[path = "support/l2.rs"]
mod l2;
#[path = "support/lex.rs"]
mod lex;

use bpe::replay_tokens;
use corpus::load_gold;
use fixture_dbs::FIXTURE_DBS;
use l2::{lex, load_schema};
use purecard::{CompiledGrammar, DecoderSession, Vocab};

/// Total in-scope gold queries (the 8 fixtures) — mirrors `l2_soundness.rs`. A
/// named constant, not a threshold: a mis-count reddens the gate.
const IN_SCOPE_TOTAL: usize = 269;

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/gold_queries.jsonl")
}

/// Fragment one gold lexeme into BPE-style chunks whose concatenation is the
/// lexeme's bytes exactly.
///
/// Identifiers / classpaths are split into thirds (so their **leading** chunk is
/// a strict prefix of a schema name); string literals split as `'` / inner / `'`;
/// numbers, dates, operators, keywords, and punctuation stay whole (real BPE does
/// not fragment those in a soundness-relevant way).
fn fragment(lexeme: &[u8]) -> Vec<Vec<u8>> {
    match lexeme.first() {
        Some(b'\'') => split_string(lexeme),
        Some(&b) if b.is_ascii_alphabetic() || b == b'_' => split_thirds(lexeme),
        _ => vec![lexeme.to_vec()],
    }
}

/// Split an identifier's bytes into up to three non-empty chunks at thirds.
fn split_thirds(bytes: &[u8]) -> Vec<Vec<u8>> {
    let n = bytes.len();
    if n < 2 {
        return vec![bytes.to_vec()];
    }
    let a = (n / 3).max(1);
    let b = (2 * n / 3).max(a + 1).min(n);
    let mut chunks = vec![bytes[..a].to_vec(), bytes[a..b].to_vec()];
    if b < n {
        chunks.push(bytes[b..].to_vec());
    }
    chunks
}

/// Split a `'…'` string literal as `'` / inner-thirds / `'` (the N6 column split).
fn split_string(bytes: &[u8]) -> Vec<Vec<u8>> {
    // A well-formed string literal has at least the two surrounding quotes.
    if bytes.len() < 2 || bytes[0] != b'\'' || bytes[bytes.len() - 1] != b'\'' {
        return vec![bytes.to_vec()];
    }
    let inner = &bytes[1..bytes.len() - 1];
    let mut chunks = vec![b"'".to_vec()];
    if !inner.is_empty() {
        chunks.extend(split_thirds(inner));
    }
    chunks.push(b"'".to_vec());
    chunks
}

/// The split token-id stream of `query`: its lexemes fragmented and mapped to
/// dense ids via `ids`.
fn split_ids(query: &str, ids: &BTreeMap<Vec<u8>, u32>) -> Vec<u32> {
    let mut stream = Vec::new();
    for lexeme in lex(query) {
        for chunk in fragment(&lexeme) {
            let id = ids
                .get(&chunk)
                .unwrap_or_else(|| panic!("chunk not in split vocab: {chunk:?}"));
            stream.push(*id);
        }
    }
    stream
}

/// A synthetic BPE vocabulary over the fragmented chunks of `queries`, deduped
/// into dense ids (mirroring `TokenVocab::build`), with EOS one past the last id.
fn build_split_vocab(queries: &[&str]) -> (Vocab, BTreeMap<Vec<u8>, u32>) {
    let mut ids: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
    let mut tokens: Vec<Vec<u8>> = Vec::new();
    for query in queries {
        for lexeme in lex(query) {
            for chunk in fragment(&lexeme) {
                if !ids.contains_key(&chunk) {
                    ids.insert(chunk.clone(), tokens.len() as u32);
                    tokens.push(chunk);
                }
            }
        }
    }
    let eos = tokens.len() as u32;
    (Vocab::from_byte_tokens(tokens, eos), ids)
}

/// Group the in-scope gold `pure_text` by database (so each db's split vocab and
/// schema are built once).
fn in_scope_by_db() -> BTreeMap<String, Vec<String>> {
    let mut by_db: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in load_gold(&corpus_path()).expect("open the committed gold corpus") {
        let record = item.expect("gold corpus line parses");
        if FIXTURE_DBS.contains(&record.db_id.as_str()) {
            by_db
                .entry(record.db_id)
                .or_default()
                .push(record.pure_text);
        }
    }
    by_db
}

#[test]
fn split_chunks_concatenate_to_the_original_query() {
    // The fragmentation must partition every lexeme exactly (no byte dropped or
    // duplicated) or the whole lane is measuring the wrong bytes.
    let sample = "|spider::car_1::model::default::CarsData.all()\
        ->filter(x|$x.horsepower == '150')";
    let mut rebuilt = Vec::new();
    for lexeme in lex(sample) {
        for chunk in fragment(&lexeme) {
            assert!(!chunk.is_empty(), "a chunk is never empty");
            rebuilt.extend_from_slice(&chunk);
        }
    }
    assert_eq!(rebuilt, sample.as_bytes());
}

#[test]
fn fragmentation_actually_splits_a_schema_identifier() {
    // Non-vacuity: the lane only exercises B1 if identifiers really fragment.
    assert!(
        fragment(b"countryName").len() >= 2,
        "an identifier must split into multiple chunks"
    );
    // A column string splits around both quotes, keeping each quote its own chunk.
    let col = fragment(b"'MaxRevenue'");
    assert!(
        col.len() >= 3,
        "a column string yields quote / inner / quote"
    );
    assert_eq!(col.first().map(Vec::as_slice), Some(b"'".as_slice()));
    assert_eq!(col.last().map(Vec::as_slice), Some(b"'".as_slice()));
    // The leading chunk of a schema name is a strict prefix — the exact token the
    // whole-lexeme narrower wrongly cleared.
    let lead = &fragment(b"countryName")[0];
    assert!(b"countryName".starts_with(lead.as_slice()) && lead != b"countryName");
}

#[test]
fn l1_lane_streams_every_split_gold_soundly() {
    // L1 (no schema): byte-liveness keeps a partial identifier alive, so every
    // gold chunk is admissible. This lane is sound today and must stay sound.
    let mut total = 0usize;
    let mut failures = Vec::new();
    for (_db, queries) in in_scope_by_db() {
        let refs: Vec<&str> = queries.iter().map(String::as_str).collect();
        let (vocab, ids) = build_split_vocab(&refs);
        let eos = vocab.len() as u32;
        let grammar = CompiledGrammar::compile(vocab);
        for query in &refs {
            let mut session = DecoderSession::new(&grammar);
            if let Err(reason) = replay_tokens(&mut session, &split_ids(query, &ids), eos) {
                failures.push(format!("{reason}\n  {query}"));
            }
            total += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "L1 BPE soundness:\n{}",
        failures.join("\n")
    );
    assert_eq!(total, IN_SCOPE_TOTAL, "in-scope query count");
}

#[test]
fn l1_l2_lane_streams_every_split_gold_soundly() {
    // L1+L2 (schema overlay): the prefix-aware narrower must keep the leading
    // chunk of every schema identifier / classpath / column string. On the
    // whole-token exact-match narrower this reddens on the first fragmented name.
    let mut total = 0usize;
    let mut failures = Vec::new();
    let mut seen_dbs = BTreeSet::new();
    for (db_id, queries) in in_scope_by_db() {
        seen_dbs.insert(db_id.clone());
        let schema = load_schema(&db_id);
        let refs: Vec<&str> = queries.iter().map(String::as_str).collect();
        let (vocab, ids) = build_split_vocab(&refs);
        let eos = vocab.len() as u32;
        let grammar = CompiledGrammar::compile(vocab);
        for query in &refs {
            let mut session = DecoderSession::with_schema(&grammar, schema.clone());
            if let Err(reason) = replay_tokens(&mut session, &split_ids(query, &ids), eos) {
                failures.push(format!("[{db_id}] {reason}\n  {query}"));
            }
            total += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "L1+L2 BPE soundness ({} of {} queries failed):\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
    assert_eq!(total, IN_SCOPE_TOTAL, "in-scope query count");
    assert_eq!(
        seen_dbs.len(),
        FIXTURE_DBS.len(),
        "every fixture db exercised"
    );
}
