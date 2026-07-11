//! The decode session: the byte-PDA plus the offset bookkeeping the recognizer
//! contract needs.
//!
//! [`DecoderSession`] is the shipped implementation of [`ByteRecognizer`]. It
//! wraps a [`Pda`] and a byte-offset counter, folding each byte through the
//! automaton and translating a dead state into a [`DecodeError::DeadState`]
//! carrying the offset at which the stream ran out of continuations.
//!
//! In M1 the session is L1-only: there is no schema, so `schema` is conceptually
//! always `None` (`docs/spec/architecture.md` §3.1). The `Schema` overlay and the
//! `allowed_mask` surface land in later milestones; this type exists now so the
//! oracle's soundness gate drives the *real* automaton, not a stub.

use crate::error::DecodeError;
use crate::grammar::pda::Pda;
use crate::recognizer::ByteRecognizer;

/// A byte-at-a-time decode session over the emitted-Pure grammar.
///
/// Construct one with [`DecoderSession::new`], then drive it through
/// [`ByteRecognizer`]. It reports deadness through
/// [`accept_byte`](ByteRecognizer::accept_byte) and completeness through
/// [`is_complete`](ByteRecognizer::is_complete); [`reset`](ByteRecognizer::reset)
/// returns it to a fresh stream while keeping the automaton's stack allocation.
#[derive(Debug, Clone)]
pub struct DecoderSession {
    pda: Pda,
    offset: usize,
}

impl Default for DecoderSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DecoderSession {
    /// A fresh L1-only session at the start of a stream.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pda: Pda::new(),
            offset: 0,
        }
    }

    /// The number of bytes consumed since the last [`reset`](ByteRecognizer::reset).
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl ByteRecognizer for DecoderSession {
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
    use crate::recognizer::ByteRecognizer;

    fn drive(text: &str) -> (Result<(), DecodeError>, DecoderSession) {
        let mut session = DecoderSession::new();
        let mut result = Ok(());
        for &byte in text.as_bytes() {
            if let Err(err) = session.accept_byte(byte) {
                result = Err(err);
                break;
            }
        }
        (result, session)
    }

    #[test]
    fn a_complete_query_streams_and_is_complete() {
        let (result, session) = drive("|X.all()->take(3)");
        assert!(result.is_ok());
        assert!(session.is_complete());
        assert_eq!(session.offset(), "|X.all()->take(3)".len());
    }

    #[test]
    fn a_partial_query_is_not_complete() {
        let (result, session) = drive("|X.all()->take(3");
        assert!(result.is_ok());
        assert!(!session.is_complete());
    }

    #[test]
    fn a_dead_byte_reports_offset_and_state() {
        // A closer with no matching opener dies at its own offset.
        let (result, _) = drive("|X.all())");
        let err = result.expect_err("extra ')' must dead-end");
        let DecodeError::DeadState {
            offset,
            byte,
            stack_top,
            ..
        } = err;
        assert_eq!(byte, b')');
        assert_eq!(offset, "|X.all()".len());
        assert_eq!(stack_top, "none");
    }

    #[test]
    fn reset_rewinds_offset_and_state() {
        let mut session = DecoderSession::new();
        for &byte in b"|X.all()" {
            session.accept_byte(byte).expect("live");
        }
        session.reset();
        assert_eq!(session.offset(), 0);
        assert!(!session.is_complete());
        // After reset the stream must open correctly again.
        assert!(session.accept_byte(b'x').is_err());
    }
}
