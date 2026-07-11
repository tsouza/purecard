//! The decode session: the byte-PDA, the per-step token mask, and the offset
//! bookkeeping the recognizer contract needs.
//!
//! [`DecoderSession`] is the shipped implementation of [`ByteRecognizer`]. It
//! wraps a [`Pda`] and a byte-offset counter, folding each byte through the
//! automaton and translating a dead state into a [`DecodeError::DeadState`]
//! carrying the offset at which the stream ran out of continuations.
//!
//! At M2 it also borrows a [`CompiledGrammar`] and exposes the masking surface
//! (`docs/spec/architecture.md` §4, §9): [`allowed_mask`](DecoderSession::allowed_mask)
//! returns the set of tokens that keep the stream on a path to a valid query, and
//! [`accept_token`](DecoderSession::accept_token) advances by a whole token,
//! rolling back untouched if the token is inadmissible. The schema overlay (L2)
//! is still absent — `schema` is conceptually `None` — and narrows the mask at a
//! single documented intersection point in a later milestone (§3.1).

use crate::error::DecodeError;
use crate::grammar::compiled::CompiledGrammar;
use crate::grammar::pda::{Frame, Pda};
use crate::mask::BitMask;
use crate::recognizer::ByteRecognizer;

/// A byte-at-a-time decode session over the emitted-Pure grammar, bound to a
/// [`CompiledGrammar`].
///
/// Construct one with [`DecoderSession::new`], then either drive it byte-wise
/// through [`ByteRecognizer`] or token-wise through
/// [`accept_token`](DecoderSession::accept_token), reading the legal next-token
/// set from [`allowed_mask`](DecoderSession::allowed_mask) at each step.
/// [`reset`](DecoderSession::reset) returns it to a fresh stream while keeping
/// the automaton's stack and the mask buffer allocated.
#[derive(Debug, Clone)]
pub struct DecoderSession<'g> {
    pda: Pda,
    offset: usize,
    grammar: &'g CompiledGrammar,
    /// The owned, reused mask buffer `allowed_mask` refills each step — sized to
    /// `vocab.len() + 1` (EOS bit included) so no per-step allocation is needed.
    mask: BitMask,
    /// A reused scratch stack for the per-step deferred-token re-probe, kept here
    /// so the hot path never allocates.
    scratch: Vec<Frame>,
}

impl<'g> DecoderSession<'g> {
    /// A fresh session at the start of a stream, masking against `grammar`.
    ///
    /// L1-only: no schema, so the mask is the pure syntactic next-token set.
    #[must_use]
    pub fn new(grammar: &'g CompiledGrammar) -> Self {
        Self {
            pda: Pda::new(),
            offset: 0,
            grammar,
            mask: BitMask::with_len(grammar.vocab().len() + 1),
            scratch: Vec::new(),
        }
    }

    /// The number of bytes consumed since the last [`reset`](DecoderSession::reset).
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The set of token ids that keep the stream on a path to a valid query at
    /// the current position: every token whose raw bytes leave the byte-PDA
    /// non-dead (§4.4), with the reserved EOS bit set iff
    /// [`is_complete`](DecoderSession::is_complete).
    ///
    /// Cost is one word-wise copy of the state's cached context-independent mask
    /// plus a re-probe of the small deferred (stack-dependent) token set against
    /// the live stack — the per-step performance core (§4.3). It fills the
    /// grammar's lazy cache for the current state on first visit.
    ///
    /// Takes `&mut self` because it refills the session's reused mask buffer in
    /// place (and fills the lazy per-state cache); a safe `&self` returning
    /// `&BitMask` is impossible without handing out a reference into an owned
    /// buffer it must first mutate, and `unsafe` is forbidden (constitution §1).
    pub fn allowed_mask(&mut self) -> &BitMask {
        let cached = self.grammar.cached(self.pda.state());
        self.mask.copy_from(&cached.indep);
        // Every deferred token is context-dependent *because* it needs an
        // enclosing frame (it died consulting the ambient stack during the
        // empty-scratch build). With an empty live stack there is nothing to
        // consult, so all of them stay dead — skip the whole re-probe.
        if self.pda.stack_top().is_some() {
            for &id in &cached.deferred {
                let bytes = self.grammar.vocab().bytes(id).unwrap_or(&[]);
                if self.pda.admits(bytes, &mut self.scratch) {
                    self.mask.set(id);
                }
            }
        }
        let eos = self.grammar.eos_bit();
        if self.pda.is_accepting() {
            self.mask.set(eos);
        } else {
            self.mask.clear(eos);
        }
        // M3 hook (§4.3, non-goal here): the schema overlay narrows the syntactic
        // mask to schema-legal terminals at exactly this point —
        //   if let Some(terminals) = &self.schema { self.mask.intersect(terminals) }
        // one word-wise `intersect` against a precomputed set, no new per-step cost.
        &self.mask
    }

