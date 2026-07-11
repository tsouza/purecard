//! Always-on wiring/liveness gate (default features, no network, no docker).
//!
//! This proves the corpus loads and streams through per-byte stepping without a
//! harness error. It is **not** a `DOMAIN.md` §8.1 soundness guarantee — that
//! test is token-level and mask-based and arrives with M1. Here the recognizer
//! is a `StubDecoder` that accepts every byte, so the gate's teeth are: every
//! line parses, the exact record count matches, and (in the negative test) the
//! single deadness channel fires on real corpus bytes.

use std::path::PathBuf;

// The M0 oracle harness lives under `tests/support/` (ADR-0003), not in the
// published `purecard` crate. Pull the modules in as crate-local siblings so
// `recognizer`/`corpus` resolve their `use crate::error::…` against this binary's
// own root — no code the published crate doesn't need is compiled here.
#[path = "support/corpus.rs"]
mod corpus;
#[path = "support/error.rs"]
mod error;
#[path = "support/recognizer.rs"]
mod recognizer;

use corpus::load_gold;
use error::DecodeError;
use recognizer::{ByteRecognizer, StubDecoder, replay_bytes};

/// The committed corpus record count. An exact named constant, not a `> 5000`
/// magic literal (constitution §4): both shrinkage and per-line corruption must
/// redden the gate.
const EXPECTED_GOLD_RECORDS: usize = 5034;

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/gold_queries.jsonl")
}

/// A recognizer that dies the first time it sees `target`, reporting an offset
/// equal to the count of preceding (non-target) bytes.
struct DiesOn {
    target: u8,
    consumed: usize,
}

impl ByteRecognizer for DiesOn {
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError> {
        if byte == self.target {
            return Err(DecodeError::DeadState {
                offset: self.consumed,
                byte,
            });
        }
        self.consumed += 1;
        Ok(())
    }
    fn is_complete(&self) -> bool {
        true
    }
    fn reset(&mut self) {
        self.consumed = 0;
    }
}

#[test]
fn gold_corpus_streams_and_replays_without_harness_error() {
    let records = load_gold(&corpus_path()).expect("open the committed gold corpus");
    let mut count = 0usize;
    for item in records {
        // Every line must parse — silent corpus corruption reddens the gate.
        let record = match item {
            Ok(record) => record,
            Err(err) => panic!("gold corpus failed to load: {err}"),
        };
        let mut recognizer = StubDecoder::new();
        let consumed = match replay_bytes(&mut recognizer, record.pure_text.as_bytes()) {
            Ok(consumed) => consumed,
            Err(err) => panic!("replay dead-ended on {}: {err}", record.source_id),
        };
        // Real assertions with mutation surface: a `replay_bytes -> Ok(0)` mutant
        // dies on the return value, the recognizer's own counter must reach the
        // full stream length (a "stub stops counting" mutant dies here), and the
        // stub must report completeness at end of stream.
        assert_eq!(consumed, record.pure_text.len(), "on {}", record.source_id);
        assert_eq!(
            recognizer.consumed(),
            record.pure_text.len(),
            "on {}",
            record.source_id
        );
        assert!(recognizer.is_complete(), "on {}", record.source_id);
        count += 1;
    }
    assert_eq!(count, EXPECTED_GOLD_RECORDS);
}

#[test]
fn corpus_path_reports_dead_state() {
    // Drive the SAME load → replay loop through real corpus bytes with a
    // recognizer that dies on '(' (present in every gold query via `.all()` /
    // `tableReference(`), so the only failable path is exercised on real data.
    const TARGET: u8 = b'(';
    let record = load_gold(&corpus_path())
        .expect("open corpus")
        .filter_map(Result::ok)
        .find(|record| record.pure_text.as_bytes().contains(&TARGET))
        .expect("a gold record containing '('");
    let bytes = record.pure_text.as_bytes();
    let expected_offset = bytes
        .iter()
        .position(|&byte| byte == TARGET)
        .expect("target present");

    let mut recognizer = DiesOn {
        target: TARGET,
        consumed: 0,
    };
    let err = replay_bytes(&mut recognizer, bytes).expect_err("must dead-end on '('");
    assert!(
        matches!(err, DecodeError::DeadState { offset, byte } if offset == expected_offset && byte == TARGET),
        "{err}"
    );
}
