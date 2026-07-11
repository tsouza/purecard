# Spec: M1 — L1 emitted-Pure grammar (byte-PDA)

- **Status:** In progress — both-arms scope approved and settled in ADR-0004.
  Soundness (G1/G2/G4) delivered and gated; precision now pinned by the negative
  reject corpus and the seeded completeness walker (G3/T8 — the engine
  compile-*rate* clause is deferred to M2, see G3). Remaining: property + mutation
  floors (G5/T9).
- **Created:** 2026-07-11
- **Owner:** decoder core (purecard crate)
- **Supersedes:** M0's throwaway `StubDecoder` / `tests/support/recognizer.rs`

## Problem

M0 shipped only wiring: a `StubDecoder` that accepts every byte and a soundness
test that asserts plumbing, not grammar. M1 is the first real decoder milestone.
It must bring the **shipped, dep-light** L1 recognizer into `src/` as a byte-level
pushdown automaton (PDA) that recognizes the emitted-Pure grammar, and promote the
soundness test from "the corpus streams" to the killer property:

> **The recognizer must never reach a dead state on a byte that a gold query
> actually emits, and must be in an accepting state at end-of-stream — for every
> in-scope gold query.**

Byte-acceptance is the strongest sound check available in M1: no model tokenizer
ships yet, and a token-mask over a token's bytes is exactly the union of the PDA's
byte transitions across those bytes — so if the PDA accepts the full byte string,
**every** tokenization of it is a walk the eventual (M2) token mask must admit.
Byte acceptance is therefore *necessary* for token-mask soundness, and sufficient
as M1's gate.

The done-criterion (overview §10): **100% gold-corpus soundness AND 100%
constrained-walk compile rate.**

## Scope decision (arm-A vs arm-C)

**Verified corpus reality** (`corpus/gold_queries.jsonl`, 5,034 records, scanned
this session):

| Arm                | Idiom                                                         | Discriminator             | Count     | Share     |
| ------------------ | ------------------------------------------------------------- | ------------------------- | --------: | --------: |
| **A** — relational | `Db->tableReference('default','T')->tableToTDS()->…` envelope | contains `tableReference` | **4,639** | **92.2%** |
| **C** — class-nav  | `Class.all()->filter(…)->…` (what §5 specs)                   | contains `.all()`         | **395**   | **7.8%**  |
| neither            | —                                                             | —                         | **0**     | 0%        |

The two envelopes are **mechanically, unambiguously** distinguishable at the
source token (`->tableReference(` vs `.all()->`) with **zero** overlap.

**Decision: grammar BOTH arms in M1 as one PDA branching at `source`.** Adopted
over arm-C-first-with-M1.5-quarantine, decisively:

1. **Arm-A is the product, not an edge case.** A recognizer that handles only the
   7.8% class-nav slice is a harness demo, not M1's decoder. The 92.2% relational
   arm is what the trained model overwhelmingly emits.
