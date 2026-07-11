//! Throwaway byte recognizer + replay driver for the M0/M1 oracle harness.
//!
//! The [`ByteRecognizer`] contract and [`DecodeError`] now ship in the published
//! `purecard` crate (M1, `src/recognizer.rs` + `src/error.rs`); this module only
//! supplies a grammar-free [`StubDecoder`] and the [`replay_bytes`] driver that
//! prove the corpus-load + per-byte-stepping plumbing. The real byte-PDA that
//! implements `ByteRecognizer` lands in a later task and may retire this stub.

use purecard::{ByteRecognizer, DecodeError};

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
