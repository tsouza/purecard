//! Legend Engine completeness probe (`DOMAIN.md` §8.2, §14).
//!
//! The response→outcome classification — the actual point of the completeness
//! loop — is a pure, always-compiled function ([`classify_return_type`]), so it
//! is covered and mutation-tested. Only the live-HTTP client (`LegendClient`)
//! is behind the `legend` feature; it is a thin `ureq` shim that delegates the
//! decision back to the pure classifier.

use serde_json::Value;

/// Outcome of a `/pure/v1/compilation/lambdaReturnType` compile probe.
#[derive(Debug, PartialEq, Eq)]
pub enum ReturnTypeOutcome {
    /// Compiled: the lambda's return type (e.g. `"TabularDataSet"`).
    ReturnType(String),
    /// Failed to compile: the engine's error payload.
    CompileError(String),
}

/// Classify a `lambdaReturnType` response body as a return type or a compile
/// error.
///
/// Pure and default-feature (no I/O), so it is unit- and mutation-tested against
/// canned success/error JSON: a mutant swapping the two arms cannot survive.
///
// ponytail: the response shape is provisional — a flat `returnType` string on
// success, a `message` on error. Regenerate the fixtures (and tighten this) from
// a real §14.2 `/lambdaReturnType` roundtrip when the engine lane is stood up
// (spec risk R4).
#[must_use]
pub fn classify_return_type(resp: &Value) -> ReturnTypeOutcome {
    match resp.get("returnType").and_then(Value::as_str) {
        Some(return_type) if !return_type.is_empty() => {
            ReturnTypeOutcome::ReturnType(return_type.to_owned())
        }
        _ => {
            let message = resp
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| resp.to_string(), str::to_owned);
            ReturnTypeOutcome::CompileError(message)
        }
    }
}

#[cfg(feature = "legend")]
pub use client::LegendClient;

#[cfg(feature = "legend")]
mod client {
    use super::{ReturnTypeOutcome, classify_return_type};
    use serde_json::Value;
    use std::time::{Duration, Instant};

    /// Path of the compile endpoint, relative to the engine API base.
    const COMPILE_PATH: &str = "/pure/v1/compilation/lambdaReturnType";
    /// Path of the engine health endpoint, relative to the base.
    const INFO_PATH: &str = "/server/v1/info";
    /// Delay between health-poll attempts.
    const POLL_INTERVAL: Duration = Duration::from_secs(2);

    /// Blocking client for the Legend Engine compile contract (§14).
    pub struct LegendClient {
        base: String,
    }

    impl LegendClient {
        /// Create a client for the engine API base, e.g.
        /// `"http://localhost:6300/api"`.
        pub fn new(base: impl Into<String>) -> Self {
            Self { base: base.into() }
        }

        /// Poll the engine `/server/v1/info` endpoint until it answers `2xx` or
        /// `timeout` elapses.
        ///
        /// Only the engine is polled: the canned-fixture `lambdaReturnType` lane
        /// never touches sdlc, so there is no second service to wait on.
        ///
        /// `http_status_as_error` is disabled so a non-2xx health response
        /// surfaces as `Ok(resp)` and the `is_success()` guard below decides
        /// readiness, rather than `.call()` short-circuiting to `Err` on 5xx.
        ///
        /// # Errors
        /// Returns an error if the engine is not healthy within `timeout`.
        pub fn health_wait(&self, timeout: Duration) -> anyhow::Result<()> {
            let url = format!("{}{INFO_PATH}", self.base);
            let deadline = Instant::now() + timeout;
            loop {
                if let Ok(resp) = ureq::get(&url)
                    .config()
                    .http_status_as_error(false)
                    .build()
                    .call()
                    && resp.status().is_success()
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    anyhow::bail!("engine not healthy at {url} within {timeout:?}");
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }

        /// POST `{lambda, model}` to `/pure/v1/compilation/lambdaReturnType` and
        /// classify the response as a return type or a compile error.
        ///
        /// `http_status_as_error` is disabled so a compile-failure response
        /// (HTTP 500 with an error body) reaches [`classify_return_type`] as a
        /// [`ReturnTypeOutcome::CompileError`], instead of `send_json` dropping
        /// the body and returning a bare status error.
        ///
        /// # Errors
        /// Returns an error if the request fails or the body is not JSON.
        pub fn lambda_return_type(
            &self,
            lambda: &Value,
            model: &Value,
        ) -> anyhow::Result<ReturnTypeOutcome> {
            let url = format!("{}{COMPILE_PATH}", self.base);
            let mut body = serde_json::Map::new();
            body.insert("lambda".to_owned(), lambda.clone());
            body.insert("model".to_owned(), model.clone());
            let mut resp = ureq::post(&url)
                .config()
                .http_status_as_error(false)
                .build()
                .send_json(Value::Object(body))?;
            let value: Value = resp.body_mut().read_json()?;
            Ok(classify_return_type(&value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReturnTypeOutcome, classify_return_type};

    fn value(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("valid test json")
    }

    #[test]
    fn classifies_a_return_type_as_success() {
        let resp = value(r#"{"returnType":"TabularDataSet"}"#);
        assert_eq!(
            classify_return_type(&resp),
            ReturnTypeOutcome::ReturnType("TabularDataSet".to_owned())
        );
    }

    #[test]
    fn classifies_an_error_payload_as_compile_error() {
        let resp = value(r#"{"status":500,"message":"Can't find property 'maker'"}"#);
        assert_eq!(
            classify_return_type(&resp),
            ReturnTypeOutcome::CompileError("Can't find property 'maker'".to_owned())
        );
    }

    #[test]
    fn treats_an_empty_return_type_as_error() {
        assert!(matches!(
            classify_return_type(&value(r#"{"returnType":""}"#)),
            ReturnTypeOutcome::CompileError(_)
        ));
    }

    #[test]
    fn compile_error_without_message_falls_back_to_the_full_body() {
        // No `message` and no usable `returnType`, so the error string must be
        // the whole response body — pins the `map_or_else` default branch
        // (`|| resp.to_string()`) against a `-> String::new()` mutant.
        let ReturnTypeOutcome::CompileError(detail) =
            classify_return_type(&value(r#"{"returnType":""}"#))
        else {
            panic!("empty returnType with no message must be a compile error");
        };
        assert!(!detail.is_empty(), "{detail}");
        assert!(detail.contains("returnType"), "{detail}");
    }
}