2. **The done-criterion means the whole corpus.** "100% gold soundness" is defined
   over the real 5,034. Both-arms meets it as written. Arm-C-first meets it only by
   a **human corpus renegotiation** (redefine M1's corpus to the 395) — a scope
   change the constitution forbids an agent from making unilaterally, and one that
   ships the minority slice first.
3. **The hard part is shared, so arm-A is additive, not a second engine.** The
   byte-PDA infrastructure — whitespace-skip layer, ident/strlit lexis with `''`
   doubling, the bracket + pipeline continuation stack, lambda frames, nested
   sub-pipeline recursion, the dead-state error tuple — is identical across arms.
   Arm-A adds ~10 leaf productions on that shared machinery.
4. **Big-bang risk is retired by sequencing, not by dropping scope.** The
   shared spine + arm-C land first as an internally-green checkpoint (the 395-query
   soundness gate passes and exercises the full harness end-to-end); then the arm-A
   productions widen the same PDA to the full 5,034. Every task is independently
   green. This captures the entire risk-reduction benefit of "arm-C first" without
   shipping an 8% decoder or renegotiating the corpus.
5. **The honest per-arm gate is kept anyway.** We retain a mechanical `Envelope`
   classifier (`tableReference` vs `.all()`) and the soundness gate **partitions**
   the corpus by it, asserting acceptance within each partition. A query that is
   neither cleanly arm-A nor cleanly arm-C reddens CI — silent half-acceptance is
   impossible even though both arms are accepted.

**Entailment — a blocking §5 spec edit.** §5 as written is arm-C only; §5.2 even
says `limit`/`extend` are "not observed — omit" because its §5.7 inventory is over
a **1,791-query pilot**, not today's 5,034. Adopting both arms therefore **requires
widening §5** to document the arm-A relational envelope. Widening only affects
completeness/precision (it admits *more*), never soundness — and since the corpus
is all-gold, it makes the spec match reality. But §5 is the authoritative grammar,
so this edit + its ADR is a **human decision** (see *Decisions for the human*).

## Goals

Mapped to the done-criterion; soundness-gate scope stated precisely.

- [x] **G1 — Real PDA in `src/`.** A hand-written byte-PDA replaces `StubDecoder`;
      the recognizer trait and impl live in shipped `src/`, not `tests/support/`.
- [x] **G2 — Soundness over the FULL 5,034 (both arms).** The killer test drives
      every gold `pure_text` byte-by-byte through the shipped PDA and asserts: never
      `DeadState`, and `is_complete()` at EOS. **Gate scope = all 5,034**, asserted
      per-envelope partition (4,639 arm-A + 395 arm-C), with an exact record count.
      This is the honest, unreduced scope — no quarantine.
- [~] **G3 — constrained-walk compile.** *Partially delivered.* The seeded PDA walk
      generator (`tests/support/walker.rs`) and its hermetic self-test
      (`tests/completeness_walks.rs`, in `just ci`) are landed, and the engine lane
      is wired (`tests/legend_completeness.rs`, `--features legend`) to round-trip
      every walk. The **100% compile-*rate*** assertion is **deferred to M2**: a
      random L1-accepting walk names classes/properties that don't resolve, so a
      real compile rate needs the §14.2 `grammarToJson` lowering *and* the L2 schema
      overlay that constrains walks to a model. Precision is meanwhile pinned by the
      negative reject corpus (`tests/precision_reject.rs`, hermetic).
- [x] **G4 — Oracle-driven tightening surface.** `DecodeError::DeadState` carries
      `{ offset, byte, state, stack_top }` so a soundness failure names exactly the
      byte/state/stack that rejected it (§8.6 relaxation loop).
- [~] **G5 — Gates stay green & dep-light.** *Partially delivered.* `just ci` green;
      core `[dependencies]` gains only `thiserror` (via an allowlist in the deplight
      gate — now hardened to resolve `package = "…"` aliases — not by disabling it);
      coverage ≥ 70%. The **mutation floor on `step`** is the remaining T9 item.

## Non-goals

- **Token-level `allowed_mask` / `accept_token` speculative masking** → M2 (needs
  the model tokenizer). M1 ships byte-level accept as the sound floor.
- **Mask cache** (per-token memoization) → M2.
- **L2 schema narrowing / `Schema` / `Vocab`-driven resolution** → M3. `schema` is
  always `None` in M1; L1 is purely syntactic and deliberately over-approximates
  (§5.6) — resolving *which* identifier is L2's job.
- **PyO3 `ffi` surface** → later milestone.
- **Compile-time / runtime EBNF-string parsing into transition tables.** Rejected
  (see *Design* and *Dependency vetting*): a hand-written match is the live
  automaton; the EBNF string is a *test oracle* only.
- Arm-A is **in scope** — explicitly not deferred.

## Design

### `src/` module layout

```text
src/
  lib.rs          // existing GuaranteeLevel; add re-exports + module decls
  vocab.rs        // existing Vocab (untouched)
  error.rs        // DecodeError (thiserror) — MOVED from tests/support, extended
  grammar/
    mod.rs        // CompiledGrammar, GrammarError, Envelope classifier, ACCEPTING set
    pda.rs        // State, Frame, Step, ByteClass consts, fn step() — the live automaton
    spec.rs       // canonical §5+armA grammar STRING + §5.7 inventory — TEST ORACLE ONLY, never executed
  session.rs      // DecoderSession { pda, grammar: &CompiledGrammar } — schema always None in M1
  recognizer.rs   // ByteRecognizer trait (moved from tests/support) + PdaRecognizer impl
```

### Grammar representation — hand-coded, not compiled from EBNF

The live automaton is an explicit Rust state machine in `pda.rs`. The §5 EBNF has
~40 recursive productions (`boolExpr`, `valueExpr`, sub-`pipeline` inside `join`/
`in`); a runtime EBNF interpreter would add a parser-combinator dependency (fails
vetting) plus a lowering pass — **two new untested soundness surfaces** — for zero
soundness gain over an explicit `match`. `spec.rs` holds the canonical grammar
string and the §5.7 construct inventory purely so the soundness test can assert the
PDA's accepted-construct set matches the documented one; it is never parsed for
execution.

### Byte-PDA: state + stack

```rust
pub struct Pda { state: State, stack: Vec<Frame> }

enum Step { Next(State), Push(Frame, State), Pop(State), Dead }

// Continuation returns + context-dependent bracket matchers (§4.2), unified.
enum Frame {
    Paren, Bracket, Brace,        // closer legal only if it matches the top frame
    ResumeAfterStep,              // after a "->step", return to pipeline
    ResumeAfterArg,               // after an arg in a comma-list
    ResumeSubquery,               // nested pipeline inside join(...) / ->in(pipeline.col)
}

enum State {
    Start, InWs,                  // whitespace-skip layer (see below)
    SimpleSource, BlockLet,       // "|" pipeline  vs  "{|" letBinding* pipeline "}"
    InIdent, Classpath,           // ident { "::" ident }
    InStrLit { escaped: bool },   // "'" body "'", '' doubling via `escaped`
    AfterDot, AfterArrow,         // ".all()" / ".getString(...)" ; "->" step
    // arm discrimination happens here:
    TableRefArgs, AfterTableToTds,// arm-A envelope
    // shared step machinery:
    StepArgs, InInt,
    // lambdas (both arms):
    LambdaBinder, TypedBinderColon, TypedBinderMult, // ident [":" classpath "[" ("1"|"*") "]"] "|"
    BraceMultiBinder,             // "{" typedBinder { "," typedBinder } "|" ... "}"  (join)
    LambdaBody,
    // ... one variant per production position; enumerated during implementation
}
```

- **`step` is pure:** `fn step(state: State, stack_top: Option<&Frame>, byte: u8) -> Step`.
  Char classes are named `const` byte ranges — `IDENT_START`, `IDENT_TAIL`, `WS`,
  `DIGIT` — never magic literals (constitution §4).
- **Brackets are context-dependent (§4.2):** `)` is `Pop` only if `stack_top` is
  `Paren`, `]` only if `Bracket`, `}` only if `Brace`; a mismatch is `Dead`.
- **Pipeline `->step`** pushes `ResumeAfterStep`; comma-lists push `ResumeAfterArg`;
  `join(<pipeline>, …)` and `->in(<pipeline>.col)` push `ResumeSubquery` so nested
  pipelines restore correctly (arm-A `join` embeds full `tableReference…tableToTDS`
  sub-pipelines — confirmed: `tableReference` occurs 8,455× across 4,639 queries).

### Whitespace handling — mandatory, both arms

Arm-A embeds `\n` and 2-space indentation *between* tokens and inside arg lists
(e.g. `->tableReference(\n  'default',\n  'Faculty'\n)`). WS (`0x20 0x09 0x0A 0x0D`)
is a **state-neutral skip layer**: consumed between tokens, never significant,
**never consumed inside `InIdent`/`InStrLit`** (a string body takes raw bytes until
the closing quote via the `''` sub-state). WS is designed in from byte one; arm-C
has little but the layer is identical.

### Arm-A construct inventory (empirically derived this session, to be locked in §5.7)

Beyond arm-C's productions, arm-A requires: `tableReference(strlit, strlit)`,
`tableToTDS()`; **3-arg string-named `agg('NAME', mapLambda, reduceLambda)`**;
**typed-multiplicity binders** `ident ":" classpath "[" ("1"|"*") "]" "|"` (incl.
`Integer[*]`, `meta::pure::tds::TDSRow[1]`); **brace multi-binder** for `join`
`{ b1:…[1], b2:…[1] | boolExpr }`; `join(<sub-pipeline>, JoinType.(INNER|LEFT_OUTER),
lambda)`; `renameColumns([ strlit "->" "pair" "(" strlit ")" … ])`; `extend([ col(…) … ])`;
`limit(int)`; **string-or-list `restrict`** (`restrict('Rank')` *and* `restrict([...])`)
and **string-key `groupBy`**; the `between(...)` boolPred. **Total raw occurrences**
(every appearance across the corpus, *not* distinct queries — a single query
repeats a construct, so these run higher than the per-query-containing counts):
`join` 3,803, `renameColumns` 8,979, `pair` 32,308, `limit` 665, `extend` 450,
`between` 35. These are the occurrence totals; the authoritative **queries-containing**
counts that the grammar is locked against live in `docs/spec/grammar.md` §5.7 (e.g.
`pair` occurs 32,308× but in 2,378 distinct queries; `limit`/`between` appear
≤once per query, so their two counts coincide). The implementation locks this
inventory against the corpus (task T7) — *"do not invent productions the corpus
doesn't exercise, do not omit ones it does"* (§5 core principle).

### The recognizer replacing StubDecoder

```rust
pub trait ByteRecognizer {                       // moved verbatim contract into src/
    fn accept_byte(&mut self, byte: u8) -> Result<(), DecodeError>;
    fn is_complete(&self) -> bool;               // stack.is_empty() && state ∈ ACCEPTING
    fn reset(&mut self);                          // state=Start, stack.clear() (keeps capacity, §9.1)
}

pub struct DecoderSession<'g> { pda: Pda, grammar: &'g CompiledGrammar /*, schema: None */ }
impl ByteRecognizer for DecoderSession<'_> { … }  // accept_byte folds step over one byte
// accept_token(bytes) = fold accept_byte over the token's bytes (no mask cache — M2).
```

`DecoderSession` owns an offset counter for error reporting. `accept_byte` maps a
`Step::Dead` to `Err(DecodeError::DeadState{…})`; otherwise applies the state/stack
transition and returns `Ok(())`.

### Error reporting for oracle-driven tightening

```rust
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("dead state at offset {offset} (byte {byte:#04x}) in {state} [stack top: {stack_top}]")]
    DeadState { offset: usize, byte: u8, state: &'static str, stack_top: &'static str },
}
```

`state`/`stack_top` are `&'static str` (cheap, no alloc). The recognizer stays
**source-agnostic** — it does not fabricate a `source_id`; the soundness harness
attaches the failing `record.source_id` when it reports, giving the full §8.6
tightening tuple `(source_id, offset, byte, state, stack_top)` without polluting the
core type. This extends M0's `DeadState { offset, byte }` (backward-compatible field
superset).

