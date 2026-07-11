//! L1: the emitted-Pure grammar as a byte-level pushdown automaton (§5).
//!
//! [`pda`] holds the live automaton — the [`State`](pda::State) /
//! [`Frame`](pda::Frame) machine and its pure [`step`](pda::step) function. This
//! module adds the two things the automaton itself does not carry: the
//! [`Envelope`] classifier that names which of the two corpus idioms a query
//! belongs to, and the [`DeadState`] carrier the [`Pda`](pda::Pda) hands back on
//! rejection.
//!
//! There is no compiled-EBNF `spec.rs` yet: M2 ships the recogniser plus the
//! [`CompiledGrammar`] vocabulary/mask cache (`docs/spec/architecture.md` §4),
//! but real EBNF spec compilation (§5) is a later milestone, so
//! [`CompiledGrammar::from_spec`] is a stub over the single fixed M1 PDA.

pub mod compiled;
pub mod pda;

pub use compiled::CompiledGrammar;

/// The automaton configuration at the point a byte was rejected: the names of the
/// current [`State`](pda::State) and the top [`Frame`](pda::Frame).
///
/// [`Pda::advance`](pda::Pda::advance) returns this on a dead state; the
/// [`DecoderSession`](crate::DecoderSession) pairs it with the byte offset to
/// build a [`DecodeError::DeadState`](crate::DecodeError::DeadState). Both fields
/// are `&'static str` names, not the enums themselves, so the error type stays
/// free of the automaton's internal vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadState {
    /// The name of the [`State`](pda::State) the automaton was in.
    pub state: &'static str,
    /// The name of the top [`Frame`](pda::Frame), or `"none"` for an empty stack.
    pub stack_top: &'static str,
}

/// Which of the two observed corpus idioms a query uses (§5.1).
///
/// The two envelopes are mechanically distinguishable and non-overlapping in the
/// gold corpus: an arm-A query is the relational `tableReference(…)->tableToTDS()`
/// pipeline, an arm-C query is the class-navigation `Class.all()->…` form. The
/// soundness gate partitions the corpus by this classifier and asserts an exact
/// record count per arm (`specs/m1-l1-grammar.md`, G2), so a mis-partitioned or
/// missing query reddens the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope {
    /// Arm A — the relational TDS envelope (`…->tableReference(…)->tableToTDS()`).
    Relational,
    /// Arm C — the class-navigation envelope (`…Class.all()->…`).
    ClassNav,
}

/// The marker substring of the arm-A relational envelope.
const RELATIONAL_MARKER: &str = "tableReference";
/// The marker substring of the arm-C class-navigation envelope.
const CLASS_NAV_MARKER: &str = ".all()";

impl Envelope {
    /// Classify a query by its envelope marker.
    ///
    /// Returns [`Relational`](Envelope::Relational) if the query contains the
    /// `tableReference` marker, [`ClassNav`](Envelope::ClassNav) if it contains
    /// the `.all()` marker, and `None` if neither (which, over the all-gold
    /// corpus, cannot happen and so fails the soundness gate's per-arm tally).
    /// The two markers are mutually exclusive across the corpus, so the order of
    /// the checks does not change any gold classification.
    #[must_use]
    pub fn classify(query: &str) -> Option<Envelope> {
        if query.contains(RELATIONAL_MARKER) {
            Some(Envelope::Relational)
        } else if query.contains(CLASS_NAV_MARKER) {
            Some(Envelope::ClassNav)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Envelope;

    #[test]
    fn relational_query_classifies_as_arm_a() {
        let query = "|db::Db->tableReference('default','T')->tableToTDS()->limit(1)";
        assert_eq!(Envelope::classify(query), Some(Envelope::Relational));
    }

    #[test]
    fn class_nav_query_classifies_as_arm_c() {
        let query = "|spider::geo::model::default::River.all()->project([x|$x.n], ['n'])";
        assert_eq!(Envelope::classify(query), Some(Envelope::ClassNav));
    }

    #[test]
    fn a_query_with_neither_marker_is_unclassified() {
        assert_eq!(Envelope::classify("|X->foo()"), None);
    }
}
