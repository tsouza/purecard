# Spec: l2-nested-scope-hardening

- Status: draft
- Created: 2026-07-17
- Owner: (AI engineer)

## Problem

The L2 `ScopeTracker` holds per-pipeline state — the pipeline element class
(`cur_class`) and each lambda variable's class binding (`var_class`) — but never
scoped it to the query's lexical structure. A **nested subquery** inside a lambda
body (`…->filter(x| OtherClass.all()->…)…`) mutates that shared state and it is
never restored, so the outer pipeline is left mis-typed and L2 **masks a valid
outer token** — a soundness violation (`L2 ⊇ gold`).

Two pre-existing leaks, both reproducible on `main` (`8a254ef`):

- **Source-class leak.** `|A.all()->filter(x|B.all()->isEmpty())->map(z|$z.n)`
  — the nested `B.all()` overwrites `cur_class`; the outer `$z.n` is then narrowed
  against B (masked) instead of A.
- **Binder-shadow leak.**
  `|A.all()->filter(x|B.all()->map(x|$x.m)->isEmpty() && $x.n > 0)` — the inner
  `map(x|…)` rebinds `x` to B in `var_class`; the outer `$x.n` is masked.

These went unnoticed because the gold + seed corpus contains **no nested-subquery
query** — a structural blind spot the replay gates cannot see.

## Goals

- [ ] Lexically scope `cur_class` and `var_class` binder bindings to the lambda
      body they belong to, so a nested subquery cannot leak either out and mask a
      valid outer token.
- [ ] Preserve every existing behaviour: the 269 in-scope gold and all L2 lanes
      stay green; the establishing-op `cur_class` → `None` transition is unchanged.

## Non-goals

- Scoping `rel_explicit`/`saw_tilde_bracket` or any arm-R relation-column state:
  those are introduced (or made leak-prone) by the arm-R column-narrowing feature
  and are scoped there, on top of this mechanism (stacked PR). No valid query
  masks via a `rel_explicit` leak without that feature, so scoping it here would
  be untestable dead code.

## Design

Pure L2 layer; no grammar/PDA change, no public-API change. One module:
`src/schema/scope.rs`. A lambda body is the one lexical region a nested pipeline
can appear in, so per-pipeline state is snapshotted at the body's **binder pipe**
and restored when the body's **delimiter closes**:

- `ScopeSave { depth, prev_cur_class }` — pushed in `on_pipe`, restored in
  `on_close` for the matching depth. Restores `cur_class` so a nested source
  class cannot re-key an outer `$var.member`. The restore runs **before** the
  establishing-op block, which still re-clears `cur_class` to `None` for a
  `project`/`groupBy` (relation → TDS row) — so that transition is unchanged.
- `BinderSave { depth, name, prev_class }` — pushed in `on_pipe` when a binder is
  bound, restored in `on_close`. Restores the `var_class` entry the binder
  shadowed, so a re-used binder name cannot outlive its lambda.

Both restore loops drain the depth-matching saves at the top of their stacks;
deeper scopes have already restored and popped, so matches are contiguous.

## API / contract impact

None. `ScopeTracker` and its helpers are `pub(crate)`. No PyO3 or public-API
change.

## Testing plan

- **Unit (`src/schema/scope.rs`)**: `a_nested_pipeline_source_class_does_not_leak_to_an_outer_navigation`
  (source-class leak) and `a_shadowed_binder_is_restored_when_the_inner_scope_closes`
  (binder-shadow leak). Each drives the real byte-PDA + tracker and asserts the
  outer navigation resolves against `A`; each verified to fail without its restore
  loop.
- **Regression**: the 269 gold (`l2_soundness`) and every L2 lane stay green.
- **Mutation**: `just test-mutation-diff` catches every mutant on the changed
  lines.

## Risks & rollout

- **Over-restoring `cur_class`** (undoing a legitimate change a lambda body makes
  for the outer continuation). Mitigated: a lambda body only changes `cur_class`
  via a nested source, which should not persist; the 269 gold + L2 lanes confirm
  no legitimate flow regresses.
- Rollback: revert the commit — the fields are additive and the prior behaviour
  is pure non-scoping.