## API / contract impact

- **New shipped `src/` surface:** `error::DecodeError` (extended), `recognizer::{ByteRecognizer, PdaRecognizer/DecoderSession}`, `session::DecoderSession`, `grammar::{CompiledGrammar, GrammarError, Envelope}`. `lib.rs` re-exports the public ones under `#![deny(missing_docs)]`.
- **M0 `tests/support/recognizer.rs` is deleted, not extended** — its own doc-comment authorized wholesale replacement. `StubDecoder`, `DiesOn`, and `replay_bytes` go away; the byte-replay *driver* logic moves into the soundness test against the real PDA.
- **`tests/support/error.rs` `DecodeError` is superseded** by the shipped `src/error.rs`; the test binaries `use purecard::DecodeError` instead of the local copy (removing a DRY duplicate).
- **`[dependencies]` gains `thiserror`** (currently dev-only). This is the core's first runtime dep — anticipated by the Cargo comment ("decoder deps land with the milestones"). Version re-verified via `cargo add thiserror` at implementation time (constitution §2 "latest stable, verified"), not carried from the `2.0.18` dev-dep pin.
- **`cargo xtask check-core-deplight` (ADR-0003)** must change from "`[dependencies]` is empty" to "`[dependencies]` ⊆ { thiserror }" — an **allowlist tightening of the gate, never a disable**. The gate stays PROTECTED; it now enforces the exact intended dep set.

