//! The compiled grammar: a [`Vocab`] plus the lazy per-state token-mask cache
//! that makes the per-step allowed set cheap (§4).
//!
//! A naive allowed-mask recomputes, at every decode step, which of ~150k vocab
//! tokens keep the byte-PDA alive — millions of [`step`](super::pda::step) calls,
//! far over the few-hundred-µs budget. [`CompiledGrammar`] follows the
//! xgrammar-style split (§4.2): at each reachable [`State`] it partitions the
//! vocabulary into
//!
//! - **context-independent survivors** — tokens whose admissibility depends only
//!   on the state, cached once as a [`BitMask`] (`indep`); and
//! - **context-dependent** tokens — bare closers `)]}`  and `,`/`;`/`*` whose
//!   admissibility depends on the live stack, kept as a small `deferred` id list
//!   the session re-probes per step against the real stack.
//!
//! The cache is **lazy** (§4.5): a [`State`]'s partition is built on first visit
//! and only for states a decode actually reaches. It is interior-mutable
//! ([`OnceCell`]) so a shared `&CompiledGrammar` can fill it as
//! [`DecoderSession`](crate::DecoderSession) drives.

use std::cell::OnceCell;

use crate::grammar::pda::{Pda, State};
use crate::mask::BitMask;
use crate::vocab::Vocab;

/// A grammar compiled against a specific model vocabulary: the vocab itself plus
/// the lazy per-[`State`] mask cache (§4).
///
/// M2 wraps the single fixed M1 byte-PDA; [`from_spec`](CompiledGrammar::from_spec)
/// is a stub until EBNF compilation (§5) lands. Build one per `(model, grammar)`
/// pair and share it across sessions.
#[derive(Debug)]
pub struct CompiledGrammar {
    vocab: Vocab,
    /// One lazily-filled partition per automaton state, indexed by
    /// [`State::index`]. `OnceCell` gives the interior mutability that lets a
    /// shared `&self` fill a state's entry on first visit.
    cache: Vec<OnceCell<Cached>>,
}

/// A single state's memoized vocabulary partition (§4.2).
#[derive(Debug)]
pub(crate) struct Cached {
    /// The context-independent survivors at this state — admissible regardless of
    /// the stack, so cacheable and copied wholesale into the per-step mask.
    pub(crate) indep: BitMask,
    /// Token ids whose admissibility at this state depends on the live stack;
    /// the session re-probes each against the real stack per step. `|deferred|`
    /// is tiny next to the vocabulary.
    pub(crate) deferred: Box<[u32]>,
}

impl CompiledGrammar {
    /// Compile the fixed M1 byte-PDA against `vocab`, sizing the (empty) lazy
    /// per-state cache. No token is probed here — every state's partition is
    /// built on first visit (§4.5).
    #[must_use]
    pub fn compile(vocab: Vocab) -> Self {
        let cache = (0..State::COUNT).map(|_| OnceCell::new()).collect();
        Self { vocab, cache }
    }

    /// **Stub.** Real EBNF spec compilation (§5) is a later milestone; today this
    /// ignores `spec` and returns the single fixed M1 PDA-backed grammar compiled
    /// against `vocab`, so the M2 masking path can be exercised through the
    /// eventual API shape.
    #[must_use]
    pub fn from_spec(_spec: &str, vocab: Vocab) -> Self {
        Self::compile(vocab)
    }

    /// The vocabulary this grammar was compiled against.
    #[must_use]
    pub fn vocab(&self) -> &Vocab {
        &self.vocab
    }

    /// The reserved EOS bit position: the id one past the last real token
    /// (Decision D3). The per-step mask spans `vocab.len() + 1` bits.
    pub(crate) fn eos_bit(&self) -> u32 {
        self.vocab.len() as u32
    }

    /// The memoized partition for `state`, built on first access (§4.5).
    pub(crate) fn cached(&self, state: State) -> &Cached {
        self.cache[state.index()].get_or_init(|| build(state, &self.vocab))
    }
}

/// Build a state's vocabulary partition by probing every token from the state
/// over an **empty** stack (§4.2). A token that stays alive is a
/// context-independent survivor; one that dies consulting the ambient stack is
/// deferred; one that dies outright is admissible from no stack and is dropped.
fn build(state: State, vocab: &Vocab) -> Cached {
    let base = Pda::at(state);
    let mut indep = BitMask::with_len(vocab.len() + 1);
    let mut deferred = Vec::new();
    let mut scratch = Vec::new();
    for id in 0..vocab.len() as u32 {
        let bytes = vocab.bytes(id).unwrap_or(&[]);
        let probe = base.probe(bytes, &mut scratch);
        if probe.consulted_ambient {
            deferred.push(id);
        } else if probe.alive {
            indep.set(id);
        }
    }
    Cached {
        indep,
        deferred: deferred.into_boxed_slice(),
    }
}

#[cfg(test)]
mod tests {
    use super::CompiledGrammar;
    use crate::grammar::pda::State;
    use crate::vocab::Vocab;

    fn vocab() -> Vocab {
        // A closer (context-dependent), an identifier (independent survivor from
        // a value position), and a bare `,` (dead in a value position — a
        // separator is not a value — regardless of any enclosing frame).
        Vocab::from_byte_tokens(
            vec![b")".to_vec(), b"name".to_vec(), b",".to_vec(), b"".to_vec()],
            3,
        )
    }

    #[test]
    fn compile_sizes_a_lazy_cache_per_state() {
        let grammar = CompiledGrammar::compile(vocab());
        assert_eq!(grammar.vocab().len(), 4);
        assert_eq!(grammar.eos_bit(), 4);
    }

    #[test]
    fn from_spec_ignores_the_spec_and_wraps_the_fixed_pda() {
        let grammar = CompiledGrammar::from_spec("ignored ebnf", vocab());
        // Same partition as `compile` — the spec argument is a no-op stub today.
        let cached = grammar.cached(State::ExpectValue);
        assert!(
            cached.indep.test(1),
            "an identifier survives a value position"
        );
    }

    #[test]
    fn build_partitions_closers_survivors_and_dead_tokens() {
        let grammar = CompiledGrammar::compile(vocab());
        let cached = grammar.cached(State::ExpectValue);
        // `name` (id 1) and the empty token (id 3) survive independently…
        assert!(cached.indep.test(1));
        assert!(cached.indep.test(3));
        // …a bare `,` (id 2) is dead in a value position and is dropped…
        assert!(!cached.indep.test(2));
        assert!(!cached.deferred.contains(&2));
        // …and the closer `)` (id 0) is deferred to the live-stack re-probe.
        assert!(!cached.indep.test(0));
        assert!(cached.deferred.contains(&0));
    }

    #[test]
    fn cached_is_memoized_across_visits() {
        let grammar = CompiledGrammar::compile(vocab());
        let first = grammar.cached(State::AfterValue) as *const _;
        let second = grammar.cached(State::AfterValue) as *const _;
        assert_eq!(
            first, second,
            "a state's partition is built once and reused"
        );
    }
}
