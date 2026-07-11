# Spec: M0 — Oracle harness

- **Status:** Draft — awaiting human sign-off on "Decisions for the human"
- **Created:** 2026-07-10
- **Owner:** Thiago (tsouza)

## Problem

PureCard is a byte-level grammar/schema-constrained decoder for Legend Pure. Before any real grammar (M1), mask cache (M2), or schema overlay (M3) exists, we need a **skeleton oracle harness** that proves the two load-bearing feedback loops are wired end to end:

1. **Offline soundness replay** — every execution-verified gold Pure lambda in the corpus can be driven, byte by byte, through a throwaway byte recognizer, without ever dead-ending. This is the loop that will later catch a PDA that rejects a byte sequence the corpus actually produces.
2. **Online completeness probe** — a gold lambda can be POSTed to Legend Engine's `/pure/v1/compilation/lambdaReturnType` and its return-type-vs-error result read back. This is the loop that will later measure the 100%-compile completeness gate.

The constraint that shapes the whole design: **no model tokenizer/vocab file ships anywhere in the repo** (filesystem-verified). §8.1's real soundness test is **token-level and mask-based** — it feeds the gold's actual tokens and asserts each stays in `allowed_mask()` over ~150k tokens, a mask computed by speculative per-token byte-feeding with rollback. That is impossible at M0, and the M0 byte-*committing* recognizer cannot express it. So M0's byte-level replay is a **harness-wiring proof for the future §8.1 token-level test**, not that test: feed the gold's raw UTF-8 bytes one at a time and assert the recognizer never reaches a dead state. It proves the corpus-load + per-byte-stepping plumbing works; it does **not** prove token-id soundness.

M0 is a skeleton. The done-criterion (§10) is exactly: *"the harness can replay a gold query through a STUB decoder and can POST a query to `/pure/v1/compilation/lambdaReturnType` and read the result."* We build the smallest correct scaffold that satisfies both clauses. The byte recognizer + `replay_bytes` are **throwaway wiring scaffolding**: the real §8.1 harness is token-level/mask-based, arrives with M1, and MAY replace this wiring wholesale rather than extend it.

## Goals

Each maps to a clause of the §10 done-criterion.

- [ ] **G1 (replay clause).** The harness streams all ~5,034 gold records from `corpus/gold_queries.jsonl`, drives each `pure_text` byte-by-byte through a `StubDecoder`, and asserts the recognizer is never dead after any byte and is complete at end — as an always-on default-feature test.
- [ ] **G2 (wiring clause).** The `ByteRecognizer` trait + `StubDecoder` + `replay_bytes` are throwaway M0 scaffolding that prove the corpus-load + per-byte-stepping plumbing. They are **not** the §9 recognizer surface: the real M1 soundness harness is token-level/mask-based (`accept_token` / `allowed_mask`, computed by speculative per-token byte-feeding with rollback), which the byte-committing `StubDecoder` cannot express. M1 supplies that harness and MAY replace this wiring, not merely swap the recognizer body. `is_complete` / `reset` are the only shapes M0 shares with §9.
- [ ] **G3 (POST clause).** A feature-gated engine client POSTs a canned `{lambda, model}` fixture to `/pure/v1/compilation/lambdaReturnType`, distinguishes a returned type from a compile error, and health-waits the engine only first (the canned-fixture `lambdaReturnType` lane never needs sdlc).
- [ ] **G4 (honesty).** The default gate (`just ci`) is green with no network, no docker, no skipped/ignored tests, no weakened assertions, and ≥70% coverage on all default-feature code.

## Non-goals

Explicitly **out of scope** for M0 — deferred, not forgotten:

- **M1 grammar** — real Pure productions, the byte-PDA, EBNF/grammar spec, any non-trivial recognizer transition. The M0 recognizer accepts every byte.
- **M2 cache** — token trie, per-state mask cache, `allowed_mask()`, any `BitMask` over a vocab. *The byte trie is M2, not M0* — nothing at M0 enumerates a vocab, so a trie has no consumer yet.
- **L2 schema overlay** — schema JSON ingestion, scope tracker, phantom-identifier / type checks in Rust (the engine does the only completeness check at M0).
- **Token-id-level replay / vocab** — no tokenizer ships, so replay is byte-level only. Token-id soundness (masking whole BPE units, verifying a specific id stays in `allowed_mask()` at a boundary) is deferred to M1+ once a host supplies a vocab. `Vocab` is scaffolded (id→bytes + eos) but has no trie and no mask; `accept_token(id)` is a later `bytes(id).for_each(accept_byte)` loop.
- **PyO3 / FFI** — no `ffi.rs`, no Python binding.
- **PMCD regeneration, fs-SDLC entity push, grammarToJson** — the engine test feeds a canned protocol-JSON + PMCD fixture; regenerating stores/models is M1+.
- **The real §8.1 soundness harness** — token-level and mask-based (`accept_token` / `allowed_mask` with speculative per-token rollback). M0's byte-committing recognizer cannot express it; M1 brings it and MAY discard the M0 wiring entirely. The M0 byte replay is a plumbing proof, not a §8.1 stand-in (see R1).