## Testing plan

**1. Soundness killer-test (hermetic, `tests/soundness_replay.rs`) — the gate.**
Promote from the stub. For every gold record: classify `Envelope`, drive the real
PDA over `pure_text.as_bytes()` one byte at a time, assert **never `DeadState`**,
then assert `is_complete()` at EOS. Assert partition counts exactly
(`ARM_A = 4639`, `ARM_C = 395`, total `EXPECTED_GOLD_RECORDS = 5034` — named
constants, not magic literals) so shrinkage, corruption, or a mis-partitioned query
all redden the gate. On any dead state, panic with the full tightening tuple.

**2. Completeness accepting-walks (engine-gated, separate job §14.4).** A PDA walker
from `Start` that at each state uniformly picks among *enabled* transitions
(honoring push/pop), bounded depth, halting at an accepting state. Seeded with a
committed list of `StdRng::seed_from_u64` seeds (named `const` seeds, not literals)
so CI reproduces byte-for-byte. Emit each walk's bytes to the engine lane
(`lambdaReturnType`); **target 100% compile.** Lives in the engine-backed job, never
the hermetic soundness job. Cover both arms' walks.

**3. Unit tests (`#[cfg(test)]` in `pda.rs`).** Per-production accept/reject;
bracket-matching context-dependence (`]` illegal when top is `Paren`); `''` string
doubling; WS-skip between tokens but not inside idents/strings; envelope
discrimination; the empty-key `groupBy([]…)` form; typed-multiplicity binder;
brace multi-binder.

