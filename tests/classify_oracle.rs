#![cfg(not(feature = "legend"))]
//! Always-on classifier gate for the Legend completeness probe.
//!
//! The response→outcome classifier ([`classify_return_type`]) is the pure,
//! offline-testable substance of the §8.2 completeness loop, and it lives in the
//! oracle harness under `tests/support/` (ADR-0003) rather than the published
//! crate. Pulling `support/legend.rs` in here as a crate-local module runs its
//! `#[cfg(test)] mod tests` classifier unit tests under default features (no
//! network, no docker), so the return-type/compile-error split is covered and
//! mutation-tested without shipping `serde_json` in `purecard`.
//!
//! Gated to `not(feature = "legend")`: with the `legend` feature on, the
//! `LegendClient` shim in `support/legend.rs` compiles but has no consumer here
//! (it would be dead code), so `legend_completeness.rs` — which does use the
//! client — carries the same `mod tests` in that configuration. Either way the
//! classifier tests run in exactly one binary per feature set, and the hermetic
//! `just ci` (default features) always exercises them here.

#[path = "support/legend.rs"]
mod legend;