## Design

### Module layout

The published `purecard` core stays dep-light and harness-free (ADR-0003): only
the pieces a downstream consumer needs live in `src/`. Every oracle-harness
module — the ones that pull in `serde`/`serde_json`/`ureq`/`anyhow` — lives under
`tests/support/` and is compiled into the integration-test binaries via `#[path]`,
so it never enters the published crate's dependency graph (`just check-core-deplight`
enforces this).

```text
src/lib.rs                    crate root: #![forbid(unsafe_code)] #![deny(missing_docs)];
                              GuaranteeLevel lattice + re-exports of the dep-light core.
src/vocab.rs                  Vocab (id→bytes + eos). No trie.
tests/support/error.rs        thiserror error types (DecodeError, CorpusError).
tests/support/recognizer.rs   ByteRecognizer trait + StubDecoder + replay_bytes helper.
tests/support/corpus.rs       GoldRecord (serde) + streaming load_gold(path) iterator.
tests/support/legend.rs       default: ReturnTypeOutcome + pure classify_return_type + the
                              URL-join helper; the LegendClient ureq shim is
                              #[cfg(feature = "legend")].
tests/soundness_replay.rs     always-on wiring/liveness gate (default features).
tests/classify_oracle.rs      runs the classifier unit tests under default features.
tests/legend_completeness.rs  opt-in engine lane (#![cfg(feature = "legend")]).
```

Rationale: this is the "minimal vertical slice" spine — one module per concept, no
`DecoderSession` wrapper (see Decisions). Splitting errors into `DecodeError`
(recognizer domain) and `CorpusError` (loader/IO domain) keeps each type honest
about its own failure surface and matches the two independent concerns the harness
bolts together.

### Byte-level soundness approach (no shipped vocab)

Because no tokenizer ships, the primitive is **`accept_byte`, not `accept_token`**. Replay feeds `pure_text.as_bytes()` through a fresh recognizer per record. This is a **harness-wiring proof for the future §8.1 token-level test**, not that test: it proves the recognizer *admits every byte sequence the corpus actually produces*. It does **not** prove token-id soundness (a byte-admissible stream can still mask a specific token id at a BPE boundary) — that is the M1+ check, and this spec says so out loud so no one mistakes the green M0 gate for the real §8.1 guarantee.

`serde_json` reads the corpus line-by-line via `BufRead::lines` → `from_str` per line — no whole-file load of the ~4.7 MB file, and full JSON string unescaping (the `\n`/nested-quote content inside `pure_text`) comes for free rather than from a hand-rolled splitter.

### Stub decoder interface

```rust
// error.rs
/// Errors from driving a byte recognizer.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The recognizer had no valid continuation for `byte` at `offset`.
    #[error("recognizer reached a dead state at offset {offset} (byte {byte:#04x})")]
    DeadState { offset: usize, byte: u8 },
}

/// Errors from loading the gold corpus.
#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    /// Underlying I/O failure reading the corpus file.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A corpus line failed to parse as a `GoldRecord`.
    #[error("corpus json parse error at line {line}")]
    Json { line: usize, #[source] source: serde_json::Error },
}
```