**4. Property tests (§8.5, `proptest` — vet first).** (a) `reset` idempotence;
(b) `accept_token(bytes)` ≡ folding `accept_byte`; (c) **WS-insertion invariant** —
inserting WS between any two tokens of an accepted string preserves acceptance;
(d) bracket-balance: an accepted string has matched, correctly-nested brackets.

**5. Mutation (`cargo-mutants` — vet first).** Floor on `pda.rs::step` and the
`Envelope` classifier; floor set at M1's measured score and **ratchets up only**
(constitution §3). Coverage floor ≥ 70%.

**6. Failure → tightening loop (§8.6).** A `DeadState` on a gold byte is not a test
weakening target — it names the construct the grammar wrongly forbids. Fix = add the
missing production to `pda.rs` (and document it in §5.7), never relax the assertion.
Per constitution §5, each such fix also lands the unit test that pins that construct.

## Dependency vetting

- **Live automaton: hand-written, zero new runtime crates.** A PEG/parser-combinator
  or EBNF-interpreter dependency fails the vetting rubric — it adds indirection and a
  lowering surface for a 40-production recursive grammar with **no soundness gain**,
  and breaks the dep-light invariant. Verdict: **write our own** (the constitution's
  "library before writing" yields to "bespoke owns its edge cases" here — the PDA is
  the product).
- **`thiserror` → `[dependencies]`:** already vetted and in the lockfile (dev-dep);
  constitution mandates `thiserror` for lib error types. Re-verify current stable via
  `cargo add` at implementation time. **Adopt.**
- **`proptest`, `cargo-mutants`:** dev/tooling only, never shipped. Run the
  `dependency-vetting` skill and pin latest-stable-verified before adding. If either
  fails vetting, its layer degrades to hand-written cases (soundness gate does not
  depend on them).

## Risks & rollout

- **R1 — §5 is arm-C-only and must widen (blocking).** Both-arms is impossible until
  §5 documents arm-A. Mitigation: land the §5 edit + ADR *first* (human sign-off),
  then implement. **Escalated to `main`.**
- **R2 — over-approximation admitting invalid Pure.** L1 deliberately
  over-approximates (§5.6); L2/L3 tighten. Bounded and by-design; not a soundness
  risk. The completeness/engine lane catches walks that don't compile.
- **R3 — deep nesting blowup** (join sub-pipelines). Mitigation: the stack is a plain
  `Vec<Frame>`; walker depth is bounded; real corpus depth is small. No recursion in
  `step` (iterative stack), so no stack-overflow surface.
- **R4 — deplight-gate drift.** Mitigation: tighten the gate to the `{thiserror}`
  allowlist in the *same* PR that adds the dep, so a stray future dep still reddens.
- **Rollout:** sequenced PRs (below); each independently green. The full-5,034
  soundness gate flips green only at T7, after arm-A widening — before that the gate
  runs on the arm-C partition as an internal checkpoint.

## Implementation tasks

Each is a small, independently testable + committable PR (one change → one worktree
→ one PR).

- [x] **T0 — §5 spec widening + ADR** (human-gated). Document the arm-A relational
      envelope and its construct inventory in §5; record the both-arms scope decision
      as an ADR (ADR-0004). Blocks all code tasks.
