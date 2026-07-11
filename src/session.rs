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
//! rolling back untouched if the token is inadmissible. The schema overlay (L2,
//! M3) is shipped: [`with_schema`](DecoderSession::with_schema) installs a
//! [`Schema`], and [`allowed_mask`](DecoderSession::allowed_mask) intersects the
//! syntactic L1 mask with the schema-legal set at each identifier and operand
//! position (§3.1) — an additive narrowing that leaves the `schema`-is-`None`
//! (L1-only) path untouched.

use crate::error::DecodeError;
use crate::grammar::compiled::CompiledGrammar;
use crate::grammar::pda::{Frame, Pda};
use crate::mask::BitMask;
use crate::recognizer::ByteRecognizer;
use crate::schema::Schema;
use crate::schema::narrow::narrow_into;
use crate::schema::scope::ScopeTracker;

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
    /// [`CompiledGrammar::mask_len`] (EOS bit included) so no per-step allocation
    /// is needed.
    mask: BitMask,
    /// A reused scratch stack for the per-step deferred-token re-probe, kept here
    /// so the hot path never allocates.
    scratch: Vec<Frame>,
    /// A second reused buffer the L2 overlay refills in place with the
    /// schema-legal set, then intersects into `mask` — so narrowing allocates no
    /// per-step mask (§4.3). Sized, like `mask`, to
    /// [`CompiledGrammar::mask_len`]. Left untouched on the L1-only (`schema` is
    /// `None`) path.
    narrow_buf: BitMask,
    /// The optional L2 schema overlay. `None` is L1-only (M0–M2 behaviour): the
    /// schema-narrowing block in [`allowed_mask`](DecoderSession::allowed_mask) is
    /// skipped entirely, so there is zero added per-step cost.
    schema: Option<Schema>,
    /// The §6.4 scope machine, advanced in lockstep with
    /// [`accept_token`](DecoderSession::accept_token). Inert (never consulted)
    /// when `schema` is `None`.
    tracker: ScopeTracker,
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
            mask: BitMask::with_len(grammar.mask_len()),
            scratch: Vec::new(),
            narrow_buf: BitMask::with_len(grammar.mask_len()),
            schema: None,
            tracker: ScopeTracker::new(),
        }
    }

    /// A fresh session that also enforces the L2 schema overlay against `schema`
    /// (`docs/spec/schema.md` §6): at each identifier and operand position the
    /// syntactic L1 mask is intersected with the schema-legal set, so phantom
    /// classes/properties and type-mismatched operands are cleared. L2 only ever
    /// *narrows* — the additive counterpart to [`new`](DecoderSession::new), which
    /// stays L1-only and byte-compatible for M0–M2 callers.
    #[must_use]
    pub fn with_schema(grammar: &'g CompiledGrammar, schema: Schema) -> Self {
        Self {
            pda: Pda::new(),
            offset: 0,
            grammar,
            mask: BitMask::with_len(grammar.mask_len()),
            scratch: Vec::new(),
            narrow_buf: BitMask::with_len(grammar.mask_len()),
            schema: Some(schema),
            tracker: ScopeTracker::new(),
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
        // L2 (§6): narrow the syntactic mask to the schema-legal set at exactly
        // this point. A pure `intersect` can only clear bits, so `L2 ⊆ L1` is
        // structural; the narrow set always keeps EOS so a complete query stays
        // completable. The set is built into the reused `narrow_buf` (no per-step
        // alloc); when `schema` is `None` the block is skipped entirely, so the
        // L1-only path keeps its zero added per-step cost.
        if let Some(schema) = &self.schema {
            let pos = self.tracker.position(self.pda.state());
            if narrow_into(
                &mut self.narrow_buf,
                schema,
                &pos,
                self.tracker.emitted_columns(),
                self.grammar.vocab(),
                self.grammar.eos_bit(),
            ) {
                self.mask.intersect(&self.narrow_buf);
            }
        }
        &self.mask
    }

    /// Advance the session by one whole token, or reject it leaving the session
    /// **untouched** (§8.5 — the invariant that makes speculative masking sound).
    ///
    /// The reserved EOS id (one past the last vocab token) is accepted iff the
    /// stream is already complete; an unknown (out-of-range) id, or one whose
    /// bytes dead-end the recognizer, is rejected. The token is folded through a
    /// **clone** of the byte-PDA and the clone is committed only if every byte
    /// survives; a mid-token dead byte discards the clone, so the live automaton
    /// — its state *and* the full contents of its frame stack — is provably
    /// unchanged. (Restoring only a saved `(state, stack_len)` could not rebuild
    /// a frame an interior `Pop` had removed.) On acceptance it is byte-for-byte
    /// equivalent to folding [`accept_byte`](ByteRecognizer::accept_byte) over
    /// `vocab.bytes(id)`.
    ///
    /// # Errors
    /// Returns [`DecodeError::UnexpectedEos`] if EOS is signalled before the
    /// stream is complete, [`DecodeError::UnknownToken`] if `id` is out of range
    /// (a host-contract violation — no `Vocab` entry), or
    /// [`DecodeError::InadmissibleToken`] if an in-range token's bytes dead-end
    /// the recognizer (a legitimate, mask-respecting reject).
    pub fn accept_token(&mut self, id: u32) -> Result<(), DecodeError> {
        if id == self.grammar.eos_bit() {
            return if self.pda.is_accepting() {
                Ok(())
            } else {
                Err(DecodeError::UnexpectedEos)
            };
        }
        // An id with no `Vocab` entry (out of range) is a host-contract violation,
        // reported distinctly from an in-range token the mask legitimately clears.
        let Some(bytes) = self.grammar.vocab().bytes(id) else {
            return Err(DecodeError::UnknownToken { id });
        };
        // Fold into a clone and commit only on full success: a rejection never
        // touches `self.pda`, so no stack contents can be corrupted by a
        // Pop-then-fail. One small stack clone per call, off the per-candidate
        // mask hot path.
        let pre_state = self.pda.state();
        let mut probe = self.pda.clone();
        for &byte in bytes {
            if probe.advance(byte).is_err() {
                return Err(DecodeError::InadmissibleToken { id });
            }
        }
        self.pda = probe;
        self.offset += bytes.len();
        // Advance the L2 scope machine in lockstep, so the next `allowed_mask`
        // narrows against the scope this token established. Skipped when L1-only.
        if let Some(schema) = &self.schema {
            self.tracker.observe(bytes, pre_state, schema);
        }
        Ok(())
    }

    /// The underlying byte-PDA at its full `(state, stack)` configuration.
    ///
    /// Exposed so a caller — or a test — can compare two sessions for a
    /// byte-identical automaton configuration, which the derived
    /// [`allowed_mask`](DecoderSession::allowed_mask) view cannot prove on its
    /// own: two different `(state, stack)` configurations can share a mask.
    #[must_use]
    pub fn pda(&self) -> &Pda {
        &self.pda
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
        self.tracker = ScopeTracker::new();
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
        self.tracker = ScopeTracker::new();
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
    fn pda_exposes_the_live_automaton_configuration() {
        use crate::grammar::pda::{Frame, State};
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        // A fresh session sits at the initial configuration…
        assert_eq!(session.pda().state(), State::Start);
        assert_eq!(session.pda().stack_top(), None);
        // …and after opening a call the accessor reflects the *real* live state
        // and stack, so it cannot be a constant / default value.
        session.accept_token(0).expect("source is admissible");
        session
            .accept_token(1)
            .expect("a step opener is admissible");
        assert_eq!(session.pda().state(), State::ExpectValue);
        assert_eq!(session.pda().stack_top(), Some(Frame::Paren));
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
    fn an_out_of_range_token_id_is_unknown_not_inadmissible() {
        // An id with no `Vocab` entry is a host-contract violation — the distinct
        // `UnknownToken`, not the mask-respecting `InadmissibleToken` an in-range
        // dead-ending token raises.
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        let err = session.accept_token(999).expect_err("no such token");
        assert!(matches!(err, DecodeError::UnknownToken { id: 999 }));
        // The reserved EOS id (== vocab.len()) is the boundary: one past it is the
        // first unknown id.
        let first_unknown = grammar.eos_bit() + 1;
        assert!(matches!(
            session.accept_token(first_unknown),
            Err(DecodeError::UnknownToken { id }) if id == first_unknown
        ));
        // An in-range closer that dead-ends stays `InadmissibleToken`.
        assert!(matches!(
            session.accept_token(3),
            Err(DecodeError::InadmissibleToken { id: 3 })
        ));
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
    fn the_recognizer_trait_reports_completeness() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let mut session = DecoderSession::new(&grammar);
        session.accept_token(0).expect("source is admissible");
        session
            .accept_token(1)
            .expect("a step opener is admissible");
        // Inside the still-open `->take(` call: not an accepting configuration,
        // read through the trait method (not the inherent one).
        assert!(!ByteRecognizer::is_complete(&session));
        session.reset();
        for id in [0u32, 1, 2, 3] {
            session.accept_token(id).expect("admissible");
        }
        // A closed, completed query: accepting, read through the trait.
        assert!(ByteRecognizer::is_complete(&session));
    }

    #[test]
    fn the_recognizer_trait_reset_restores_the_initial_configuration() {
        let grammar = CompiledGrammar::compile(token_vocab());
        let fresh = DecoderSession::new(&grammar);
        let mut session = DecoderSession::new(&grammar);
        for id in [0u32, 1, 2] {
            session.accept_token(id).expect("admissible");
        }
        // Reset through the trait must rewind the *full* configuration: offset,
        // automaton state, and the entire frame stack — not merely the offset.
        ByteRecognizer::reset(&mut session);
        assert_eq!(session.offset(), 0);
        assert_eq!(session.pda(), fresh.pda());
        // …and the per-step mask must equal a never-driven session's, bit for bit.
        let mut untouched = DecoderSession::new(&grammar);
        assert_eq!(session.allowed_mask(), untouched.allowed_mask());
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