```rust
// recognizer.rs
/// A recognizer that consumes a decode stream one byte at a time.
///
/// Throwaway M0 wiring: it proves the corpus-load + per-byte-stepping
/// plumbing. It is **not** the §9 recognizer surface — M1's soundness
/// harness is token-level/mask-based (`accept_token`/`allowed_mask`) and
/// may replace this trait and its driver wholesale.
pub trait ByteRecognizer {
    /// Advance the recognizer by one byte. This is the **single deadness
    /// channel**: it returns `Err(DecodeError::DeadState { offset, byte })`
    /// (offset from the recognizer's own consumed counter) iff the byte has
    /// no valid continuation, and `Ok(())` otherwise.
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError>;
    /// True iff the recognizer is in an accepting state (EOS is legal here).
    /// A pure query used by the caller's completeness assertion, not by
    /// `replay_bytes`. Deadness reaches the caller solely through
    /// `accept_byte`'s `Err` — there is no separate `is_dead` channel.
    fn is_complete(&self) -> bool;
    /// Return to the initial state for a fresh stream.
    fn reset(&mut self);
}

/// Grammar-free recognizer: accepts every byte, never dies, always complete.
/// It tracks only how many bytes it has consumed, so assertions have
/// something real to check (a no-op stub would be mutation-invisible).
#[derive(Debug, Default)]
pub struct StubDecoder { consumed: usize }

impl StubDecoder {
    /// Create a fresh stub recognizer.
    pub fn new() -> Self { Self::default() }
    /// Bytes consumed since the last reset.
    pub fn consumed(&self) -> usize { self.consumed }
}

impl ByteRecognizer for StubDecoder {
    fn accept_byte(&mut self, _byte: u8) -> Result<(), DecodeError> {
        self.consumed += 1;
        Ok(())
    }
    fn is_complete(&self) -> bool { true }
    fn reset(&mut self) { self.consumed = 0; }
}

/// Drive `bytes` through `rec`, one byte at a time. Deadness is signalled
/// solely by `accept_byte` returning `Err(DeadState)` — the single deadness
/// channel — which propagates here; `is_complete` is not consulted.
/// Returns the number of bytes consumed on success.
pub fn replay_bytes<R: ByteRecognizer>(
    rec: &mut R,
    bytes: &[u8],
) -> Result<usize, DecodeError> {
    rec.reset();
    for &byte in bytes {
        rec.accept_byte(byte)?;
    }
    Ok(bytes.len())
}
```

`replay_bytes` lives in the library (not the test) so it is unit-testable and mutation-covered. It is M0-only wiring: M1's token-level §8.1 harness may replace it rather than reuse it.

### Vocab (scaffold only, no trie)

```rust
// vocab.rs
/// An indexed table mapping token ids to their raw bytes, plus the EOS id.
/// M0 has no consumer that enumerates it; the mask trie is M2.
#[derive(Debug, Clone)]
pub struct Vocab { tokens: Vec<Vec<u8>>, eos: u32 }

impl Vocab {
    /// Build from a list of token byte-strings and the EOS token id.
    pub fn from_byte_tokens(tokens: Vec<Vec<u8>>, eos: u32) -> Self { Self { tokens, eos } }
    /// Raw bytes for token `id`, or `None` if out of range.
    pub fn bytes(&self, id: u32) -> Option<&[u8]> { self.tokens.get(id as usize).map(Vec::as_slice) }
    /// The EOS token id.
    pub fn eos(&self) -> u32 { self.eos }
    /// Number of tokens in the table.
    pub fn len(&self) -> usize { self.tokens.len() }
    /// True iff the table is empty.
    pub fn is_empty(&self) -> bool { self.tokens.is_empty() }
}
```

### Corpus loader

```rust
// corpus.rs
/// One line of `corpus/gold_queries.jsonl` (§13.1 schema).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GoldRecord {
    pub db_id: String,
    pub source_id: String,
    pub arm: String,
    pub constructs: Vec<String>,
    pub pure_text: String,
}

/// Stream gold records from `path`, one parsed record per line, lazily.
/// Never loads the whole file; each line is `from_str`-parsed on demand.
pub fn load_gold(
    path: &std::path::Path,
) -> Result<impl Iterator<Item = Result<GoldRecord, CorpusError>>, CorpusError>;
```

### Engine classification (default feature) + client (feature-gated, `ureq`)

The response→outcome classification — the actual point of the completeness
loop — is a **default-feature pure function** with no `ureq`, so it is
covered and mutation-tested. Only the live HTTP shim is feature-gated.

```rust
// legend.rs — default feature, no ureq
use serde_json::Value;

/// Outcome of a `lambdaReturnType` compile probe.
#[derive(Debug, PartialEq, Eq)]
pub enum ReturnTypeOutcome {
    /// Compiled: the lambda's return type (e.g. "TabularDataSet").
    ReturnType(String),
    /// Failed to compile: the engine's error payload.
    CompileError(String),
}

/// Classify a `lambdaReturnType` response body as a return type or a compile
/// error. Pure, default-feature, no I/O — unit- and mutation-tested against
/// canned success/error JSON so a mutant swapping the arms cannot survive.
pub fn classify_return_type(resp: &Value) -> ReturnTypeOutcome { /* … */ }
```