- [x] **T1 — Core plumbing.** Move `DecodeError` into `src/error.rs` (extended
      fields); add `thiserror` to `[dependencies]`; tighten `check-core-deplight` to
      the `{thiserror}` allowlist; move the `ByteRecognizer` trait into
      `src/recognizer.rs`. Green: existing tests compile against the new surface.
- [x] **T2 — Lexis + WS layer.** `ByteClass` consts; `InWs` skip; `InIdent`/
      `Classpath` (`::`); `InStrLit{escaped}` with `''` doubling. Unit-green.
- [x] **T3 — Arm-C spine.** `.all()` source → pipeline → `->step` continuation stack
      → bracket frames. Bare-binder lambda `x|boolExpr`; nested subquery frames
      (`->in(pipeline.col)`). Wire the soundness test on the **395 arm-C** partition
      → green. (Full harness now exercised end-to-end against a sound gate.)
- [x] **T4 — Arm-C steps.** `filter`/`project`/`groupBy`/`restrict`/`sort`/`take`/
      `distinct`/`olapGroupBy`, `agg` (2-arg), reducers, `colAccess`/`toOne`. Arm-C
      partition still green.
- [x] **T5 — Arm-A envelope.** `->tableReference(str,str)->tableToTDS()`; `Envelope`
      classifier; branch at `source`. Arm-A queries begin accepting.
- [x] **T6 — Arm-A productions.** Typed-multiplicity + brace-multi binders; 3-arg
      string-named `agg`; string-or-list `restrict`/`groupBy`; `join`+`JoinType`+
      sub-pipeline; `renameColumns`/`pair`; `extend`/`col`; `limit`; `between`.
- [x] **T7 — Full soundness green.** Flip the gate to all **5,034**, per-partition
      assertions + exact counts. Lock §5.7 inventory against the corpus. **G2 done.**
- [~] **T8 — Completeness walker** (seeded, committed seeds). *Walker + hermetic
      self-test delivered* (`tests/support/walker.rs`, `tests/completeness_walks.rs`
      in `just ci`); the engine lane is wired behind `--features legend`. The
      **100%-compile-rate, both-arms** engine assertion is **deferred to M2**: it
      needs §14.2 `grammarToJson` lowering and the L2 schema overlay to make walks
      semantically resolvable (an unschema'd L1 walk cannot compile by
      construction). The precision that T8 was meant to prove is meanwhile pinned
      hermetically by the negative reject corpus (`tests/precision_reject.rs`).
- [ ] **T9 — Property + mutation.** Add §8.5 property tests; set the `cargo-mutants`
      floor on `step`/classifier; confirm coverage ≥ 70%. **G5 remaining.**

## Decisions for the human

1. **[TOP] Arm scope + §5 widening.** Approve **both arms in M1**, which *entails*
   widening §5 to document the arm-A relational envelope (`tableReference`/
   `tableToTDS`, 3-arg string-named `agg`, typed-multiplicity/brace binders, `join`+
   `JoinType`, `renameColumns`/`pair`, `extend`/`col`, `limit`, string-or-list
   `restrict`/`groupBy`, `between`) + an ADR. **Recommend: approve.** Rationale:
   arm-A is 92.2% of gold; the done-criterion is defined over the full 5,034; the PDA
   infrastructure is shared so arm-A is additive; big-bang risk is retired by
   sequencing (arm-C green checkpoint at T3–T4 before arm-A widening). The only
   alternative — arm-C-first — requires *this same human* to renegotiate M1's corpus
   down to 395 (7.8%) and ships the minority slice. **Escalate to `main` before T1.**

2. **Core gains its first runtime dependency (`thiserror`).** M1 moves `DeadState`
   into shipped `src/`, so the core `[dependencies]` table stops being empty and
   `check-core-deplight` becomes a `{thiserror}` allowlist. **Recommend: approve** —
   anticipated by ADR-0003's comment and mandated by the constitution's "thiserror in
   libs." Confirm no objection to the deplight gate changing from empty-set to
   singleton-allowlist.

3. **Mutation floor initial value.** M1 sets the first `cargo-mutants` floor on
   `step`. **Recommend:** set it at M1's *measured* score (ratchet-up-only per §3),
   rather than picking an aspirational number now. Confirm the mechanism (measure →
   pin → only-raise) is acceptable for the first floor.
