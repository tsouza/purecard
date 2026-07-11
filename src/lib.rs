#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! # PureCard
//!
//! A grammar- and schema-constrained decoder for **Legend Pure**. PureCard sits
//! between a language model's logits and its sampler and masks every next token
//! that cannot lead to a valid Pure query — so output is valid *by construction*
//! in a single forward pass, with no compile-repair round-trip.
//!
//! It offers two nested guarantees and deliberately refuses a third; see
//! [`GuaranteeLevel`]. The complete design — grammar, masking algorithm, schema
//! overlay, and the oracle-driven test strategy — is specified under
//! `docs/spec/`.
//!
//! ## Status
//!
//! All milestones **M0–M5** are shipped. The core is the [`GuaranteeLevel`]
//! lattice, the [`vocab`] module's [`Vocab`] table (token id → raw bytes), the
//! byte-level recogniser (the [`grammar`] module's hand-written pushdown
//! automaton [`Pda`] over the emitted-Pure grammar (§5), the [`DecoderSession`]
//! that drives it as a [`ByteRecognizer`], and the [`DecodeError`] it reports),
//! the M2 mask cache ([`CompiledGrammar`]), and the M3 [`schema`] overlay:
//! [`Schema::from_json`] ingests the host contract as JSON and
//! [`DecoderSession::with_schema`] narrows the mask to schema-legal terminals at
//! each identifier/operand position. The gold-corpus loader and the Legend
//! completeness probe remain test-oracle scaffolding under `tests/` (see
//! `docs/decisions/0003-non-core-in-tests-deplight-core.md`); the core's runtime
//! dependencies are `thiserror` (error types) and `serde`/`serde_json` (the L2
//! JSON ingress, ADR-0005).
//!
//! Milestone **M4** adds the PyO3 boundary: the feature-gated `ffi` module
//! (compiled only under `--features python`) marshals the core to a Python
//! `purecard` extension module — a thin, decode-logic-free surface packaged as a
//! maturin abi3 wheel. The default build stays pyo3-free and pure.
//!
//! Milestone **M5** is the hardening pass: the [`selfcheck`] surface
//! ([`self_check`], [`self_check_smoke`], [`SelfCheckError`]) round-trips a host
//! tokenizer against the vocabulary before decode; [`accept_token`] finalizes on
//! the reserved EOS id (accepted only in an accepting configuration); the
//! [`DecodeError`] token-level channel is split into
//! [`InadmissibleToken`](DecodeError::InadmissibleToken),
//! [`UnknownToken`](DecodeError::UnknownToken), and
//! [`UnexpectedEos`](DecodeError::UnexpectedEos); and the fuzz targets and
//! benches under `fuzz/` and `benches/` guard the decoder against regressions.
//!
//! [`accept_token`]: DecoderSession::accept_token

pub mod error;
pub mod grammar;
pub mod mask;
pub mod recognizer;
pub mod schema;
pub mod selfcheck;
pub mod session;
pub mod vocab;

// The Python extension surface (M4): a private, feature-gated boundary module.
// Its items are not part of the Rust public API — they are reachable only from
// Python via the generated `purecard` module — so `deny(missing_docs)` does not
// reach them, but they are documented all the same.
#[cfg(feature = "python")]
mod ffi;

pub use error::DecodeError;
pub use grammar::Envelope;
pub use grammar::compiled::CompiledGrammar;
pub use grammar::pda::Pda;
pub use mask::BitMask;
pub use recognizer::ByteRecognizer;
pub use schema::{Schema, SchemaError};
pub use selfcheck::{SelfCheckError, self_check, self_check_smoke};
pub use session::DecoderSession;
pub use vocab::Vocab;

/// The nested guarantee levels a constrained decoder can offer, ordered from
/// weakest (the largest set of queries) to strongest (the smallest).
///
/// The sets form a strict containment hierarchy — every faithful query is
/// schema-consistent, and every schema-consistent query is syntactic, but not
/// the reverse:
///
/// ```text
/// faithful ⊂ schema-consistent ⊂ syntactic
/// ```
///
/// PureCard moves a model's output into [`Syntactic`](GuaranteeLevel::Syntactic)
/// (L1) and, when given a schema, into
/// [`SchemaConsistent`](GuaranteeLevel::SchemaConsistent) (L2). It *cannot* reach
/// [`Faithful`](GuaranteeLevel::Faithful) (L3): a logits mask sees the schema and
/// the partial output, but never the question's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuaranteeLevel {
    /// **L1.** The query parses as (emitted-subset) Pure.
    Syntactic,
    /// **L2.** Every identifier and type resolves against a specific model: no
    /// phantom classes or properties, no type mismatch.
    SchemaConsistent,
    /// **L3.** The query actually answers the question that was asked.
    /// Structurally unreachable at decode time — PureCard never claims it.
    Faithful,
}

impl GuaranteeLevel {
    /// The strongest guarantee PureCard can enforce at decode time:
    /// schema-consistency (L2). [`Faithful`](GuaranteeLevel::Faithful) (L3) is
    /// out of reach by construction.
    pub const MAX_ENFORCEABLE: GuaranteeLevel = GuaranteeLevel::SchemaConsistent;

    /// Whether holding `self` also guarantees the weaker property `other`.
    ///
    /// Follows the containment hierarchy: a schema-consistent query is also
    /// syntactic, so `SchemaConsistent.guarantees(Syntactic)` is `true`, but
    /// `Syntactic.guarantees(SchemaConsistent)` is not.
    #[must_use]
    pub fn guarantees(self, other: GuaranteeLevel) -> bool {
        self >= other
    }

    /// Whether PureCard can actually enforce this level — i.e. it is no stronger
    /// than [`MAX_ENFORCEABLE`](GuaranteeLevel::MAX_ENFORCEABLE). L3 is not
    /// enforceable.
    #[must_use]
    pub fn is_enforceable(self) -> bool {
        self <= Self::MAX_ENFORCEABLE
    }
}

#[cfg(test)]
mod tests {
    use super::GuaranteeLevel;
    use super::GuaranteeLevel::{Faithful, SchemaConsistent, Syntactic};

    #[test]
    fn containment_orders_weakest_to_strongest() {
        assert!(Syntactic < SchemaConsistent);
        assert!(SchemaConsistent < Faithful);
    }

    #[test]
    fn a_stronger_guarantee_implies_every_weaker_one() {
        assert!(SchemaConsistent.guarantees(Syntactic));
        assert!(SchemaConsistent.guarantees(SchemaConsistent));
        assert!(Faithful.guarantees(SchemaConsistent));
        assert!(Faithful.guarantees(Syntactic));
    }

    #[test]
    fn a_weaker_guarantee_does_not_imply_a_stronger_one() {
        assert!(!Syntactic.guarantees(SchemaConsistent));
        assert!(!Syntactic.guarantees(Faithful));
        assert!(!SchemaConsistent.guarantees(Faithful));
    }

    #[test]
    fn purecard_enforces_up_to_schema_consistency_only() {
        assert!(Syntactic.is_enforceable());
        assert!(SchemaConsistent.is_enforceable());
        assert!(!Faithful.is_enforceable());
        assert_eq!(GuaranteeLevel::MAX_ENFORCEABLE, SchemaConsistent);
    }
}
