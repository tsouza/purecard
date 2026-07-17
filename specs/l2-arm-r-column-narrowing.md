# Spec: l2-arm-r-column-narrowing

- Status: draft
- Created: 2026-07-17
- Owner: (AI engineer)

## Problem

The L2 gap report's "better fix" (the precision upgrade deferred by PR #35). In
an arm-R aggregation pipeline the relation-row binder's column access
`$x.<Col>` currently degrades to **pass-through** (`L2Position::None`): sound,
but a *phantom* column reference that names no emitted column is not masked.

```text
…->project(~[Week: …, Segment: …, GC: x|$x.REVENUE.netRevenueExPcc])
  ->groupBy(~[Week, Segment], ~'Gross Credits': x|$x.GC : y|$y->sum())
```

`$x.GC` is a real projected column (must stream); `$x.zzNotAColumn` is a
phantom (should be masked). Today both pass. This tightens precision to mask the
phantom while never masking a real column (soundness is PROTECTED).

## Goals

- [ ] Narrow a **bare-ident** column access `$x.<Col>` on an **arm-R
      relation-row** binder against the **emitted-column universe**, so a phantom
      column name is masked.
- [ ] Preserve soundness: never mask a column the query actually emitted upstream
      (the universe is an accumulated **superset**, exactly as N6 already is for
      quoted columns).
- [ ] Zero behaviour change for arm-A/TDS relation rows (`$r.getString('X')`
      stays admissible — the getter methods are not columns).

## Non-goals

- Window/brace-lambda binders (`{p,w,r|$r.v}`): left as pass-through (sound, less
  precise) — the reducer frame binds no tracked var today.
- (No residual here: nested subqueries headed by a navigation
  (`$x.rel->groupBy(~[…])`) as well as by `Class.all()` are both scoped — see
  Design §4.)
- Per-stage column-set liveness (dropping a column that an earlier stage removed):
  we keep the accumulated-superset model N6 uses, so a stale but once-emitted
  column stays admissible. Sound, deliberately imprecise.
- Typing a column to its primitive class for a following comparison (a T1-style
  lever on columns) — separate future work.

## Design

All changes are in the pure L2 layer; no grammar/PDA change, no public-API
change. Modules: `src/schema/scope.rs` (the tracker), `src/schema/narrow.rs`
(one new narrowing branch), `L2Position` (one new variant).

**1. Column universe (superset).** The tracker already records every emitted
single-quoted string into an emitted-column set (arm-A N6, `~'Gross Credits'`).
Extend it to also record the arm-R **column names** introduced with `~`:

- a bare `~Col` reference — the identifier whose anchor pre-state is `SawTilde`;
- a `~[…]` column-set key — the identifier at `ExpectValue` directly inside a
  tilde bracket (`~[Week, Segment]`, and the `Week`/`Segment`/`GC` before the
  `:` in `~[Week: …]`).

Recording is *generous by construction*: over-recording an identifier only lets
more through (loses precision), never masks — so the set stays a superset of the
columns actually live on any relation row. Every arm-R column is introduced by a
`~`-construct, so a semantically-valid `$x.Col` always finds `Col` in the set.

**2. Arm-R relation-row binders.** A `~[…]` anywhere latches `saw_tilde_bracket`
(arm-R established; arm-A never opens one). When a map-lambda binder pipe binds a
variable whose receiver is unknown (`None`) **and** a relation exists
(`rel_explicit`) **and** `saw_tilde_bracket`, the variable is a **relation row**,
tracked in `relation_row_vars` rather than `var_class`. Inside `project(~[Col:
x|$x.prop])` the binder's receiver is the *source class* (not `None`), so `x`
stays class-typed there and `$x.prop` is still N1 — unchanged.