    /// Advance the session by one whole token, or reject it leaving the session
    /// **untouched** (§8.5 — the invariant that makes speculative masking sound).
    ///
    /// The reserved EOS id (one past the last vocab token) is accepted iff the
    /// stream is already complete. Otherwise the token's raw bytes are folded
    /// through the byte-PDA; if any byte dead-ends, the automaton is rolled back
    /// to its pre-token configuration and the token is rejected. Byte-for-byte
    /// equivalent to folding [`accept_byte`](ByteRecognizer::accept_byte) over
    /// `vocab.bytes(id)`, except the roll-back hides a partial advance on
    /// rejection.
    ///
    /// # Errors
    /// Returns [`DecodeError::UnexpectedEos`] if EOS is signalled before the
    /// stream is complete, or [`DecodeError::InadmissibleToken`] if the token's
    /// bytes dead-end the recognizer (including an out-of-range id).
    pub fn accept_token(&mut self, id: u32) -> Result<(), DecodeError> {
        if id == self.grammar.eos_bit() {
            return if self.pda.is_accepting() {
                Ok(())
            } else {
                Err(DecodeError::UnexpectedEos)
            };
        }
        let Some(bytes) = self.grammar.vocab().bytes(id) else {
            return Err(DecodeError::InadmissibleToken { id });
        };
        // Snapshot the automaton so an interior dead byte rolls the whole token
        // back — `advance` is no-mutate-on-dead per byte, but earlier bytes of
        // this token have already mutated. The clone is one small stack copy per
        // *accepted* token, off the per-candidate mask hot path.
        let saved = self.pda.clone();
        for &byte in bytes {
            if self.pda.advance(byte).is_err() {
                self.pda = saved;
                return Err(DecodeError::InadmissibleToken { id });
            }
        }
        self.offset += bytes.len();
        Ok(())
    }

    /// Whether the stream so far is a complete query (an accepting configuration).
    ///
    /// Re-exposed inherently so callers need not import [`ByteRecognizer`]; it
    /// mirrors [`ByteRecognizer::is_complete`].
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.pda.is_accepting()
    }

    /// Return to a fresh stream, keeping the automaton's stack and the mask
    /// buffer allocated for reuse (§9.1). Mirrors [`ByteRecognizer::reset`].
    pub fn reset(&mut self) {
        self.pda.reset();
        self.offset = 0;
    }
}

