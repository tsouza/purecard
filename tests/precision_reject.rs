//! The precision (negative) corpus — the pin no other gate can replace
//! (`specs/m1-l1-grammar.md`, Fix 1a; ADR-0004).
//!
//! Gold soundness (`tests/soundness_replay.rs`) proves the PDA *accepts* every
//! valid query; coverage and mutation observe which lines run and which mutants
//! die. **None of them can see over-acceptance** — an automaton that accepted
//! every byte string would pass all three identically. This suite is the missing
//! half: a curated set of malformed emitted-Pure strings that the recogniser MUST
//! reject, so a widening that reopens one of these structural holes reddens a PR
//! instead of silently passing.
//!
//! "Reject" is the exact negation of the soundness killer property: a string is
//! rejected when the real [`DecoderSession`] either hits a [`DecodeError`] on some
//! byte **or** ends the stream in a non-accepting (incomplete) state. Both are
//! genuine refusals — a decoder that never dead-ends but never completes has still
//! declined the string.

use purecard::{ByteRecognizer, DecodeError, DecoderSession};

/// Drive `text` through a fresh real [`DecoderSession`] and report whether the
/// recogniser refuses it — a mid-stream dead state, or an incomplete stream at
/// end-of-input. The mirror image of `soundness_replay::replay`.
fn dies(text: &str) -> bool {
    let mut session = DecoderSession::new();
    for &byte in text.as_bytes() {
        if let Err(DecodeError::DeadState { .. }) = session.accept_byte(byte) {
            return true;
        }
    }
    !session.is_complete()
}

/// Sanity anchor: a well-formed query from each arm is *not* rejected, so `dies`
/// is discriminating and not vacuously true.
#[test]
fn well_formed_gold_shapes_are_not_rejected() {
    assert!(!dies("|X.all()->take(3)"));
    assert!(!dies(
        "|db::Db->tableReference('default','T')->tableToTDS()->limit(5)"
    ));
    assert!(!dies(
        "{|let m = X.all()->take(1); Y.all()->filter(b|$b.v == $m)->take(1);}"
    ));
}

/// A query is a source pipeline, never a bare value (findings A/B: `|42`, `|*`,
/// `|( )` reached [`AfterValue`] and were accepted as complete).
#[test]
fn a_top_level_source_must_be_an_identifier() {
    assert!(dies("|42"));
    assert!(dies("|42 "));
    assert!(dies("|*"));
    assert!(dies("|( )"));
    assert!(dies("|'lit'"));
    assert!(dies("|%2018-03-17"));
    assert!(dies("|$x->take(1)"));
}

/// A completed term must be followed by a connector/operator/closer, never a bare
/// abutting identifier — the headline missing-`->` hole (findings A/B).
#[test]
fn a_completed_term_is_not_followed_by_a_bare_identifier() {
    assert!(dies("|foo bar baz"));
    assert!(dies("|foo bar baz "));
    assert!(dies("|X.all() take(3)"));
    assert!(dies("|X.all()->take(1) take(2)"));
    assert!(dies("|X.all()->filter(nonsense garbage here)"));
}

/// A binary operator demands an operand; a closer may not follow it (finding D).
#[test]
fn a_dangling_operator_before_a_closer_dies() {
    assert!(dies("|X.all()->take(1 +)"));
    assert!(dies("|X.all()->take(1 -)"));
    assert!(dies("|X.all()->take(1 *)"));
    assert!(dies("|X.all()->filter(x|$x.a && )"));
    assert!(dies("|X.all()->filter(x|$x.a || )"));
    assert!(dies("|X.all()->filter(x|$x.a > )"));
    assert!(dies("|X.all()->filter(x|$x.a == )"));
}

/// Numeric literals must be well-formed: a sign needs a digit, a `.` needs a
/// fractional digit, and a doubled sign is invalid (finding E).
#[test]
fn malformed_numeric_literals_die() {
    assert!(dies("|X.all()->take(-)"));
    assert!(dies("|X.all()->take(1.)"));
    assert!(dies("|X.all()->take(--5)"));
    assert!(dies("|X.all()->take(-.5)"));
    assert!(dies("|X.all()->filter(x|$x.a > .5)"));
}

/// A date literal must carry at least one date character (finding F).
#[test]
fn an_empty_date_literal_dies() {
    assert!(dies("|X.all()->take(%)"));
    assert!(dies("|X.all()->filter(x|$x.d < %)"));
}

/// A lone `=` is not a comparison operator; only `==` compares, and a single `=`
/// lives only in a block-query `let` binder (finding G).
#[test]
fn a_single_equals_as_a_comparison_operator_dies() {
    assert!(dies("|X.all()->filter(x|$x.a = 1)"));
    assert!(dies(
        "|db::Db->tableReference('default','T')->tableToTDS()\
                  ->filter(row: meta::pure::tds::TDSRow[1]|$row.getInteger('c') = 1)"
    ));
}

/// A block query is `{|…}`; the leading pipe is not optional (finding I).
#[test]
fn a_block_query_without_the_leading_pipe_dies() {
    assert!(dies("{X.all()->take(1)}"));
    assert!(dies("{X.all()->take(1);}"));
    assert!(dies("{ X.all()->take(1) }"));
}

/// Only `::` (classpath) and a single typed-binder `:` are valid; a tripled colon
/// is not (finding J).
#[test]
fn colon_runs_beyond_a_double_colon_die() {
    assert!(dies("|X:::Y.all()->take(1)"));
    assert!(dies("|meta:::pure::Thing.all()->take(1)"));
    // A `:` (single or `::`) demands an identifier segment, never a digit, and a
    // `::` separator carries no interior whitespace.
    assert!(dies("|X:5.all()->take(1)"));
    assert!(dies("|X::5.all()->take(1)"));
    assert!(dies("|meta:: pure::Thing.all()->take(1)"));
}

/// Structural closers still honour the frame stack and the source rule together —
/// a spot check that the tightenings did not reopen the delimiter invariants.
#[test]
fn delimiter_and_source_invariants_hold_together() {
    // Crossed closer under the new source rule.
    assert!(dies("|X.all()->take(2]"));
    // Unmatched trailing closer.
    assert!(dies("|X.all())"));
    // Unclosed call — incomplete, not dead.
    assert!(dies("|X.all()->take(2"));
}
