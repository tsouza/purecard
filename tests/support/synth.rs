//! Synthetic vocabulary + the brute-force mask oracle, shared by the M2 mask
//! tests and the criterion benchmark (`docs/spec/testing.md` §8.5).
//!
//! There is no real model tokenizer in the repo (non-goal, as at M0/M1), so the
//! mask is exercised against a *synthetic* byte-token vocabulary: every short
//! byte string over the walker's representative [`ALPHABET`], enumerated
//! deterministically. [`brute_force_mask`] is the permanently-correct reference —
//! it clones the live [`Pda`] and replays each token's bytes one at a time,
//! independent of the cache and the `probe` machinery it is meant to check.
//!
//! Shared via `#[path]` into several targets — the oracle, the property lane, and
//! the criterion bench — each of which uses a subset (the bench needs only
//! [`synthetic_vocab`]). `allow(dead_code)` covers the items a given target does
//! not touch; this is a cross-target helper, not dead product code.
#![allow(dead_code)]

use purecard::{BitMask, Pda, Vocab};

/// The representative byte alphabet the synthetic tokens are drawn from — the
/// same one the walk generator probes (`tests/support/walker.rs`), so tokens
/// cover every character class the byte-PDA distinguishes.
pub const ALPHABET: &[u8] = b"abXY1_ |{}()[].,;:$%'-><=!&+*/";

/// A deterministic synthetic vocabulary of `count` distinct byte-tokens: every
/// string over [`ALPHABET`] in ascending length, then ascending index within a
/// length, until `count` are produced. The reserved EOS bit is `count` (one past
/// the last id), so `Vocab`'s own eos is set to `0` and is irrelevant here.
#[must_use]
pub fn synthetic_vocab(count: usize) -> Vocab {
    let base = ALPHABET.len();
    let mut tokens: Vec<Vec<u8>> = Vec::with_capacity(count);
    let mut len = 1u32;
    while tokens.len() < count {
        let total = base.pow(len);
        for idx in 0..total {
            if tokens.len() == count {
                break;
            }
            let mut token = Vec::with_capacity(len as usize);
            let mut rem = idx;
            for _ in 0..len {
                token.push(ALPHABET[rem % base]);
                rem /= base;
            }
            tokens.push(token);
        }
        len += 1;
    }
    Vocab::from_byte_tokens(tokens, 0)
}

/// The permanently-correct reference mask at `pda`'s live configuration: the set
/// of token ids whose raw bytes keep a *clone* of the automaton non-dead, plus
/// the reserved EOS bit iff the automaton is already accepting.
///
/// Deliberately naive — a fresh clone and a byte-at-a-time `advance` per token,
/// touching only the public [`Pda`] API — so it shares no code with the cache or
/// `probe` it validates.
#[must_use]
pub fn brute_force_mask(pda: &Pda, vocab: &Vocab) -> BitMask {
    let mut mask = BitMask::with_len(vocab.len() + 1);
    for id in 0..vocab.len() as u32 {
        let bytes = vocab.bytes(id).unwrap_or(&[]);
        let mut clone = pda.clone();
        if bytes.iter().all(|&byte| clone.advance(byte).is_ok()) {
            mask.set(id);
        }
    }
    if pda.is_accepting() {
        mask.set(vocab.len() as u32);
    }
    mask
}
