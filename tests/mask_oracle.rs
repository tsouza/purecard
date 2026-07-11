//! The mask correctness oracle (`docs/spec/testing.md` §8.5, G1).
//!
//! The one acceptance criterion the whole M2 cache rides on: the cached,
//! runtime-flipped [`allowed_mask`](purecard::DecoderSession::allowed_mask) must
//! equal the naive brute-force truth at **every reachable state**. This drives
//! real reachable configurations by replaying each seeded accepting walk
//! (`tests/support/walker.rs`) prefix-by-prefix — never a synthetic `State`
//! literal — and, at each prefix, asserts the session's mask is *bit-equal* to
//! [`brute_force_mask`](synth::brute_force_mask) (a fresh-clone, byte-at-a-time
//! probe of every synthetic-vocab token), EOS bit included.
//!
//! This pins `cache[state].indep ∪ runtime-deferred-flip == uncached truth`. It
//! is the executioner for every cache mutant: "return the cache without the
//! flip", "flip the wrong bit", "skip EOS", "cache the wrong state".

#[path = "support/synth.rs"]
mod synth;
#[path = "support/walker.rs"]
mod walker;

use purecard::{ByteRecognizer, CompiledGrammar, DecoderSession, Pda};
use synth::{brute_force_mask, synthetic_vocab};
use walker::generate_walks;

/// Synthetic vocabulary size for the oracle. Large enough that every character
/// class (and so every context-independent / context-dependent partition) is
/// densely represented — all 1- and 2-byte strings over the alphabet plus a
/// slice of 3-byte ones — while keeping the O(prefixes · vocab) brute force fast.
const VOCAB_SIZE: usize = 1200;

/// Assert the session's live mask equals the brute-force reference at `reference`'s
/// configuration, reporting the walk and prefix length on mismatch.
fn assert_mask_matches(
    session: &mut DecoderSession<'_>,
    reference: &Pda,
    vocab: &purecard::Vocab,
    walk: &[u8],
    prefix_len: usize,
) {
    let expected = brute_force_mask(reference, vocab);
    let got = session.allowed_mask();
    assert!(
        *got == expected,
        "mask mismatch after {prefix_len} bytes of walk {:?} (state {:?}, stack_top {:?})",
        String::from_utf8_lossy(&walk[..prefix_len]),
        reference.state(),
        reference.stack_top(),
    );
}

#[test]
fn allowed_mask_bit_equals_brute_force_at_every_reachable_walk_prefix() {
    let grammar = CompiledGrammar::compile(synthetic_vocab(VOCAB_SIZE));
    let vocab = grammar.vocab();
    let walks = generate_walks();

    let mut distinct_states = std::collections::HashSet::new();
    let mut saw_nonempty_stack = false;
    let mut saw_deferred_survivor = false;

    for walk in &walks {
        let mut session = DecoderSession::new(&grammar);
        let mut reference = Pda::new();

        // The empty prefix (state Start) counts too.
        assert_mask_matches(&mut session, &reference, vocab, walk, 0);

        for (i, &byte) in walk.iter().enumerate() {
            session
                .accept_byte(byte)
                .expect("a generated walk prefix is always live");
            reference
                .advance(byte)
                .expect("a generated walk prefix is always live");

            distinct_states.insert(reference.state());
            if reference.stack_top().is_some() {
                saw_nonempty_stack = true;
                // A closing byte for the live top frame is a context-dependent
                // token that must survive the runtime flip — proves the flip is
                // exercised against a real stack, not vacuously.
                let closer = match reference.stack_top() {
                    Some(purecard::grammar::pda::Frame::Paren) => b')',
                    Some(purecard::grammar::pda::Frame::Bracket) => b']',
                    _ => b'}',
                };
                if reference.probe(&[closer], &mut Vec::new()).alive {
                    saw_deferred_survivor = true;
                }
            }

            assert_mask_matches(&mut session, &reference, vocab, walk, i + 1);
        }
    }

    // Non-vacuity: the walks must have exercised many distinct states, at least
    // one non-empty stack, and at least one live context-dependent closer.
    assert!(
        distinct_states.len() >= 15,
        "walks reached only {} distinct states — oracle is too narrow",
        distinct_states.len()
    );
    assert!(
        saw_nonempty_stack,
        "no walk prefix reached a non-empty stack"
    );
    assert!(
        saw_deferred_survivor,
        "no prefix exercised a live context-dependent closer against the stack"
    );
}
