# Spec: l1-latest-milestoning

- Status: draft
- Created: 2026-07-16
- Owner: Thiago Souza

## Problem

The fine-tuned NL→Legend-Pure model that PureCARD constrains emits **modern
Legend Pure**, which includes the symbolic milestoning literal `%latest` (and its
processing-time sibling `%latestdate`). The L1 byte-PDA admits `%`-prefixed
*numeric* date/datetime literals only (`%2020-01-01[T…]`), so the very first byte
after `%` in `%latest` — the `l` — is a dead state. The gap report
(`PURECARD_GRAMMAR_GAP_REPORT.md`, §5/G2) measured `%latest` in **311** of 3,908
target-dialect gold queries (102 distinct signatures), including
`Class.all(%latest)`, bitemporal `Class.all(%latest, %latest)`, and milestoned
property calls `$x.FACET(%latest, %latest)`. Masking it is a soundness bug against
the emitted dialect.

## Goals

- [ ] The L1 PDA admits `%latest` and `%latestdate` wherever a literal operand is
      legal (`.all(%latest)`, `.PROP(%latest, %latest)`, comparison operands).
- [ ] Bare `%` and malformed `%`-literals stay dead (the existing precision pin
      holds).
- [ ] New gold seed is added to `corpus/gold_queries.jsonl` so the addition is
      oracle-driven, not invented; the soundness gate replays it green.
- [ ] Every testing layer covers the new construct (see Testing plan).

## Non-goals

- The `~` Relation/Function API family (gap report G1, "arm-R") — a separate,
  larger change in its own PR (`feat/l1-relation-api-arm-r`), stacked on this one
  because gold seed line 155 exercises both `~` and `%latest` together.
- L2 schema-narrowing of the milestoning literal. `%latest` is a `Lexeme::Date`,
  which the scope tracker treats as a pass-through operand — no narrowing claim.

## Design

`%latest` is a *symbolic* milestoning literal: a `%` sigil followed by a lowercase
keyword, structurally parallel to how the machine already admits *any* identifier
where a reducer/step/property name is expected (an over-approximation §5.6
sanctions and the compiler/L2 re-checks). One new automaton state carries it:

- `State::SawPercent` gains a branch: a lowercase-letter first byte after `%`
  enters `State::InMilestoneLit` (a digit/`-`/`T`/`:` still enters `InDateLit`,
  unchanged). A non-date, non-lowercase byte (`)`, whitespace, EOF) stays a dead
  state, so bare `%` is still rejected.
- `State::InMilestoneLit` accumulates lowercase ASCII letters and is
  **value-terminal**: any other byte delegates to `State::AfterValue` (via `step`),
  so `%latest` completes at a value boundary exactly like `%2020-01-01`. Its
  `lexeme_kind` is `LexKind::Date` (it is a `%`-literal), so the L2 scope tracker
  buffers and classifies it identically to a numeric date literal.

Oracle: gold seed strings in `corpus/gold_queries.jsonl` (see Testing plan). Spec
§5.4/§5.5/§5.7 of `docs/spec/grammar.md` gain the `milestoneLit` production and
its inventory row.

## API / contract impact

None. No public Rust signature or PyO3 boundary changes — this widens the fixed
internal grammar only. `State::COUNT` grows by one; the mask cache
(`compiled.rs`) keys on `State::COUNT`/`index()` dynamically, so it adapts with no
call-site change. The `pub` `State` enum grows one additive variant; the
`cargo-public-api` baseline is refreshed via `just public-api-bless` as an
intended, documented surface change.

## Testing plan

- **Unit** (`src/grammar/pda.rs` `#[cfg(test)]`): `InMilestoneLit` in `ALL_STATES`
  / `index` bijection / `lexeme_kind == Date`; transition tests — `%latest`,
  `%latestdate` accept and complete; `%l` mid-literal is non-accepting; the
  uppercase/digit boundary stays dead.
- **Soundness** (`tests/soundness_replay.rs`): new `%latest` gold seed replays
  green; `ARM_C` count constant bumped to match the added class-nav seeds.
- **Precision** (`tests/precision_reject.rs`): bare `%` still dies
  (`take(%)`, `< %`); `%LATEST` (uppercase) and `%latest1` boundary rejects that
  pin the lowercase-letters-only lexeme.
- **L2** (`tests/l2_soundness.rs` / scope unit): `%latest` is pass-through — a
  `.all(%latest)` stream masks nothing.
- **Completeness walk** (`tests/support/walker.rs`): a seeded accepting walk
  containing `%latest` round-trips (hermetic self-test + engine lane).
- **Mutation** (`just test-mutation-diff`): the new `SawPercent`/`InMilestoneLit`
  transitions' mutants are killed by the unit + precision tests.
- **Fuzz** (`fuzz/fuzz_targets/accept_token.rs`): unchanged harness reaches the
  new states via arbitrary bytes; a `%latest` seed added to the corpus dir.
- **E2E** (`python/tests/test_session.py`): a `Session` admits a `%latest` query
  end-to-end through the compiled grammar + FFI.

## Risks & rollout

- **Over-acceptance** of `%<lowercase>` beyond `%latest`/`%latestdate` (e.g.
  `%foo`). Deliberate, sound widening (§5.6): the compiler/L2 reject an unknown
  milestone symbol; L1's job is to not dead-state a token the model emits. Pinned
  by the precision suite so it cannot silently widen further (uppercase/digit
  boundaries stay dead).
- Rollback is a one-state revert; the grammar is internal and unversioned.