**3. Narrowing.** `on_dot` over a `relation_row_var` sets `dot_is_column`;
`opening_position(AfterDot)` then yields the new `L2Position::RelationColumn`
instead of `Member`. `resolve_member` treats a column nav as terminal (no schema
member, cursor cleared → a following `.` degrades to pass-through). The narrower
adds a `RelationColumn` branch: a `TrieKind::Ident` trie built from the **raw**
(unquoted) column universe — the bare-ident dual of the existing quoted `Column`
branch, which keeps building `quote(c)`. Both draw from the same raw universe.

**Why arm-A is untouched:** a pure arm-A query never opens a `~[`, so
`saw_tilde_bracket` is false, no var becomes a relation row, and `$r.getString`
keeps its N1-`None`/getter behaviour. The 256 arm-A gold queries prove this.

**4. Lexical scoping (soundness).** Without it, a nested subquery leaks tracker
state to the enclosing pipeline and masks a valid outer token. A lambda body is
the one lexical region a nested pipeline can appear in, so all per-pipeline state
is snapshotted at the body's binder pipe (a `ScopeSave`) and restored when the
body's delimiter closes — covering nested pipelines headed by `Class.all()` *and*
by a navigation (`$x.rel->groupBy(~[…])`) uniformly. Three fields are scoped:

- *Bound class* (`cur_class`): a nested source class cannot re-key an outer
  `$var.member` navigation to the wrong class and mask a valid member. The restore
  runs before the establishing-op block, which still re-clears `cur_class` to
  `None` for a `project`/`groupBy` (relation → TDS row).
- *Arm/relation state* (`rel_explicit`, `saw_tilde_bracket`): an inner arm-R
  subquery cannot leak arm-R onto an outer arm-A pipeline whose TDS getter would
  then be masked as a phantom column. A nested `all()` source additionally *resets*
  these on entry, so the subquery classifies its own binders against a clean
  baseline; the enclosing body's `ScopeSave` restores the outer values on close.
- *Binder bindings*: a binder saves the `var_class`/relation-row binding it
  shadows at its `|` and restores it on close, so a nested subquery reusing an
  outer binder name cannot leave the outer navigation reclassified (this also
  closes a pre-existing `var_class` shadowing hole).

The `cur_class` leak is *pre-existing* (present before this feature); it is folded
here (constitution §6) because it is the same nested-pipeline state the arm/binder
scopings restore, through the identical `ScopeSave` structure — fixing two of
three would leave a known sibling hole (§5).

## API / contract impact

None. `L2Position` and the tracker are `pub(crate)`. No PyO3 surface change, no
public-API change (`cargo public-api` unaffected).

## Testing plan

- **Unit (`src/schema/scope.rs`)**: a relation-row binder's `$x.Col` yields
  `RelationColumn`; the emitted universe contains project/groupBy `~`-keys and
  bare `~col`; inside `project` the binder stays `Member(source)`; an arm-A
  `filter(r|$r.getString(…))` keeps `r` off the relation-row path.
- **Unit (`src/schema/narrow.rs`)**: `RelationColumn` admits an emitted bare-ident
  column, masks an unemitted one, and passes an incomplete prefix through.
- **Integration (`tests/l2_precision.rs`)**: end-to-end via
  `DecoderSession::with_schema` on `car_1` — the real projected column streams
  (soundness) and a phantom column is masked (precision), through the full
  grammar+scope+narrower.
- **Regression**: the 269 gold (`l2_soundness`) and every existing L2 lane stay
  green; each new test verified to fail without the change.
- **Mutation**: `just test-mutation-diff` catches every mutant on the changed
  lines.

## Risks & rollout

- **Soundness (masking a real column).** Mitigated by the accumulated-superset
  universe and by gating on `saw_tilde_bracket`; the killer gate is the gold +
  precision replay. If any real token is masked, the gate reddens and names it.
- **Over-narrowing an arm-R `.method` (if one exists).** The seeds show arm-R rows
  use only bare-ident column access; a stray `.method` would redden the soundness
  replay, at which point the method name is added to the admitted set. No silent
  masking.
- Rollback: revert the commit — the field additions are additive and the prior
  behaviour is pure pass-through.
