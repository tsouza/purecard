//! Byte recognizer + replay driver — throwaway M0 wiring.
//!
//! This proves the corpus-load + per-byte-stepping plumbing. It is **not** the
//! `docs/spec/architecture.md` §9 recognizer surface: the real §8.1 soundness harness is
//! token-level and mask-based (`accept_token` / `allowed_mask`, computed by
//! speculative per-token byte-feeding with rollback), which a byte-*committing*
//! recognizer cannot express. M1 supplies that harness and may replace this
//! trait and its driver wholesale rather than extend them.

use crate::error::DecodeError;

/// A recognizer that consumes a decode stream one byte at a time.
pub trait ByteRecognizer {
    /// Advance the recognizer by one byte.
    ///
    /// This is the **single deadness channel**: it returns
    /// `Err(DecodeError::DeadState { offset, byte })` — `offset` taken from the
    /// recognizer's own consumed counter — iff the byte has no valid
    /// continuation, and `Ok(())` otherwise.
    ///
    /// # Errors
    /// Returns [`DecodeError::DeadState`] when `byte` cannot extend the stream.
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError>;

    /// Whether the recognizer is in an accepting state (EOS is legal here).
    ///
    /// A pure query used only by the caller's completeness assertion;
    /// [`replay_bytes`] never consults it. Deadness is a separate concern and
    /// reaches the caller solely through [`ByteRecognizer::accept_byte`]'s `Err`.
    fn is_complete(&self) -> bool;

    /// Return to the initial state for a fresh stream.
    fn reset(&mut self);
}

/// Grammar-free recognizer: accepts every byte, never dies, always complete.
///
/// It tracks only how many bytes it has consumed, so a caller can assert
/// byte-consumption progress (via [`StubDecoder::consumed`]).
#[derive(Debug, Default)]
pub struct StubDecoder {
    consumed: usize,
}

impl StubDecoder {
    /// Create a fresh stub recognizer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes consumed since the last reset.
    #[must_use]
    pub fn consumed(&self) -> usize {
        self.consumed
    }
}

impl ByteRecognizer for StubDecoder {
    fn accept_byte(&mut self, _byte: u8) -> Result<(), DecodeError> {
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

/// Drive `bytes` through `rec`, one byte at a time, returning the number of
/// bytes consumed on success.
///
/// Deadness is signalled solely by [`ByteRecognizer::accept_byte`] returning
/// `Err(DeadState)` — the single deadness channel — which propagates here;
/// `is_complete` is not consulted.
///
/// # Errors
/// Returns the [`DecodeError::DeadState`] produced by `rec` at the first byte
/// with no valid continuation.
pub fn replay_bytes<R: ByteRecognizer>(rec: &mut R, bytes: &[u8]) -> Result<usize, DecodeError> {
    rec.reset();
    for &byte in bytes {
        rec.accept_byte(byte)?;
    }
    Ok(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::{ByteRecognizer, StubDecoder, replay_bytes};

    #[test]
    fn stub_consumes_every_byte_and_stays_live() {
        let mut rec = StubDecoder::new();
        let consumed = replay_bytes(&mut rec, b"|Class.all()").expect("stub never dies");
        assert_eq!(consumed, "|Class.all()".len());
        assert_eq!(rec.consumed(), "|Class.all()".len());
        assert!(rec.is_complete());
    }

    #[test]
    fn reset_zeroes_the_consumed_counter() {
        let mut rec = StubDecoder::new();
        replay_bytes(&mut rec, b"abc").expect("live");
        replay_bytes(&mut rec, b"de").expect("live");
        // replay_bytes resets first, so only the second run's bytes remain.
        assert_eq!(rec.consumed(), 2);
    }
}
