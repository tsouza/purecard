#![cfg(feature = "legend")]
//! Engine-backed completeness lane — opt-in (`DOMAIN.md` §8.2, §14.4).
//!
//! Compiled and run ONLY under `--features legend` (via `just test-legend`)
//! against a live Legend stack. The entire compilation unit is absent from the
//! default build graph, so the hermetic `just ci` gate never collects it — no
//! `#[ignore]`, no runtime skip, no weakened assertion.

use std::path::Path;
use std::time::Duration;

// The client + classifier live in the oracle harness (ADR-0003). Pull the module
// in as a crate-local sibling; `--features legend` compiles its `LegendClient`.
#[path = "support/legend.rs"]
mod legend;

use legend::{LegendClient, ReturnTypeOutcome};

/// The engine API base the pinned `corpus/legend-stack` exposes.
const ENGINE_BASE: &str = "http://localhost:6300/api";
/// Health-wait budget (compose sets the engine `start_period` to 60s).
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).expect("read fixture");
    serde_json::from_str(&text).expect("fixture is valid json")
}

#[test]
fn engine_client_reaches_lambda_return_type_endpoint() {
    // ponytail: the committed fixtures are provisional placeholders, so this
    // lane will 500 against a live stack until they are regenerated from a real
    // §14.2 grammarToJson -> lambdaReturnType roundtrip (M1, spec R4). At M0 the
    // assertion is the §10 done-criterion clause 2 exactly — that we can POST a
    // query and *read the result*: the call reaches the engine and returns a
    // classified outcome (a ReturnType, or a CompileError from the placeholder).
    // Asserting a specific ReturnType is deferred to M1 with the real fixtures.
    let client = LegendClient::new(ENGINE_BASE);
    client
        .health_wait(HEALTH_TIMEOUT)
        .expect("Legend engine healthy");
    let outcome = client
        .lambda_return_type(&fixture("lambda.json"), &fixture("model.json"))
        .expect("POST lambdaReturnType and read a classified result");
    assert!(matches!(
        outcome,
        ReturnTypeOutcome::ReturnType(_) | ReturnTypeOutcome::CompileError(_)
    ));
}