```rust
// legend.rs — #[cfg(feature = "legend")] block: the thin ureq I/O shim
use std::time::Duration;

/// Blocking client for the Legend Engine compile contract (§14).
#[cfg(feature = "legend")]
pub struct LegendClient { base: String }

#[cfg(feature = "legend")]
impl LegendClient {
    /// Base URL, e.g. "http://localhost:6300/api".
    pub fn new(base: impl Into<String>) -> Self { Self { base: base.into() } }

    /// Poll engine `/server/v1/info` until 200 or timeout. Engine only: the
    /// canned-fixture `lambdaReturnType` lane never needs sdlc.
    pub fn health_wait(&self, timeout: Duration) -> anyhow::Result<()>;

    /// POST `{lambda, model}` to `/pure/v1/compilation/lambdaReturnType`,
    /// then delegate to `classify_return_type` for the return-type/error split.
    pub fn lambda_return_type(
        &self, lambda: &Value, model: &Value,
    ) -> anyhow::Result<ReturnTypeOutcome>;
}
```

`anyhow` is used **only** in the `#[cfg(feature = "legend")]` shim (per
constitution §1: `thiserror` in the lib, `anyhow` at boundaries). Because the
whole harness is `[dev-dependencies]` on the published crate (ADR-0003), `ureq`
and `anyhow` are **unconditional dev-deps**, and `legend` is a bare cfg flag
(`legend = []`) rather than a `dep:`-activating feature — a dev-dependency cannot
be `dep:`-gated. The default-feature surface — `classify_return_type` included —
uses `thiserror`/plain returns exclusively. grammarToJson (§14.2 step 1) is
bypassed at M0 by feeding a canned protocol-JSON fixture, so the client makes
exactly one POST.

## API / contract impact

M0's surface is deliberately **not** the §9 recognizer API. The byte recognizer + replay driver are throwaway wiring; the real §9 surface (`accept_token` / `allowed_mask` on a `DecoderSession`) is M1 work. The table records which pieces genuinely survive to M1 and which M1 may discard:

| §9 concept      | M0 form                                                    | M1+ evolution                                                                                                                                                   |
| --------------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Recognizer      | `trait ByteRecognizer` + `StubDecoder` (throwaway wiring)  | **Replaced.** M1's §8.1 harness is token-level/mask-based (`accept_token` / `allowed_mask`, speculative rollback); the byte-committing trait cannot express it. |
| Replay driver   | `replay_bytes(rec, &[u8])` (throwaway wiring)              | **Likely replaced** by a token-level replay that feeds tokens and checks `allowed_mask()` at each step.                                                         |
| Vocab           | `Vocab` (id→bytes + eos)                                   | Gains trie/mask accessors (M2); `accept_token(id)` consumes it.                                                                                                 |
| Corpus          | `GoldRecord` + `load_gold`                                 | Unchanged — the one piece that genuinely survives.                                                                                                              |
| Engine          | `classify_return_type` (default) + `LegendClient` (gated)  | `classify_return_type` survives; the client gains grammarToJson + PMCD regen (M1+).                                                                             |

M0 makes no claim to freeze the §9 API. What genuinely survives to M1 is the corpus loader (`GoldRecord` + `load_gold`) and the pure `classify_return_type`; the `ByteRecognizer` trait, `StubDecoder`, and `replay_bytes` are disposable scaffolding that prove the corpus-load + per-byte-stepping plumbing and MAY be discarded when the token-level §8.1 harness lands. The M1 soundness test is a **rewrite, not an extension** — accepted deliberately (see Decisions).

## Testing plan

> Full test pyramid and layer map: [../docs/methodology/decoder-testing.md](../docs/methodology/decoder-testing.md). Note its **fork verdict: do NOT fork Legend** — `grammarToJson` already returns the parsed AST, and the differential frame is language-membership, not AST-equality.

### Always-on default gate (wiring / liveness)

`tests/soundness_replay.rs` — default features, no network, no docker. This is a **wiring/liveness** check that the corpus loads and streams through per-byte stepping without a harness error; it is **not** a §8.1 soundness guarantee (that harness is token-level and arrives with M1):

