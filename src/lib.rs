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
//! Milestone **M1** (L1 byte-PDA grammar), in progress. The shippable core is
//! the [`GuaranteeLevel`] lattice, the [`vocab`] module's [`Vocab`] table
//! (token id → raw bytes), and the decoder plumbing the byte-PDA is built on:
//! the [`DecodeError`] reported when a stream dead-ends and the
//! [`ByteRecognizer`] contract the automaton implements. The concrete PDA lands
//! on top of this plumbing in a later task. The gold-corpus loader and the
//! Legend completeness probe remain test-oracle scaffolding under `tests/`, not
//! in the published crate (see
//! `docs/decisions/0003-non-core-in-tests-deplight-core.md`), so the core's only
//! runtime dependency is `thiserror` for its error types.
//!
//! The mask cache (M2), the schema overlay (M3), and the PyO3 boundary (M4) land
//! in later milestones.

pub mod error;
pub mod recognizer;
pub mod vocab;

pub use error::DecodeError;
pub use recognizer::ByteRecognizer;
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
