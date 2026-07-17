# Spec: l1-quoted-member-access

- Status: draft
- Created: 2026-07-17
- Owner: (AI engineer)

## Problem

Legend Pure allows a **quoted member/column name after a navigation dot** —
`$x.'Cnt'`, `$r.'Gross Credits'` — for relation columns whose names aren't bare
identifiers (spaces, reserved words, punctuation). The L1 byte-PDA's `AfterDot`
state accepts only an identifier-start byte, so a `'` there is a dead state: every
query navigating a quoted column dead-states at the `'`.

`.'name'` is legal Legend Pure, so this is a genuine L1 gap; it also blocks the
arm-R nested-subquery shapes for the soundness corpus (they use `$x.'Cnt'`,
`$r.'ClientGC'`).

## Goals

- [ ] Admit a single-quoted string as a member name after a navigation dot:
      `AfterDot` accepts `'`, streams the string body (reusing the existing
      `InStrLit` production, including `''` quote-doubling), and returns to a
      completed-value position so normal continuations (`->`, comparison,
      further `.`) follow.
- [ ] Land three arm-R nested-subquery shapes in the modern-dialect soundness
      corpus, now that they parse.

## Non-goals

- The `$x['name']` / `$x[0]` **bracket** access form. The engine rejects it
  ("Bracket operation is not supported"); it is the GAP-3(c) `$it[0]` construct
  and stays an accepted L1 over-approximation (irreducible — shares the `ident[`
  production with `T[1]` multiplicities; 14322 gold uses). Untouched here.
- Any L2 schema narrowing of the quoted member (N1/N6 keying on a quoted name is
  separate follow-up work; this is L1 admission only).

## Design

Oracle-driven from a must-parse/must-reject set (`.'name'` is legal Legend Pure;
the `['name']` bracket form is not). Byte-PDA changes in `src/grammar/pda.rs`:

- `State::AfterDot` (a *value-navigation* dot) gains
  `b'\'' => Step::Next(State::InStrLit { escaped: false })`.
- A new `State::AfterSourceDot` handles a *pipeline-source* dot (`X.all()`):
  identifier-only, a quoted string is a dead end. `State::InSourceIdent`'s `.`
  routes here instead of the shared `AfterDot`.

`InStrLit` already streams a single-quoted body with `''` doubling and closes (on
an un-doubled `'`) by re-dispatching from `AfterValue` — so `$x.'name'` lands at
`AfterValue`, exactly like a bare `$x.member`, and every existing continuation
works unchanged.

**Scoping the two dot contexts.** `AfterDot` was previously reached from *both*
value navigation (`$x.`) and the pipeline source (`X.`), so admitting `'` at
`AfterDot` alone would also admit `|X.'name'` in source position — a construct the
oracle never sanctioned and the feature never intended. Splitting the source dot
into its own identifier-only state keeps quoted-member access confined to value
navigation. For the 5034 gold this is invariant-preserving: the quoted branch is
additive (never masks a gold token — `AfterDot` used to dead-state on `'`), and
the source path stays identifier-only exactly as before.

Note on escaping: the engine notation `'a\'b'` denotes an escaped quote in the
name. This grammar's single-quoted strings escape by **doubling** (`''`, §5.5), so
the corpus form is `'a''b'`; quoted-member access inherits that by reusing
`InStrLit`, no new escaping rule.

## API / contract impact

None. L1 grammar only; no L2 rule, no public-API or PyO3 change.

## Testing plan

- **Grammar unit (`src/grammar/pda.rs`)**: `AfterDot` + `'` transitions to
  `InStrLit`; value-nav continuations (chained `->`, dotted `.next`); the quoted
  member does **not** leak into source position (`|X.'name'`, `|X.'name'->all()`,
  `|demo::Reading.'Cnt'` all die) while `|X.all()` still accepts; the `['name']`
  bracket form is unaffected.
- **Soundness (`tests/modern_dialect_soundness.rs`)**: the must-parse set streams
  to `is_complete` — `$x.'Cnt'`, a name with spaces (`'Gross Credits'`), a doubled
  quote (`'a''b'`), a ref to an arm-R column, inside a brace window frame, and a
  chained `->toOne()` — plus the three arm-R nested-subquery shapes added to
  `corpus/modern_dialect_seeds.jsonl` (arm-R, `db_id` "modern").
- **Precision**: `$x['Cnt']` is *not* newly rejected here (documented non-goal),
  but the plain `.'`-without-a-closing-quote boundary (`$x.'` then EOS) must not be
  `is_complete` (unclosed string).
- **Regression**: 5034 gold (`soundness_replay`) + all L2 lanes stay green; seed
  arm counts updated for the three new seeds.
- **Mutation**: `just test-mutation-diff` covers the new transition.

## Risks & rollout

- **Additive-admit safety**: admitting `.'…'` at a value dot cannot mask a gold
  token; the only regression surface would be a test asserting `.'` is rejected —
  none exists (every `.'` in the gold is string-internal, inside `InStrLit`, not a
  navigation dot). The source-dot split re-narrows source position to
  identifier-only, which is exactly its prior behaviour, so it too preserves the
  gold. Both are enforced by the `soundness_replay` gold-replay gate, not this
  prose.
- Rollback: revert the whole change cohort together so the source-of-truth and CI
  gates stay consistent — the `AfterDot` transition and the new `AfterSourceDot`
  state (`src/grammar/pda.rs`) with their `after_dot_admits_a_quoted_member_name`
  test and the updated `an_all_dot_is_not_a_member_navigation` scope test, the
  three seed records (`corpus/modern_dialect_seeds.jsonl`), and the matching
  `SEED_ARM_R = 14` count (`tests/modern_dialect_soundness.rs`). Reverting the
  single commit does exactly this.
