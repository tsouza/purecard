# Spec: l1-differential-oracle

- Status: draft
- Created: 2026-07-18
- Owner: (AI engineer)

## Problem

L1's soundness rests on `L1 ⊇ gold` — but the gold corpus is a fixed sample, so a
grammar change can drop a *legal* construct the gold never happened to contain and
no test notices. That is exactly how the source-position `|X.'name'` regression
shipped: an LLM reviewer *reasoned* the construct was illegal, the change split a
shared state to reject it, and nothing mechanical caught that the real Legend
engine **parses** `|X.'name'`. We were relying on review to know the grammar; we
need the grammar's ground truth in CI.

The same absence hid three real literal gaps L1 wrongly rejected (leading-dot
float `.5`, scientific `1.5e3`, datetime fractional seconds), all of which the
engine accepts.

## Goals

- [ ] A **differential gate**: a corpus of query strings labeled by the real
      Legend engine's grammar, replayed in CI against L1, asserting L1 admits every
      query the engine parses (soundness), modulo a documented allowlist.
- [ ] Fix the three engine-verified literal gaps so the gate ships fully green.
- [ ] Version-faithfulness: the labeling asserts the engine matches L1's pinned
      target (4.113.0), so a comparison never runs against a different grammar.

## Non-goals

- Calling the engine from CI or the core (constitution §1). Labeling is offline
  tooling; CI replays a frozen corpus.
- Matching the engine's *permissiveness*. Its `grammarToJson` parses `5abc`/`1_000`
  as element references (`packageableElementPtr`); a constrained decoder must not
  admit that residue where a value belongs. Those stay documented divergences.
- The `L1-accepts ⟹ engine-parses` direction. L1 deliberately over-approximates
  (§5.6); those cases are tracked, not gated.

## Design

- `scripts/label-differential.mjs` (`just label-differential`): offline Bun tool.
  Reads `corpus/differential_l1.jsonl`, asserts the running engine version equals
  the pinned `4.113.0` (via `/api/server/v1/info`; overridable only with an
  explicit re-pin flag), POSTs each query to `…/grammar/grammarToJson/lambda`
  (200 = `parse_ok`, 400 = `parse_fail`), and freezes the verdicts back into the
  file. Draws on `scripts/lib/`.
- `corpus/differential_l1.jsonl`: diverse in-scope query strings (arm-A/-C/-R,
  literals/dates, quoted members, operators, nesting, negatives, and the engine's
  permissive residue), each `{q, legend, dimension, note}`. The row-count floor is
  the machine-asserted `MIN_CORPUS_ROWS` in `tests/differential_l1.rs`, not prose.
- `tests/differential_l1.rs`: the CI gate. For every `legend == parse_ok` row,
  L1 must accept — unless the query is in `KNOWN_DIVERGENCES` (the element-ref
  residue + a niche zero-param lambda). A new unallowlisted `parse_ok`-but-rejected
  query fails the gate. A second test keeps the allowlist honest (each entry must
  be in the corpus, engine-legal, and still L1-rejected).
- The three literal fixes in `src/grammar/pda.rs`: value-hub/`SawNumSign` `.` opens
  a leading-dot float; `InNumberFrac` `e`/`E` opens an exponent (`SawExp` /
  `NeedExpDigit` / `InExp`), decimal-point required to match the engine (`1e3` is
  an element ref, not a float); `InDateLit` admits `.` for fractional seconds.
- `docs/spec/grammar.md`: §5.4 EBNF updated for the new literal forms; §5.10 and a
  version-target paragraph document the gate and the 4.113.0 pin.

## API / contract impact

None. Grammar-only lexer additions (purely additive — no gold token is masked);
no L2, public-API, or PyO3 change.

## Testing plan

- **Grammar unit (`pda.rs`)**: `extended_numeric_and_date_literals_stream` (the
  new forms admit; `1e3` still dies) and the updated malformed-literal rejects.
- **Differential gate (`tests/differential_l1.rs`)**: the soundness replay +
  allowlist-freshness, verified non-vacuous (emptying the allowlist reddens with
  byte-precise divergence names).
- **Regression**: 5034/5034 gold (`soundness_replay`), `precision_reject`, and all lanes
  stay green; each literal fix was engine-cross-checked (accept the legal form,
  reject the residue).
- **Mutation**: `just test-mutation-diff` covers the new lexer arms.

## Risks & rollout

- **Silent engine drift**: mitigated by the version assertion in the labeling
  script. A mismatched local engine fails loudly instead of mislabeling.
- **Deployment-mode caveat**: the reference engine runs
  `TEST_IGNORE_FUNCTION_MATCH`; the oracle is used strictly as a *grammar/parse*
  acceptor (200/400), not a semantic-validity judge, so that mode is immaterial.
- **Corpus staleness**: `just label-differential` re-freezes on demand; a verdict
  flip prints loudly and is reviewed.
- Rollback: revert the commit — the lexer changes are additive and the gate is new.