impl ByteRecognizer for DecoderSession<'_> {
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError> {
        match self.pda.advance(byte) {
            Ok(()) => {
                self.offset += 1;
                Ok(())
            }
            Err(dead) => Err(DecodeError::DeadState {
                offset: self.offset,
                byte,
                state: dead.state,
                stack_top: dead.stack_top,
            }),
        }
    }

    fn is_complete(&self) -> bool {
        self.pda.is_accepting()
    }

    fn reset(&mut self) {
        self.pda.reset();
        self.offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::DecoderSession;
    use crate::error::DecodeError;
    use crate::grammar::compiled::CompiledGrammar;
    use crate::recognizer::ByteRecognizer;
    use crate::vocab::Vocab;

    /// An L1-only grammar over an empty vocabulary — enough to drive the
    /// byte-recognizer surface, which does not consult the vocab.
    fn l1_grammar() -> CompiledGrammar {
        CompiledGrammar::compile(Vocab::from_byte_tokens(Vec::new(), 0))
    }

    fn drive(text: &str) -> (Result<(), DecodeError>, bool, usize) {
        let grammar = l1_grammar();
        let mut session = DecoderSession::new(&grammar);
        let mut result = Ok(());
        for &byte in text.as_bytes() {
            if let Err(err) = session.accept_byte(byte) {
                result = Err(err);
                break;
            }
        }
        (result, session.is_complete(), session.offset())
    }

    #[test]
    fn a_complete_query_streams_and_is_complete() {
        let (result, complete, offset) = drive("|X.all()->take(3)");
        assert!(result.is_ok());
        assert!(complete);
        assert_eq!(offset, "|X.all()->take(3)".len());
    }

    #[test]
    fn a_partial_query_is_not_complete() {
        let (result, complete, _) = drive("|X.all()->take(3");
        assert!(result.is_ok());
        assert!(!complete);
    }

    #[test]
    fn a_dead_byte_reports_offset_and_state() {
        let grammar = l1_grammar();
        let mut session = DecoderSession::new(&grammar);
        let mut err = None;
        for &byte in "|X.all())".as_bytes() {
            if let Err(e) = session.accept_byte(byte) {
                err = Some(e);
                break;
            }
        }
        let DecodeError::DeadState {
            offset,
            byte,
            stack_top,
            ..
        } = err.expect("extra ')' must dead-end")
        else {
            panic!("expected a dead state");
        };
        assert_eq!(byte, b')');
        assert_eq!(offset, "|X.all()".len());
        assert_eq!(stack_top, "none");
    }

    #[test]
    fn reset_rewinds_offset_and_state() {
        let grammar = l1_grammar();
        let mut session = DecoderSession::new(&grammar);
        for &byte in b"|X.all()" {
            session.accept_byte(byte).expect("live");
        }
        session.reset();
        assert_eq!(session.offset(), 0);
        assert!(!session.is_complete());
        assert!(session.accept_byte(b'x').is_err());
    }

    /// A vocabulary of whole tokens for the token-level surface: an opener/source
    /// prefix, a step, closers, and the empty token.
    fn token_vocab() -> Vocab {
        Vocab::from_byte_tokens(
            vec![
                b"|X.all()".to_vec(), // 0: a complete source expression
                b"->take(".to_vec(),  // 1: a step opening a Paren
                b"1".to_vec(),        // 2: a digit
                b")".to_vec(),        // 3: a closer
                b"".to_vec(),         // 4: the empty token
            ],
            4,
        )
    }

    #[test]
    fn accept_token_streams_a_query_token_by_token() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        for id in [0u32, 1, 2, 3] {
            session.accept_token(id).expect("admissible token");
        }
        assert!(session.is_complete());
        assert_eq!(session.offset(), "|X.all()->take(1)".len());
    }

    #[test]
    fn an_inadmissible_token_is_rejected_and_leaves_the_session_untouched() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        session.accept_token(0).expect("source is admissible");
        // `|X.all()` is itself a complete query (AfterValue, empty stack).
        assert!(session.is_complete());
        let before_offset = session.offset();
        // A lone closer `)` cannot follow a completed value with an empty stack.
        let err = session
            .accept_token(3)
            .expect_err("closer must be rejected");
        assert!(matches!(err, DecodeError::InadmissibleToken { id: 3 }));
        // The rejected token left the session byte-identical: same offset, and
        // still complete.
        assert_eq!(session.offset(), before_offset);
        assert_eq!(session.offset(), "|X.all()".len());
        assert!(session.is_complete());
    }

    #[test]
    fn an_out_of_range_token_id_is_inadmissible() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        let err = session.accept_token(999).expect_err("no such token");
        assert!(matches!(err, DecodeError::InadmissibleToken { id: 999 }));
    }

    #[test]
    fn eos_is_accepted_only_when_the_stream_is_complete() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let eos = grammar.eos_bit();
        let mut session = DecoderSession::new(&grammar);
        // Premature EOS on an empty stream is rejected.
        assert!(matches!(
            session.accept_token(eos),
            Err(DecodeError::UnexpectedEos)
        ));
        for id in [0u32, 1, 2, 3] {
            session.accept_token(id).expect("admissible");
        }
        // Now the query is complete, EOS is legal.
        assert!(session.accept_token(eos).is_ok());
    }

    #[test]
    fn allowed_mask_sets_the_eos_bit_iff_complete() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let eos = grammar.eos_bit();
        let mut session = DecoderSession::new(&grammar);
        assert!(!session.allowed_mask().test(eos), "start is not complete");
        for id in [0u32, 1, 2, 3] {
            session.accept_token(id).expect("admissible");
        }
        assert!(
            session.allowed_mask().test(eos),
            "completed stream allows EOS"
        );
    }
}
