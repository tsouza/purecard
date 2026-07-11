//! Error types for the M0 oracle harness.
//!
//! Two independent failure domains, kept separate so each stays honest about its
//! own surface: [`DecodeError`] for driving a byte recognizer, [`CorpusError`]
//! for loading the gold corpus.

/// An error from driving a byte recognizer.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The recognizer had no valid continuation for `byte` at `offset`.
    #[error("recognizer reached a dead state at offset {offset} (byte {byte:#04x})")]
    DeadState {
        /// Byte offset, taken from the recognizer's own consumed counter, at
        /// which deadness was reached.
        offset: usize,
        /// The byte that had no valid continuation.
        byte: u8,
    },
}

/// An error from loading the gold corpus.
#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    /// Underlying I/O failure reading the corpus file.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A corpus line failed to parse as a [`crate::corpus::GoldRecord`].
    #[error("corpus json parse error at line {line}")]
    Json {
        /// 1-based line number of the offending record.
        line: usize,
        /// The underlying `serde_json` parse error.
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::{CorpusError, DecodeError};

    #[test]
    fn dead_state_display_reports_offset_and_hex_byte() {
        let err = DecodeError::DeadState {
            offset: 7,
            byte: 0x2c,
        };
        let shown = err.to_string();
        assert!(shown.contains("offset 7"), "{shown}");
        assert!(shown.contains("0x2c"), "{shown}");
    }

    #[test]
    fn corpus_json_error_reports_line_number() {
        let source = serde_json::from_str::<serde_json::Value>("{").expect_err("malformed json");
        let err = CorpusError::Json { line: 42, source };
        assert!(err.to_string().contains("line 42"), "{err}");
    }
}
