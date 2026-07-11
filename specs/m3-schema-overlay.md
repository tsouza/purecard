# Spec: M3 — L2 schema-consistency overlay

- **Status:** Implemented (serde-into-core approved; N3/N1/N2/N6/T1 shipped)
- **Created:** 2026-07-11
- **Owner:** `thiago@squid.cloud`

## Problem

M1 gives L1 (parses). M2 makes it fast. Neither knows a specific schema, so L1 accepts phantom classes/properties (`sallary`, `Person`) that parse but never resolve. M3 is the L2 overlay: at identifier and type positions, intersect the L1 mask with a schema-legal set. §6.1 says L1 *cannot* do this — resolving `$var.a.b.c` needs a typed scope (bound var, nav class reached, running multiplicity) threaded through the parse, which a PDA cannot carry. **L2 only ever narrows** (intersect, never set); the killer property is it must never clear a bit a real gold query needs (§8.1 soundness).

## Scope decision

Honest corpus reality: 8 fixtures back **269** in-scope gold, split **256 arm-A / 13 arm-C**. Arm-A (pure relational) exercises only N6 + a table-exists check; the §6 property/type rules fire **only on 13 arm-C queries + 5 hand-authored fixtures**. (These figures are not prose to trust — they are named constants `IN_SCOPE_TOTAL`/`IN_SCOPE_ARM_A`/`IN_SCOPE_ARM_C` asserted in `tests/l2_soundness.rs`, so they fail the gate if the corpus or fixtures drift.)

- **Ships (soundness+precision proven):** N3 (source class exists), N1/N2 (member/nav after dot), N6 (relation columns), T1 (cmpop operand type-class — car_1 `horsepower:String` is the precision lever).
- **Defers:** N5-as-distinct (the `navigable` map still ships, N1/N2 need it), T2/T3/T4/T6/T7 (thin coverage), N4/T5 (inert — no corpus enum; asserted to mask nothing), full olap re-typing.
- The `ScopeTracker` (S1–S3) ships **whole** — a partial machine is a soundness hazard.

## Key decisions resolved

**Integration — scope-in-session, NOT position-tagged-PDA. `pda.rs` gets no change.** Rationale: a `ScopeTracker` must exist regardless (only it holds the context-sensitive facts §6.1 forbids the PDA); once it advances from `(bytes, State, Frame)` in lockstep with `accept_token`, it already disambiguates `AfterDot` into N1/N2 via a `steps` counter. `Step::Tagged`/`last_tag()` would be redundant AND would perturb M2's cache bijection and `Pda::at()` re-probing. The PDA supplies the lexical anchor; the tracker supplies the semantic `L2Position`.

**The intersect point** (verified against the real commented hook at session.rs L102–106, after `indep` copy + deferred re-probe + EOS set/clear):

```text
mask = cache[state].indep ∩ runtime(deferred) ∩ schema_narrow(pos, scope)
```

One word-wise `intersect`; `schema_narrow` seeds the EOS bit so a complete query stays completable; `schema = None` skips the block (zero L1 cost). Because it is a pure `intersect`, `L2 ⊆ L1` is structural, not just tested.

**Dependency — adopt serde + serde_json into core `[dependencies]`** (today dev-only), widening the PROTECTED `check-core-deplight` allowlist `{thiserror}` → `{thiserror, serde, serde_json}`. `from_json` is shipped host-facing code (§6.3, §9), so its parser can't stay dev-only; serde is vetted, unsafe-free, already in the lockfile; bespoke JSON fails "library before writing." This is the top human decision.

## Goals (mapped to done-criterion)

G1 L2 soundness on all 269 (no shipped rule masks a gold token) · G2 phantom precision (N1/N2/N3 masked) · G3 type-mismatch precision (T1 masked) · G4 `L2 ⊆ L1` never widens · G5 generalization on 3 OOS held-out fixtures · G6 zero L1 cost when `schema = None`.

## Non-goals

L3 faithfulness · PyO3/ffi · the 4764 fixtureless gold · deferred rules · a `.md` parser · any `pda.rs`/`State`/M2-cache change.

## API impact

New `schema::{model, scope, narrow}`; `Schema::from_json` (§9 ingress); additive `DecoderSession::with_schema(grammar, schema)` leaving `new(grammar)` byte-compatible for M0–M2. Serde dev→core + allowlist widen (PROTECTED, justified in PR).

## Testing

(1) **Killer soundness** — 269 gold × `from_json` (JSON fixtures, so the ingress is under test), assert gold's next token ∈ `allowed_mask()` every step. (2) **Precision** — mechanically swap one token per arm-C stream: phantom ident/class, type-mismatch RHS → assert masked. (3) **Property/mutation** — `L2 ⊆ L1`, lambda push/pop balance, mult monotone, `cargo-mutants` over S1–S3 + each shipped predicate, N4/T5 inert guard. (4) **Generalization** — 3 OOS fixtures. **Honest caveat led in the PR:** load-bearing surface is 13 arm-C gold + 5 fixtures, not "269"; follow-up = commit arm-C-heavy fixtures (geo/cre_Theme_park/bike_1/hr_1).

## Implementation tasks

13 ordered, independently-testable steps: model.rs → dep+gate → navigable ctor → soundness harness (red) → N3 → ScopeTracker S1–S3 → N1/N2 → N6 → T1 → session wiring+EOS → precision → property/mutants → PR.

## Decisions for the human

1. **(Top) serde+serde_json into core + allowlist widen** — recommend **approve** (PROTECTED-gate widen; no smaller option keeps L2 ingress in-crate). 2. Scope N3/N1/N2/N6/T1, defer rest — recommend approve (ships only what the corpus proves sound). 3. scope-in-session vs tagged-PDA — recommend scope-in-session (protects M2 cache). 4. `with_schema` vs breaking `new(g, Option<Schema>)` — recommend `with_schema`.

## Outcome (as implemented)

All four decisions were taken as recommended. Shipped: `schema::{model, scope,
narrow}`, `Schema::from_json`, `DecoderSession::with_schema`; `serde` +
`serde_json` moved into core `[dependencies]` with the `check-core-deplight`
allowlist widened to `{ thiserror, serde, serde_json }` (a PROTECTED-gate widen,
justified above). `pda.rs`/`State`/the M2 cache are untouched.

Test results: L2 soundness green on **all 269** in-scope gold queries (no shipped
rule masks a gold token); M1 gold soundness still **5034/5034**; L2 precision
counterfactuals mask phantom classes/properties, type-mismatched operands, and
unemitted columns (incl. on the 3 OOS held-out schemas); `L2 ⊆ L1` holds at every
step of all 269.

**Honest coverage caveat (lead with this).** The load-bearing narrowing surface is
the **13 arm-C** gold queries plus the 5 hand-authored arm-C fixtures — not "269".
The 256 arm-A queries exercise only the N6 relation-column check plus a
table-exists check; the §6 property/type rules (N1/N2/T1) fire only on arm-C.
Highest-value follow-up: commit arm-C-heavy fixtures (geo/cre_Theme_park/bike_1/hr_1).
