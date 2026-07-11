//! The byte-recognizer surface of the decoder core.
//!
//! This ships the [`ByteRecognizer`] contract in `src/` (M1). The concrete
//! byte-PDA implementation lands in a later task (`specs/m1-l1-grammar.md`, T2+);
//! until then the oracle harness under `tests/support/` supplies a throwaway
//! `StubDecoder` that drives this trait.

use crate::error::DecodeError;

/// A recognizer that consumes a decode stream one byte at a time.
pub trait ByteRecognizer {
    /// Advance the recognizer by one byte.
    ///
    /// This is the **single deadness channel**: it returns
    /// `Err(DecodeError::DeadState { .. })` — with `offset` taken from the
    /// recognizer's own consumed counter — iff the byte has no valid
    /// continuation, and `Ok(())` otherwise.
    ///
    /// # Errors
    /// Returns [`DecodeError::DeadState`] when `byte` cannot extend the stream.
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError>;

    /// Whether the recognizer is in an accepting state (EOS is legal here).
    ///
    /// Used by the caller's completeness assertion; deadness is a separate
    /// concern and reaches the caller solely through
    /// [`ByteRecognizer::accept_byte`]'s `Err`.
    fn is_complete(&self) -> bool;

    /// Return to the initial state for a fresh stream.
    fn reset(&mut self);
}
