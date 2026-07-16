# Spec: l1-relation-api-arm-r

- Status: draft
- Created: 2026-07-16
- Owner: Thiago Souza

## Problem

The fine-tuned NL→Legend-Pure model emits the **modern Legend Pure
Relation/Function API** (`meta::pure::functions::relation::*`) — a construct family
distinguished by the `~` column sigil: `project(~[…])`, `groupBy(~[…], ~'agg':…)`,
`sort([ascending(~col)])`, window `extend(over(~col), ~[…])`, and `rename(~old,
~new)`. The gap report (`PURECARD_GRAMMAR_GAP_REPORT.md`, §4/G1) measured 61
`project(~[…])` + 24 window `extend` (and more) over 3,908 target-dialect gold. The
L1 byte-PDA dead-stated on the first `~`. This is gap report **G1** ("arm-R"),
stacked on the `%latest` PR (G2) because a gold seed exercises both together.

## Goals

- [ ] The PDA admits every arm-R construct in the seed corpus (project / groupBy /
      sort / extend-window / rename, with bare `~ident` and quoted `~'…'` columns
      and empty `~[]` keys).
- [ ] A `~`-bearing query classifies as a new `Envelope::RelationApi` (arm-R).
- [ ] The widening is additive: arm-A/arm-C parse identically; the 5,034 gold
      partition is unchanged and asserts arm-R never appears in it.
- [ ] Oracle-driven: arm-R seeds added to `corpus/modern_dialect_seeds.jsonl`.
- [ ] Every testing layer covers the new construct.

## Non-goals

- L2 schema-narrowing of `~`-columns. A `~`-column name is a synthetic relation
  column, not a schema class/property; it opens at the `SawTilde` anchor
  (`L2Position::None`) and is a pass-through — no narrowing claim.
- arm-R over a *relational* (arm-A `tableToTDS`) source. The seeds are all
  class-nav-sourced; a relational-sourced `~` query is future work (ADR-0008
  "revisit if").

## Design

The byte-PDA is a residual over-approximation built around value hubs, lambdas,
brackets, `:`, `->` and reducers (§5.6). So the whole arm-R family collapses to
**one new state**, `SawTilde`:

- `value_position` gains `~ → SawTilde`.
- `SawTilde` transitions on `[` (push a relation column-set bracket → `ExpectValue`),
  a single quote (a quoted column name → `InStrLit`), or an identifier start (a
  bare column name → `InIdent`); anything else — whitespace, a closer, another `~`
  — is a dead state.
- `AfterColon` / `AfterColonWs` additionally admit `{` (push a brace lambda), for
  the window frame `agg:{p,w,r|…}` / `agg: {p,w,r|…}`.

Everything else in arm-R — the `:` column-to-lambda separators, `over(~…)`, the
`{p,w,r|…}` frames, the reducers, bracket nesting — reuses existing machinery. A
new `Envelope::RelationApi` (marker `~`, checked first) classifies arm-R.
`docs/spec/grammar.md` §5.9 documents the productions, oracle'd by §5.8's seed
corpus. ADR-0008 records the scope decision.

## API / contract impact

`State::COUNT` grows by one (mask cache adapts dynamically). `Envelope` grows a
`RelationApi` variant — every `match` over `Envelope` now handles it (the Spider
soundness lane panics if arm-R ever appears in `gold_queries.jsonl`; the seed lane
counts it). `cargo-semver-checks` passes (additive). No PyO3 signature change.

## Testing plan

- **Unit** (`pda.rs`): `SawTilde` in `ALL_STATES`/`index`/`lexeme_kind==None`;
  direct `step` branches; accept every §4.1 seed shape; reject `~)`, `~ [`, `~~`,
  `~` in source position.
- **Soundness** (`modern_dialect_soundness.rs`): 11 arm-R seeds replay green and
  classify `RelationApi`; `SEED_ARM_R = 11`. The 5,034 gold partition is unchanged
  and asserts no gold classifies as arm-R.
- **Classifier** (`grammar/mod.rs` unit): a `~` query classifies `RelationApi`.
- **Precision** (`precision_reject.rs`): the `~`-sigil boundary rejects.
- **L2** (`scope.rs` unit): a `~`-column (`~A`, `~[Col: …]`) is `L2Position::None`.
- **Completeness walk** (`support/walker.rs`): `~` added to the alphabet so
  generated walks exercise `SawTilde`.
- **Mutation** (`just test-mutation-diff`): the new transitions' mutants die.
- **Fuzz**: harness reaches `SawTilde` via arbitrary bytes.
- **E2E** (`python/tests/test_session.py`): a `project(~[…])` query streams through
  the PyO3 boundary.

## Risks & rollout

- **Over-acceptance** beyond the strict productions (e.g. a `winAggSpec` colon that
  should be bare but admits a leading `~`). Deliberate, sound widening (§5.6); the
  compiler oracle catches the residue. Pinned by the precision suite.
- Rollback is a one-state revert plus removing the `Envelope::RelationApi` arm.
