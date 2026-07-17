# Spec: l1-structural-test-gates

- Status: draft
- Created: 2026-07-17
- Owner: (AI engineer)

## Problem

A recent change added `'` → `InStrLit` to `State::AfterDot` to admit quoted
member access. Because `AfterDot` was shared by both value-navigation dots
(`$x.`) and pipeline-source dots (`X.`), it silently also admitted `|X.'name'`
in source position. The existing tests — all *positive* must-parse replays plus a
few boundary rejects — did not catch it; only external review reasoning about the
shared state did. Separately, mutation testing then found the fix's own test
didn't pin that the new state requires an identifier start.

The lesson: the suite covers "does the feature work" (positive replay) but not the
*negative space* and *structural invariants* around the state machine — so a
transition leaking a lexeme into an adjacent position, an orphaned/mis-routed
state, or a registration drift is invisible to it. This adds **deterministic**
structural gates that catch that whole class without relying on review.

## Goals

- [ ] Pin every pipeline-source-lane state's admit-set, so any value-literal
      opener leaking into source position fails a byte-precise assertion.
- [ ] Pin the value-dot vs source-dot distinction exactly (differ by the quote
      only), catching the incident in both directions and a DRY re-merge.
- [ ] Prove structurally that no state is orphaned or a black-hole, and that the
      frame set is complete — closing the "forgotten/mis-wired list entry" class.

## Non-goals

- Differential testing against the Legend engine (a separate corpus + harness —
  the black-box counterpart to these white-box gates).
- Widening or changing any grammar behaviour: this is tests only.

## Design

All additions live in the `#[cfg(test)]` module of `src/grammar/pda.rs` (plus a
few test-only helpers), driven off `step`, `ALL_STATES`, and `ALL_FRAMES`. Nothing
in the shipped grammar changes.

1. **Source-lane admissibility property**
   (`source_lane_states_admit_exactly_their_declared_byte_class`). For each of the
   six source-lane states, a `source_lane_admits(state, byte)` predicate
   transcribed from its `step` arm is checked against the machine for **all 256
   bytes × all five stack tops**. Source states read no stack, so iterating the top
   also pins stack-independence. A widening (a value opener admitted in source
   position) disagrees with the redundant spec and names the exact byte/state.

2. **Value-vs-source dot symmetric difference**
   (`value_dot_and_source_dot_differ_only_by_the_quoted_member`). The admit-sets of
   `AfterDot` and `AfterSourceDot` must differ by exactly `{'}`. Catches the quote
   re-leaking into source (symdiff shrinks), the quoted-member feature dropping
   from value position (symdiff shrinks), any other byte drifting into one dot
   (symdiff grows), and a re-merge of the two states (symdiff empties).

3. **Reachability closure** (`every_state_is_reachable_and_no_state_is_a_black_hole`).
   A fixpoint BFS over `(state, bounded stack)` configs driven by `step` asserts:
   every `ALL_STATES` variant is reached from `Start` (no orphan / mis-routed
   transition); every state has ≥1 non-Dead edge (no black-hole sink); and the
   frames `step` pushes equal `ALL_FRAMES` (no frame drift). The bounded stack
   over-approximates reachability, so it can never invent a false orphan.

These complement the existing registration gates: a forgotten `ALL_STATES` entry
is already a compile error (`[State; State::COUNT]`) and `index` is a
compiler-forced exhaustive match; what was missing is orphan/mis-route/black-hole
and the position/admit-set invariants.

## API / contract impact

None — test-only. No public-API, PyO3, or grammar-behaviour change.

## Testing plan

The additions *are* tests. Each was verified **non-vacuous** by a deliberate break:
injecting the quote into `AfterSourceDot` reddens gates 1 and 2 (naming byte
`0x27`); re-routing the source dot to `AfterDot` orphans `AfterSourceDot` and
reddens gate 3. The 5034-gold and all existing lanes stay green (behaviour
unchanged). `just test-mutation-diff` covers the new predicates.

## Risks & rollout

- **False positive from the reachability cap.** `STACK_CAP` over-approximates, so
  it can only ever reach *more* states — it cannot invent a false orphan. If a
  future state legitimately needs deeper nesting to reach, raise the cap.
- **Predicate drift.** Gate 1's predicate is a hand transcription of the source
  arms; an *intended* future widening of a source state must update the predicate
  in lockstep — that lockstep is the review checkpoint the gate exists to force.
- Rollback: revert the commit — tests only, no behaviour to restore.
