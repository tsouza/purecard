# Spec: M5 — hardening

- **Status:** Draft (ready to implement)
- **Created:** 2026-07-11
- **Owner:** PureCard decoder engineer

## Problem

M0–M4 delivered a working decoder: a pure byte-PDA (`DecoderSession`, `CompiledGrammar`), a per-state mask cache, the L2 schema overlay, and the PyO3 boundary (PR #12). The decoder is *capable* but not yet *hardened for real host integration*. Three latent risks remain, in descending soundness cost:

1. **Tokenizer exactness (the load-bearing risk, overview §11).** The pure core has no tokenizer — the host supplies token→bytes via `Vocab`. If the host's byte representation of a token disagrees with the model's actual tokenization, the mask is computed against the wrong byte-stream and soundness breaks *invisibly*: no gate observes it, because gold soundness is measured on the core's own byte-concatenation, not the host's. Nothing today forces the host's `Vocab` to be able to *express* a grammar-legal query.
2. **Stream-end handling (deferred, old issue #8).** `is_accepting()` accepts only `State::AfterValue`. A stream ending on a bare completed lexical token (a trailing top-level identifier) reports `incomplete`. Not corpus-reachable (all 5034 gold end in `)`), so a latent precision wart, not a live soundness bug — but stream-end must be handled cleanly and by a documented contract before ship.
3. **No panic backstop / no locked perf record.** `#![forbid(unsafe_code)]` plus the no-panic rule are asserted but not *fuzzed*; and the criterion bench exists but its numbers are not locked as a regression-guarded baseline.

M5 closes these without adding decoder capability.

## Goals

Mapped to the five M5 areas and the overview §10 done-criterion ("Tokenizer self-check … incomplete-generation handling, error recovery, fuzzing, final benchmarks").

- [x] **G1 — Tokenizer self-check (§10).** A pure, opt-in, side-effect-free `self_check` that round-trips gold-shaped queries through `tokenize→bytes→decoder` against the host `Vocab`, catching host-vocab vs model-tokenizer drift before it silently breaks soundness, and failing loud via a *distinct* error type.
- [x] **G2 — Incomplete-generation / EOS finalization (§10).** `is_accepting` widened to finalize any completed lexical token at stream end, derived from the single source of truth (`step`), with a documented `is_complete` contract — and a proof it only *adds* accepting configurations (no gold regression).
- [x] **G3 — Error recovery (§10).** The `DecodeError` surface distinguishes a host-contract violation (out-of-range token id) from a legitimate mask-respecting reject, so a host can always recover/resample; no dead-ends.
- [x] **G4 — Fuzzing (§10).** A separate `fuzz/` crate with targets over `accept_token`, `allowed_mask`, and `Schema::from_json` proving no panic on arbitrary/malformed input, run as a bounded, corpus-cached CI gate — with the core staying `forbid(unsafe)`-clean.
- [x] **G5 — Final benchmarks (§10).** The criterion bench extended (per-step `accept_token`, the M2 cache win, the L2 overhead) and locked as the shipped baseline behind the existing CodSpeed regression guard, with the numbers recorded in the architecture doc.
- [ ] **G6 — Fold tracked deferrals.** Opportunistic spec-content clarity pass (old issue #5); confirm the commitlint subject-case class-fix (lefthook `commit-msg` hook) is durable.

## Non-goals

- **No new decoder capability.** No new grammar productions, no new operators, no widening of §5 beyond the §5.6 residuals already sanctioned.
- **The L3 "faithful" tier is out of scope.** M5 hardens the L1/L2 decoder as shipped; the faithful-projection tier is future work.
- **No `finalize()` state machine, no EOS-signal plumbing, no new `State` variant.** EOS handling is a gate widening, not a subsystem.
- **No exhaustive fuzz campaign.** 60 s/PR + 900 s nightly is a backstop, not a proof of exhaustion; deeper campaigns grow the committed corpus over time.
- **No per-token gold corpus compiled into the pure core.** The core stays dep-light; the heavy round-trip lives in `tests/`.
- **No new runtime dependency in the core.** New pins are confined to the excluded `fuzz/` crate.

## Design

### Area 1 — Tokenizer self-check

**Location.** A new *pure* module `src/selfcheck.rs`, exposing **free functions**, not a constructor variant:

```rust
pub fn self_check(grammar: &CompiledGrammar, samples: &[&[u8]]) -> Result<(), SelfCheckError>;
pub fn self_check_smoke(grammar: &CompiledGrammar) -> Result<(), SelfCheckError>;
```

A free function beats a `CompiledGrammar::self_checked` constructor variant because it is opt-in and side-effect-free — the host decides *when* to pay the cost — and it keeps `CompiledGrammar::compile` allocation-only. `CompiledGrammar` already carries the host `Vocab`, so there is no extra plumbing.

**Round-trip (what it verifies).** Byte-driving the PDA alone would exercise only the grammar, never the vocab — useless for drift. The check must go *through tokens*. For each sample query byte-string:

1. Open a fresh `DecoderSession::new(grammar)`.
2. At the current byte offset, **greedily longest-match-segment** the remaining bytes against the host `Vocab` (`bytes(id)` is a prefix of the remaining bytes; pick the longest such id). If no token matches the prefix → `Unsegmentable { pos }`.
3. For that segment id, assert `allowed_mask()` has the bit set (the stream stays *live*; the mask must admit the very token the vocab produced). If not → `DeadEnd { pos, id }`.
4. `accept_token(id)`; a reject is also `DeadEnd { pos, id }`.
5. At end, assert `is_complete()` (the EOS bit is masked-in). If still open → `Incomplete { consumed }`.

A grammar-legal query the host vocab **cannot segment**, that **dead-ends**, or that **never completes** proves the host's declared token→bytes cannot express a grammar-legal query — i.e. host-vocab vs model-tokenizer drift.

**Loud failure.** A distinct `SelfCheckError` (its own `thiserror` type — **not** a `DecodeError` variant), so an alarm is unmistakably "vocab drift," never a routine reject:

```rust
#[derive(Debug, thiserror::Error)]
pub enum SelfCheckError {
    #[error("query {query_index}: no vocab token matches the byte prefix at position {pos}")]
    Unsegmentable { query_index: usize, pos: usize },
    #[error("query {query_index}: token id {id} at position {pos} is masked out / dead-ends the decoder")]
    DeadEnd { query_index: usize, pos: usize, id: u32 },
    #[error("query {query_index}: stream still incomplete after consuming {consumed} bytes")]
    Incomplete { query_index: usize, consumed: usize },
}
```

**Sample set.** `self_check_smoke` passes an embedded `const SMOKE: &[&[u8]]` of ~4 canonical gold-shaped byte literals. **No corpus file is compiled into the pure core**; the full 5034-query round-trip is a `tests/` integration test that loads via the existing oracle harness and calls `self_check` with the whole corpus.

**Purity.** All in-process: const `&[u8]` samples, byte-slice matching over `Vocab`, no I/O/async/network. `forbid(unsafe_code)`/`deny(missing_docs)`-clean. Zero new core deps.

### Area 2 — EOS finalization

**Exact states.** The value-terminal lexical states — those whose `step` delegates to `AfterValue` on a token-boundary byte — are `InIdent`, `InNumberInt`, `InNumberFrac`, `InDateLit`, `InStrLit { escaped: true }` (closing quote seen), plus `AfterValue` itself.

**Acceptance rule.** Do **not** hand-maintain that list — derive it from the single source of truth `step`, using whitespace as the canonical token terminator:

```rust
const VALUE_BOUNDARY: u8 = b' ';

pub fn is_accepting(&self) -> bool {
    self.stack.is_empty()
        && matches!(step(self.state, None, VALUE_BOUNDARY), Step::Next(State::AfterValue))
}
```

`step`'s signature is `step(state: State, stack_top: Option<Frame>, byte: u8) -> Step`; at an empty stack, `None` is the consistent `stack_top`. This auto-includes every terminal-lexical state and auto-excludes the rest, verified per-state against `step`:

- **Excluded (stay non-accepting):** `ExpectValue`/`ExpectValueReq`/`ExpectSource`/`AfterDot`-type hubs (ws→self), `InStrLit { escaped: false }` (still inside the string), `InMultiplicity` (ws→Dead), and crucially **`InSourceIdent`** (ws→Dead). Excluding `InSourceIdent` is deliberate: a bare `|X` source is *not* a completed value by design (pda.rs source-state doc), so the task's `|X` example is correctly **not** finalized.
- Any future terminal state added to `step` is covered for free (DRY, no magic list).

**No-regression argument (the proof).** The change touches only the `is_accepting` gate (→ the EOS mask bit). It never alters `step`, never clears a non-EOS bit, never turns a live byte dead — it *strictly adds* accepting configurations. Because the empty-stack guard still holds, numbers/strings/dates — which are only ever reachable *under an open frame* (an argument list, a bracket) — remain non-accepting: their state passes the `step` test but their stack is non-empty. The **only newly-reachable** empty-stack completion is a trailing top-level identifier (`|X.all()->name`). All 5034 gold queries end in `)` → `Step::Pop(AfterValue)` with an empty stack → still `AfterValue`, still accepting. Therefore **gold soundness stays 5034/5034**; the new acceptance is over-acceptance (an accepted L1 residual per §5.6), caught downstream, never a soundness regression.

**No `finalize()`.** No new state, no EOS-signal plumbing, no forked contract. `accept_token`'s EOS branch already consults `is_accepting`, so it benefits automatically.

### Area 3 — Error recovery

**Audit of the current surface.** `DeadState` (rich: offset/byte/state/stack_top), `InadmissibleToken { id }`, and `UnexpectedEos` cover the per-step reject surface; `SchemaError::Json` already cleanly types a malformed schema at construction, correctly *outside* `DecodeError`. The §8.5 rollback-into-a-clone invariant guarantees no dead-end — after any reject the session is untouched, so the host always resamples.

**The one gap.** `InadmissibleToken { id }` conflates two distinct causes (documented as such in its own doc-comment): an **out-of-range id** (a host-contract violation — the host ignored the mask / passed an id with no `Vocab` entry) versus an **in-range token whose bytes dead-end** the recognizer (a normal, mask-respecting rejection). Split the host-bug case off:

```rust
/// A token id with no entry in the host `Vocab` (out of range) was submitted —
/// a host-contract violation, distinct from a legitimately masked reject.
#[error("token id {id} is unknown: no entry in the host vocabulary")]
UnknownToken { id: u32 },
```

`InadmissibleToken { id }` narrows to *in-range, masked-out* rejections. This makes a host bug distinguishable from routine masking and keeps recovery advice unambiguous.

### Area 4 — Fuzzing

**Crate layout.** A separate `fuzz/` crate, added to the root `[workspace] exclude` alongside `lints` (libfuzzer-sys's `fuzz_target!` needs `unsafe` + nightly). The core keeps `#![forbid(unsafe_code)]` and stays stable-pinned. `fuzz/Cargo.toml`:

```toml
[dependencies]
libfuzzer-sys = "0.4.13"
arbitrary = { version = "1.4.2", features = ["derive"] }
purecard = { path = ".." }

[package.metadata]
[package]
# cargo-fuzz marker
```

(`[package] fuzz = true` metadata per cargo-fuzz convention.)

**Targets (`fuzz/fuzz_targets/`):**

- `accept_token.rs` — `#[derive(Arbitrary)]` a `(seed_vocab_bytes, Vec<u32>)`; build a `CompiledGrammar` + `Vocab::from_byte_tokens`, loop `session.accept_token(id)` over the id stream, and after each accepted step assert the mask-length invariant (`allowed_mask().len() == expected_words`) and that **every set bit `< vocab.len()`** (the OOB invariant). No panic on any id, including out-of-range (which must now surface `UnknownToken`).
- `allowed_mask.rs` — arbitrary byte prefix driven via `accept_byte`, then `allowed_mask()`; assert the same bounds invariant plus `is_complete()` totality (it never panics, always returns a bool).
- `schema_from_json.rs` — feed an arbitrary `&[u8]` straight to `Schema::from_json`; it must return `Result` (a `SchemaError`), never panic, on arbitrary/malformed JSON.

**CI gating** — see the CI section.

### Area 5 — Benchmarks

**Extend** `benches/allowed_mask.rs` (keep `harness = false`): retain the three existing `allowed_mask` configs; add

- **(a)** `accept_token` per-step latency,
- **(b)** the **M2 cache win** — cold (fresh session, first `allowed_mask` at a state) vs warm (cache hit), quantifying M2,
- **(c)** the **L2 overhead** — `with_schema` vs `new` at the identifier position (the L2-vs-L1 delta).

**Regression guard.** CodSpeed is already wired (`ci.yml:256` `bench (codspeed)` job, OIDC, gated on `vars.CODSPEED_ENABLED == 'true'`) — instruction-count, walltime-independent, so it reproduces faithfully in CI (§2 pre-merge reproducibility). Flip `CODSPEED_ENABLED=true` now that the benches are meaningful, set the regression threshold in repo config, and document the shipped numbers + threshold in `docs/spec/architecture.md` §4 (G3). Reproducibility already holds: fixed `VOCAB_SIZE=150_000`, pre-warmed cache, `black_box`, `Swatinem/rust-cache` + `[profile.bench] opt-level = 3`.

## API / contract impact

New public surface, all under `deny(missing_docs)`, kept minimal:

- **`purecard::selfcheck::self_check(&CompiledGrammar, &[&[u8]]) -> Result<(), SelfCheckError>`** and **`self_check_smoke(&CompiledGrammar) -> Result<(), SelfCheckError>`** — opt-in, pure, side-effect-free.
- **`purecard::SelfCheckError`** (new `thiserror` enum: `Unsegmentable`, `DeadEnd`, `Incomplete`) — a *distinct* type, deliberately not a `DecodeError` variant.
- **`DecodeError::UnknownToken { id }`** — new variant; `InadmissibleToken { id }` narrows to in-range masked rejects. This is an additive enum change; the doc-comment moves the "out of range" clause from `InadmissibleToken` to `UnknownToken`.
- **`is_complete` contract clarified (behavioral widening, same signature).** Documented as: *`true` iff every frame is closed **and** the last token is fully lexed at a value boundary* — so a completed trailing top-level identifier at stream end now reports complete. No signature change; a strict superset of prior `true` cases.

No breaking changes: every change is additive (new module, new fn, new variant) or a documented widening of an existing `bool`.

## Testing plan

Gold soundness is the invariant that must not move.

- **Gold soundness stays 5034/5034.** The existing all-gold soundness test is unchanged and must still pass (all gold end in `)` → `AfterValue`).
- **EOS / `is_accepting` (white-box, mirroring the `index` bijection enumeration):** for each newly-accepting state (`InIdent`, `InNumberInt`, `InNumberFrac`, `InDateLit`, `InStrLit{escaped:true}`), `Pda::at(state).is_accepting()` is `true`; for each non-terminal (`InSourceIdent`, `ExpectValue`, `InStrLit{escaped:false}`, `InMultiplicity`), it is `false`.
- **EOS driven:** `|X.all()->name` → `is_complete()`; `|X.all()->take(3` (open frame) → not complete; `|X` still dies (`InSourceIdent` ws→Dead).
- **Self-check happy path:** a `Vocab` that covers the smoke set ⇒ `self_check_smoke` returns `Ok(())`.
- **Self-check counterfactual (the guard that the check catches drift):** a deliberately-broken vocab — drop the `")"` token (→ `Unsegmentable`/`Incomplete`) and, separately, corrupt one token's bytes (→ `DeadEnd`) — ⇒ `self_check` returns the expected `SelfCheckError` variant at the expected `byte_pos`. This is what proves the check has teeth.
- **Self-check corpus integration (`tests/`):** load all 5034 gold via the oracle harness, build a faithful `Vocab`, call `self_check` ⇒ `Ok(())`.
- **Error split:** an out-of-range id ⇒ `DecodeError::UnknownToken`; an in-range, masked-out id ⇒ `InadmissibleToken`.
- **Fuzz smoke:** each of the three targets builds (`just fuzz-build`) and survives a short bounded run in CI with no crash; seed corpus committed under `fuzz/corpus/<target>/`.
- **Bench:** the extended `benches/allowed_mask.rs` compiles and runs under `cargo codspeed`; numbers recorded in architecture §4.

Coverage floor 70 and mutation 0-missed hold; no skips, no weakened assertions.

## Dependency vetting

Run the `dependency-vetting` rubric on the two new fuzz crates before pinning. Versions verified against crates.io on 2026-07-11 (per §2 "latest stable, verified"):

| Crate           | Version          | Scope                                                        | Verdict                                          |
| --------------- | ---------------- | ------------------------------------------------------------ | ------------------------------------------------ |
| `cargo-fuzz`    | 0.13.2           | dev tool (installed via `taiki-e/install-action`, not a dep) | adopt — de-facto standard, first-party rust-fuzz |
| `libfuzzer-sys` | 0.4.13           | `fuzz/` crate only                                           | adopt — rust-fuzz canonical harness              |
| `arbitrary`     | 1.4.2 (`derive`) | `fuzz/` crate only                                           | adopt — canonical structured-fuzzing input crate |
| `criterion`     | 0.8.2            | already pinned in `benches`                                  | current; no bump needed                          |

**The pure core adds zero dependencies.** `selfcheck.rs` is inline `&[u8]` literals + the existing `Vocab`/session APIs. All `unsafe` and both new pins are confined to the excluded `fuzz/` crate, so the core keeps `#![forbid(unsafe_code)]` and `#![deny(missing_docs)]`.

## CI

- **New `fuzz.yml`.** `dorny/paths-filter` on core paths (`src/**`, `fuzz/**`) gates the job. On every code PR: a **compile-only** `just fuzz-build` (`cargo fuzz build` — catches bit-rot, zero run cost) plus a **time-boxed** `just fuzz <target> 60` (`-max_total_time=60`) per target. A `schedule:` nightly cron runs longer (`900`). Install `cargo-fuzz` via `taiki-e/install-action` (`tool: cargo-fuzz`); provide nightly via a **scoped** `dtolnay/rust-toolchain@nightly` (the §1-sanctioned "channel the pinned file doesn't provide," confined to this one job). A crash uploads the offending input as an artifact and reddens the PR.
- **Corpus cache (no unbounded external fetch, per §2).** Seed corpus committed under `fuzz/corpus/<target>/`; restore/persist via `actions/cache` keyed on `fuzz-corpus-${{ hashFiles('fuzz/**') }}`. No `curl`, no external download.
- **Gate wiring.** Add `just fuzz-build` (compile-only) to the justfile alongside the existing `just fuzz`. Add the `fuzz` job to the `ci-gate` / `no-warnings` needs list so a fuzz-build failure blocks merge, and so warnings are errors there too (§2).
- **Bench/CodSpeed.** Flip repo var `CODSPEED_ENABLED=true` (the `bench (codspeed)` job at `ci.yml:256` is already OIDC-authed and gated on it); set the regression threshold in CodSpeed repo config.
- **Mutation** already runs at merge-to-main (not per-PR); release runs full CI incl. mutation and publishes only when green. No change.

## Risks & rollout

- **EOS widening over-accepts a trailing top-level identifier.** Mitigation: this is a §5.6-sanctioned L1 residual (precision loss, not soundness loss), proven above to keep gold at 5034/5034; caught downstream by the compiler. Pinned by the white-box `is_accepting` enumeration.
- **Self-check gives false confidence if the sample set is unrepresentative.** Mitigation: the `tests/` integration test runs the *full* 5034-query corpus, not just the 4-query smoke set; the broken-vocab counterfactual proves the check actually fails on drift.
- **Fuzz flake / CI cost.** Mitigation: PR runs are compile-only + 60 s-bounded with a committed, cached corpus; deep runs are nightly-only. No unbounded fetch.
- **CodSpeed threshold too tight → false regressions.** Mitigation: instruction-count is deterministic; set the threshold from the first locked baseline with headroom, documented in architecture §4.
- **Rollout order** is bottom-up (see tasks); each task is independently green under `just ci` before the next. The whole milestone is one spec, landed as the reviewer-gated PR sequence the constitution requires (one change → one worktree → one PR each where separable; EOS + error-split + selfcheck + benches + fuzz are independently testable).

## Implementation tasks

Ordered, small, each independently testable and green under `just ci`:

1. **EOS widen.** Add `const VALUE_BOUNDARY`, rewrite `is_accepting` to the `step`-derived rule; add the white-box per-state enumeration + the three driven cases; update the `is_complete` doc-comment. Confirm gold 5034/5034.
2. **Error split.** Add `DecodeError::UnknownToken { id }`, narrow `InadmissibleToken`'s doc + behavior; add the two-way test (OOB vs masked).
3. **`selfcheck.rs`.** New module: `SelfCheckError`, `self_check`, `self_check_smoke`, `const SMOKE`; the happy-path unit test and the broken-vocab counterfactual (drop `")"` + corrupt-bytes). Export from `lib.rs`.
4. **Corpus self-check test (`tests/`).** Load all gold via the oracle harness, build a faithful `Vocab`, assert `self_check` ⇒ `Ok`.
5. **Bench extension + CodSpeed.** Add `accept_token`, cache-win, L2-overhead benches; flip `CODSPEED_ENABLED=true`; set threshold; record numbers in `docs/spec/architecture.md` §4.
6. **`fuzz/` crate.** New excluded crate, three targets, committed seed corpus; add `just fuzz-build`.
7. **`fuzz.yml`.** Paths-filter, compile-only + bounded run, nightly cron, `actions/cache` corpus, `taiki-e/install-action` + scoped nightly; wire `fuzz` into `ci-gate` needs.
8. **Fold deferrals (G6).** Opportunistic spec-content clarity pass (old issue #5); confirm the lefthook `commit-msg` commitlint subject-case rule is durable (a fixture commit message that violates subject-case is rejected by the hook).

## Decisions for the human

Genuine choices, with a recommendation for each:

1. **Self-check location — free fn vs constructor variant.** *Recommend the free `self_check`/`self_check_smoke` pair* (chosen above): opt-in, side-effect-free, keeps `compile` allocation-only. A `CompiledGrammar::self_checked` constructor would force every host to pay the cost and couple compilation to a corpus. **Decision: free fn.**
2. **EOS — widen `is_accepting` vs add a `finalize()` state.** *Recommend the `step`-derived widen* (chosen above): no new state, no forked contract, DRY against `step`, provably additive. A `finalize()` machine is a subsystem for a non-corpus-reachable wart — over-engineering. **Decision: widen.**
3. **`InSourceIdent` at EOS — accept a bare `|X` source, or not?** *Recommend NOT* (the rule excludes it): a bare source is not a completed value by design (pda.rs source-state doc), and accepting it would be a real capability change, not hardening. Flag for confirmation because it is the one place where "finalize completed tokens" could be read more broadly.
4. **Fuzz PR budget — 60 s/target on every code PR, or nightly-only?** *Recommend 60 s/PR + 900 s nightly*: the 60 s run is cheap insurance that catches regressions at the PR, the nightly is the depth. If CI minutes are tight, compile-only on PR + all runs nightly is the fallback.
5. **CodSpeed threshold value.** Needs a human to pick the regression tolerance (e.g. ±5% instruction count) from the first locked baseline. *Recommend setting it from task 5's numbers with modest headroom, then ratcheting tighter over time* (PROTECTED gates only tighten).

## Outcome (M5, landed)

- **G1 Tokenizer self-check** — `src/selfcheck.rs`: `self_check` / `self_check_smoke`, distinct `SelfCheckError` (`Unsegmentable` / `DeadEnd` / `Incomplete`, each with `query_index` + position), inline `SMOKE`. Unit tests cover happy path, dropped-`)` → `Unsegmentable`, a grammar-illegal segmentation → `DeadEnd`, a partial query → `Incomplete`. `tests/selfcheck_corpus.rs` self-checks all **5034** gold queries via faithful per-query lexeme vocabs.
- **G2 EOS finalization** — `Pda::is_accepting` derives terminality from `step` (`stack.is_empty() && matches!(step(state, None, VALUE_BOUNDARY), Step::Next(AfterValue))`), documented `is_complete` contract. White-box per-state enumeration + driven cases. Strictly additive → **gold stays 5034/5034**. See ADR-0006.
- **G3 Error recovery** — `DecodeError::UnknownToken { id }` split from `InadmissibleToken { id }`; `accept_token` returns `UnknownToken` for out-of-range ids. Both variants tested.
- **G4 Fuzzing** — excluded `fuzz/` crate (ADR-0006): `libfuzzer-sys` 0.4.13, `arbitrary` 1.4.2, cargo-fuzz 0.13.2. Three targets (`accept_token`, `allowed_mask`, `schema_from_json`); the CI bit-rot gate compiles every target under nightly on each PR, and an `xtask` unit test keeps `FUZZ_TARGETS`, the `fuzz/fuzz_targets/` files, and the `fuzz.yml` matrix in sync so drift fails a gate. The crash-free result is an **observed, time-boxed run — not a permanent guarantee**: a bounded fuzz run (60 s/PR, 900 s cron) explores only the inputs it happens to reach and is never exhaustive, so "0 crashes" describes those runs, not a machine-asserted invariant. CI `fuzz.yml` (paths-gated, nightly, cargo-fuzz via `taiki-e/install-action`, corpus cached). `just fuzz` / `just fuzz-build` / `just fuzz-ci` (xtask loop).
- **G5 Benchmarks** — `benches/allowed_mask.rs` extended: `accept_token`, `cache_win` (cold vs warm), `l2_overhead` (with_schema vs new). Local wall-clock figures are representative and machine-dependent, never a gate: shallow/identifier masks are sub-µs and the deep-stack worst case is a few hundred µs (within the §4.5 target), `accept_token` is tens of ns, and a warm cache is ~4 orders of magnitude cheaper than a cold first-visit build (the M2 win). CodSpeed is the intended instruction-count regression guard but is **opt-in and not yet enforced** — the `bench` job is gated behind `vars.CODSPEED_ENABLED == 'true'`, so no perf delta blocks a PR until the app is installed and that variable is set; recommended first-lock threshold ±10 %. Documented in `docs/spec/architecture.md` §4.6.
- **G6 (deferred)** — no spec-content clarity pass was in-scope-quick, so old issue #5 is left noted. The commitlint subject-case `commit-msg` hook (`lefthook.yml`) is installed by the onboarding (per the graduated lesson); durability unchanged.