- **`gold_corpus_streams_and_replays_without_harness_error`** — declare `const EXPECTED_GOLD_RECORDS: usize = 5034;`, then `load_gold("corpus/gold_queries.jsonl")` and for each item: assert it is `Ok` (fail on the first `CorpusError`, reporting its line number, so silent corpus corruption reddens the gate), then `replay_bytes(&mut StubDecoder::new(), record.pure_text.as_bytes())`, assert `Ok`, assert `is_complete()` after. Assert the final `count == EXPECTED_GOLD_RECORDS` (an exact named constant — never a `> 5_000` magic literal, which constitution §4 forbids).
- **`corpus_path_reports_dead_state`** (default-feature negative test) — run the same `load_gold` → `replay_bytes` loop with a test recognizer whose `accept_byte` returns `Err` on a byte the gold corpus is known to contain, and assert `Err(DecodeError::DeadState { offset, byte })` with the correct `offset`/`byte`. This drives the **only failable path** through real corpus data, so a mutant that neuters the deadness channel reddens against actual gold bytes, not just a synthetic string.
- Runs against the committed corpus, streams line-by-line, completes in well under a second.

### Unit tests (default, coverage + mutation)

- `vocab.rs` — `from_byte_tokens` round-trip via `bytes`, `eos`, `len`/`is_empty`, out-of-range `bytes(id) == None`.
- `recognizer.rs` — `StubDecoder` never dead over arbitrary bytes, `is_complete` always true, `consumed` counts, `reset` zeroes; `replay_bytes` returns the right length and (via a tiny test-only recognizer whose `accept_byte` returns `Err` on a chosen byte) surfaces `DeadState { offset, byte }` with the right fields.
- `corpus.rs` — parse one canned valid line into a `GoldRecord` with expected fields; a malformed line yields `CorpusError::Json { line, .. }` with the correct line number.
- `legend.rs` (default) — `classify_return_type` maps a canned success JSON to `ReturnTypeOutcome::ReturnType(_)` and a canned engine-error JSON to `CompileError(_)`; both arms asserted so a mutant swapping them is killed.

These keep all M0 decision logic — the classifier included — in default features, so it is covered (≥70% floor) and mutation-tested. The classification arms and the `DeadState` channel are exercised by real assertions, so a mutant that swaps the arms or neuters deadness is killed.

### Opt-in engine lane (completeness) — the no-skip mechanism

`tests/legend_completeness.rs` begins with **`#![cfg(feature = "legend")]`** (file-level). The exact honesty mechanism: **cargo/nextest never *collect* these tests unless `--features legend` is passed** — the entire compilation unit is conditionally absent from the default build graph. There is **no `#[ignore]`, no runtime `return`, no weakened assertion** — nothing is silenced. The default gate's pass/fail is provably identical whether or not the engine exists, matching §14.4's opt-in/nightly recommendation.

- **`engine_client_reaches_lambda_return_type_endpoint`** — `LegendClient::health_wait(timeout)`, POST the canned protocol-JSON + PMCD fixtures (committed under `tests/fixtures/`), and assert a classified outcome is read back — `ReturnTypeOutcome::ReturnType(_) | CompileError(_)`. The fixtures are provisional placeholders (spec R4), so asserting a *specific* `ReturnType(_)` is deferred to M1 once they are regenerated from a real §14.2 roundtrip.
- Driven by `just test-legend`, which delegates to `cargo xtask test-legend`: the xtask brings the pinned stack up (`docker compose … up -d`), runs `cargo nextest run --features legend` (each test §14.1 health-waits the engine itself), then **always** tears the stack down — teardown lives in xtask, not a shell trap (constitution §2).

