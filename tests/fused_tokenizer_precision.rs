//! Tier-A: hermetic fused-nav-dot precision replay through REAL tokenizations.
//!
//! The synthetic `bpe_split_soundness` lane proves L2 stays *sound* under BPE
//! fragmentation, and `l2_precision`'s fused cases prove the decoder *masks* a
//! hand-authored fused `.`+char token. This lane closes the gap between them: it
//! drives fused tokens that a REAL byte-level BPE tokenizer (Qwen2.5-Coder and
//! GPT-4's cl100k_base) actually emits — proving fused-nav-dot PRECISION against
//! genuine tokenizer merge boundaries, not a proxy.
//!
//! It is per-PR **hermetic**: it reads only the vendored
//! `tests/fixtures/tokenizers/fused_precision.jsonl` (each row is one real
//! tokenizer's byte-level token strings, byte-unicode encoded), with NO tokenizer
//! crate and NO network. The feature-gated Tier-B extractor
//! (`fused_tokenizer_extract.rs`) re-derives that fixture from the actual
//! tokenizers and the nightly `qwen-oracle.yml` workflow diffs it, so the fixture
//! cannot rot silently.
//!
//! For each row it decodes the token strings to raw bytes (the same byte-level
//! table serves both tokenizers), builds a per-query [`Vocab`] in fixture id order,
//! loads the named schema, drives the prefix tokens through a schema-aware
//! [`DecoderSession`], then asserts the fused token at `fused_index`: an `admit`
//! case must stay admissible (soundness — a real member fused with the dot), a
//! `mask` case must be cleared (precision — a phantom whose first char begins no
//! navigable member).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

#[path = "support/bpe.rs"]
mod bpe;
#[path = "support/fused_fixture.rs"]
mod fused_fixture;
#[path = "support/l2.rs"]
mod l2;
#[path = "support/lex.rs"]
mod lex;

use bpe::{gpt2_byte_decoder, true_bytes};
use fused_fixture::{Expect, FusedCase};
use l2::load_schema;
use purecard::{CompiledGrammar, DecoderSession, Vocab};

/// Every case the fixture must carry — an exact count so a truncated or bloated
/// fixture reddens the gate (constitution §3, no thresholds).
const EXPECTED_CASES: usize = 16;

/// The minimum cases each tokenizer must contribute, so the precision proof is
/// driven through BOTH real tokenizers and a fixture missing one reddens.
const MIN_PER_TOKENIZER: usize = 6;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tokenizers/fused_precision.jsonl")
}

fn load_cases() -> Vec<FusedCase> {
    let path = fixture_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fused fixture {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("parse fixture row {l:?}: {e}")))
        .collect()
}

/// Decode a row's token strings to the raw bytes the model emits, in fixture id
/// order (token id `i` is `token_strings[i]`), and cross-check that the tokens up
/// to `fused_index` reconstruct the recorded prefix exactly.
fn decode_row(case: &FusedCase) -> Vec<Vec<u8>> {
    let dec = gpt2_byte_decoder();
    let tokens: Vec<Vec<u8>> = case
        .token_strings
        .iter()
        .map(|t| true_bytes(t, &dec))
        .collect();
    assert!(
        case.fused_index < tokens.len(),
        "fused_index {} out of range in {:?}",
        case.fused_index,
        case.note
    );
    let prefix_bytes: Vec<u8> = tokens[..case.fused_index].concat();
    assert_eq!(
        prefix_bytes,
        case.prefix.as_bytes(),
        "prefix tokens do not reconstruct the recorded prefix in {:?}",
        case.note
    );
    let fused = &tokens[case.fused_index];
    assert!(
        fused.first() == Some(&b'.') && fused.len() >= 2,
        "fixture row is not a genuine fused nav dot in {:?}: {fused:?}",
        case.note
    );
    tokens
}

/// Drive the prefix tokens through a schema-aware session and return it positioned
/// at the fused decision point. Every prefix token must stream (it is a real
/// tokenization of a legal partial query — a masked prefix token is a soundness
/// bug), exercising exactly the cross-boundary merge tokens (`.all`, `()->`, `(c`,
/// `|$`) the fused lane exists to stress.
fn drive_to_fused<'g>(
    grammar: &'g CompiledGrammar,
    schema: purecard::Schema,
    tokens: &[Vec<u8>],
    fused_index: usize,
    note: &str,
) -> DecoderSession<'g> {
    let mut session = DecoderSession::with_schema(grammar, schema);
    for (step, _tok) in tokens.iter().take(fused_index).enumerate() {
        let id = step as u32;
        assert!(
            session.allowed_mask().test(id),
            "SOUNDNESS: prefix token at step {step} masked in {note:?}"
        );
        session
            .accept_token(id)
            .unwrap_or_else(|e| panic!("prefix token at step {step} rejected in {note:?}: {e}"));
    }
    session
}

#[test]
fn real_tokenizer_fused_nav_dots_are_precisely_masked() {
    let cases = load_cases();

    let mut per_tokenizer: BTreeMap<String, usize> = BTreeMap::new();
    for case in &cases {
        *per_tokenizer.entry(case.tokenizer.clone()).or_default() += 1;

        let tokens = decode_row(case);
        let eos = tokens.len() as u32;
        let vocab = Vocab::from_byte_tokens(tokens.clone(), eos);
        let grammar = CompiledGrammar::compile(vocab);
        let schema = load_schema(&case.db);
        let mut session = drive_to_fused(&grammar, schema, &tokens, case.fused_index, &case.note);

        let fused_id = case.fused_index as u32;
        let admitted = session.allowed_mask().test(fused_id);
        match case.fused_expect {
            Expect::Admit => assert!(
                admitted,
                "SOUNDNESS: a real fused member navigation was masked in {:?} [{}]",
                case.note, case.tokenizer
            ),
            Expect::Mask => assert!(
                !admitted,
                "PRECISION: a fused phantom nav dot survived in {:?} [{}]",
                case.note, case.tokenizer
            ),
        }
    }

    // The fixture must be whole and span both real tokenizers.
    assert_eq!(
        cases.len(),
        EXPECTED_CASES,
        "fused-precision case count moved (regenerate + review)"
    );
    for tokenizer in ["qwen", "gpt4"] {
        let n = per_tokenizer.get(tokenizer).copied().unwrap_or(0);
        assert!(
            n >= MIN_PER_TOKENIZER,
            "tokenizer {tokenizer:?} contributes only {n} fused cases (< {MIN_PER_TOKENIZER}); the precision proof must span both real tokenizers"
        );
    }
}