The completeness-loop *logic* — `classify_return_type` — lives in default features and is fully covered and mutation-tested (above), so the return-type/error split cannot pass vacuously. Only the live HTTP shim (`LegendClient`'s `ureq` POST + `health_wait`) is deferred behind `feature = "legend"`; it is pure I/O with nothing meaningful to mutation-test hermetically, and it removes nothing from the measured set. **Pre-merge, `just ci` runs `cargo clippy --all-features -- -D warnings`**, so `legend.rs` — the gated shim included — is compiled and linted (`missing_docs`, `unsafe`, clippy) on every PR with zero docker/network. That is the constitution §2 pre-merge counterpart; only the LIVE-engine POST stays nightly/opt-in per DOMAIN §14.4.

## Dependency vetting

Rubric: `docs/methodology/overview.md` + the `dependency-vetting` skill. Prefer a vetted crate over bespoke only when it clears maintenance/license/supply-chain **and** the bespoke alternative owns hard edge cases. `deny.toml` already allows MIT/Apache and trusts only crates.io.

| Dep              | Verdict                                          | One-line justification                                                                                                                                                                                                                                                                                                                                      |
| ---------------- | ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `serde` (derive) | **Adopt**                                        | Standard, license-clean; `GoldRecord` derive is the idiomatic, DRY way to read §13.1 records.                                                                                                                                                                                                                                                               |
| `serde_json`     | **Adopt**                                        | Already a workspace dep (xtask); a hand-rolled line splitter would have to reimplement full JSON string unescaping for the escaped `\n`/quotes inside `pure_text` — a bespoke parser owning the format's edge cases (constitution §4), rejected.                                                                                                            |
| `thiserror`      | **Adopt**                                        | Constitution §1 mandates it for the lib error enums.                                                                                                                                                                                                                                                                                                        |
| `ureq`           | **Adopt (dev-dep; `legend` lane only)**          | Blocking one-shot POST, tiny tree, actively maintained. Configured `default-features = false`, so **TLS is off** — the engine is a local, plain-HTTP service (`http://localhost:6300`), pulling in no rustls/ring/webpki-roots tree. `reqwest` (tokio/hyper/TLS) is rejected for a byte-level lib; `attohttpc` viable but `ureq` has the smaller footprint. |
| `anyhow`         | **Adopt (dev-dep; `legend` lane only)**          | Constitution §1 permits it at boundaries; an **unconditional `[dev-dependency]`** used solely by the `#[cfg(feature = "legend")]` `LegendClient` shim. `legend` is a bare cfg flag, not a `dep:` feature — a dev-dependency cannot be `dep:`-gated.                                                                                                         |

**Write-our-own:** the recognizer, replay driver, and `Vocab` — trivial, domain-specific, no crate offers them. **Pin discipline:** each dep's pin is the current crates.io release, verified at add-time per constitution §2 (`cargo add` with no version writes it); the exact versions land in the PR, not from memory.

## Risks & rollout

- **R1 — replay proves less than §8.1.** Byte-level replay is *not* token-id soundness; a green M0 gate can lull us into thinking the real §8.1 guarantee holds. *Mitigation:* the non-goal and the soundness-test doc comment state explicitly that token-id replay is deferred to M1+ once a host vocab ships. No claim of §8.1 soundness at M0.
- **R2 — corpus path brittleness.** The soundness test hardcodes `corpus/gold_queries.jsonl` relative to the crate root. *Mitigation:* resolve via `env!("CARGO_MANIFEST_DIR")` so it is CWD-independent and reproducible in CI's detached-HEAD checkout (constitution §2 pre-merge/CI reproducibility).
- **R3 — engine lane is amd64 + ~1.7 GB docker.** Can't run in the default hermetic gate. *Mitigation:* that is the point of the feature gate — the lane is opt-in (`just test-legend`), and the default gate is fully hermetic. Engine images are pinned (finos 4.113.0 / 0.195.0) per finding.
- **R4 — canned engine fixtures are provisional placeholders, not a real roundtrip.** The committed `tests/fixtures/{lambda,model}.json` are hand-written PROVISIONAL placeholders (each carries a `_comment` saying so), **not** artifacts of a real §14.2 `grammarToJson -> lambdaReturnType` roundtrip. Against a live stack the engine lane will therefore return an HTTP 500 compile error until they are regenerated. *Mitigation:* regeneration from a real roundtrip is an M1 concern, tracked as a `ponytail:` deferral in `legend.rs` and `tests/legend_completeness.rs`; until then the lane asserts only that a classified outcome (return type **or** compile error) is read back, and the specific-`ReturnType` assertion is deferred to M1.

**Rollout:** land behind default features with the engine lane off; `just ci` green pre-merge. The engine lane runs on demand locally and can later be promoted to a nightly CI job (cached/pinned images per constitution §2) without touching the default gate.

## Implementation tasks

Ordered, each independently testable and independently committable:

1. **Cargo.toml** — add `serde` (derive), `serde_json`, `thiserror`, `ureq` (`default-features = false`, `json`), and `anyhow` to **`[dev-dependencies]`**; keep the published `[dependencies]` table empty; declare the bare `legend = []` feature (a dev-dependency can't be `dep:`-gated). *(testable: `just check-core-deplight` proves the core stays dep-light; `just lint` compiles + lints every feature set clean.)*
2. **`tests/support/error.rs`** — `DecodeError`, `CorpusError` + unit tests for `Display`/fields. *(testable: `just test`.)*
3. **`src/vocab.rs`** — `Vocab` + unit tests. *(testable: `just test`.)*
4. **`tests/support/recognizer.rs`** — `ByteRecognizer`, `StubDecoder`, `replay_bytes` (single deadness channel via `accept_byte`'s `Err`) + unit tests (incl. a test recognizer whose `accept_byte` returns `Err` to exercise `DeadState`). *(testable: `just test`.)*
5. **`tests/support/corpus.rs`** — `GoldRecord`, `load_gold` streaming iterator + unit tests (valid + malformed line). *(testable: `just test`.)*
6. **`tests/soundness_replay.rs`** — pull the `support/` modules in via `#[path]`; `gold_corpus_streams_and_replays_without_harness_error` (assert every item `Ok`, `count == EXPECTED_GOLD_RECORDS`) + `corpus_path_reports_dead_state` negative test; wire path via `CARGO_MANIFEST_DIR`. Run `just ci`; confirm coverage + mutants green with `just coverage` / `just test-mutation`. *(testable: integration, satisfies G1/G2/G4.)*
7. **`tests/support/legend.rs`** — default-feature `ReturnTypeOutcome` + pure `classify_return_type(&Value)` (with the URL-join helper) and canned success/error JSON unit tests, run under default features via `tests/classify_oracle.rs`; the `#[cfg(feature = "legend")]` `LegendClient` (`health_wait`, `lambda_return_type`) is a thin `ureq` shim that delegates classification to it. `just ci`'s all-features clippy pass compiles + lints the shim pre-merge (constitution §2). *(testable: classifier unit + mutation on default features via `just test` / `just test-mutation`; `just lint` builds the legend shim.)*
8. **`tests/fixtures/`** + **`tests/legend_completeness.rs`** (`#![cfg(feature = "legend")]`) + the `just test-legend` target (delegating to `cargo xtask test-legend`: compose up → `nextest --features legend` → unconditional teardown). *(testable: opt-in lane against a live stack, satisfies G3.)*
9. **`src/lib.rs`** — `GuaranteeLevel` + re-exports of the dep-light core; confirm `#![forbid(unsafe_code)]` / `#![deny(missing_docs)]`. Final `just ci` green. *(testable: `just ci`.)*

## Decisions for the human

1. **`DecoderSession` wrapper — skip it. RESOLVED (yes, skip).** `api-shape-first` proposes a `DecoderSession<R>` struct bundling recognizer + vocab. Nothing at M0 needs the pairing (the vocab has no consumer, and replay drives a bare recognizer), so it is speculative surface that KISS/YAGNI say to omit. **Decision: ship the bare `ByteRecognizer` + `replay_bytes` helper; add a session type in M1/M2 if a real consumer appears.**
2. **Trait shape — keep bare `ByteRecognizer` as throwaway scaffolding. RESOLVED.** The alternative was to add `accept_token` / `allowed_mask` / a snapshot checkpoint to the trait now (critique option b). We instead keep the byte-committing `ByteRecognizer` + `replay_bytes` as disposable M0 wiring, **consciously conceding that M0's recognizer is throwaway and M1's token-level §8.1 harness may replace it wholesale rather than extend it** (critique option a). This trades a false "M1 extends, not rewrites" claim for honest scaffolding.
3. **Split vs single error type — split (recommended).** `DecodeError` (recognizer) and `CorpusError` (loader/IO) vs one unified `Error`. The two failure surfaces are genuinely independent and the recognizer trait's `Result<(), DecodeError>` should not carry corpus-IO variants into M1. **Recommendation: two types.** (Alternative: one `Error` enum — marginally fewer lines, but leaks IO variants into the recognizer contract.)
4. **Engine module location — in the lib behind `feature = "legend"` (recommended).** Alternative is a separate `purecard-engine` crate. A feature gate is the smallest correct scaffold, keeps the default build hermetic and dep-light, and matches the finding's plan; a second crate is premature workspace ceremony at M0. **Recommendation: feature-gated module; revisit a split crate only if the engine surface grows non-trivially in M1+.**
