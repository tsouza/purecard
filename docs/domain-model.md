# Domain Model

The evolving statement of **what PureCard is and does**. It is the elaboration of
the domain section of [`../constitution.md`](../constitution.md): the constitution
states the non-negotiable domain *rules*; this file describes the *entities,
workflows, and invariants* those rules govern. When the two disagree, the
constitution wins.

The authoritative, self-contained specification is [the full spec](spec/README.md);
this file is the navigable model over it. This document is **EVOLVABLE** and grows
one reviewer-approved PR at a time.

## How to use this file

- When a feature introduces a new domain concept, add or update its entry here as
  part of the same PR that adds the code.
- Keep it a **model**, not a changelog. Describe the current truth; history lives
  in git and in [`decisions/`](decisions/).
- No filler. If an entry says nothing a reader couldn't infer from the type names,
  delete it.
- Cross-link the feature spec (`specs/<name>.md`, from `just spec`) and the
  `docs/spec/` section that introduced each concept.

---

## Entities

### GuaranteeLevel

**What it is.** The three nested guarantees a constrained decoder can offer:
`Syntactic` (L1), `SchemaConsistent` (L2), and `Faithful` (L3). PureCard enforces
up to L2 and refuses L3. Implemented in `src/lib.rs`.

**Invariants.** The levels form a strict containment hierarchy —
`Faithful ⊂ SchemaConsistent ⊂ Syntactic`. A stronger guarantee implies every
weaker one. `Faithful` is never enforceable at decode time (the mask never sees
the question's intent).

**Relationships.** Every other entity exists to move a model's output up this
hierarchy: the grammar/PDA delivers L1; the schema overlay delivers L2.

**Introduced by.** [`spec/overview.md`](spec/overview.md) §1. *(Skeleton — the entities below are specified in
the spec and land across milestones M0–M4; they are recorded here as the target
model, not as shipped code.)*

### Vocab

**What it is.** The model vocabulary as raw byte strings per token id, plus a byte
trie. A token is admissible iff feeding its raw bytes advances the byte-level
automaton to a non-dead state — sidestepping subword-boundary alignment entirely.

**Introduced by.** [`spec/architecture.md`](spec/architecture.md) §4.1, §4.4, §9.1.

### PureGrammar / CompiledGrammar

**What it is.** The L1 context-free skeleton of the *emitted subset* of Pure
(class-anchored relation pipelines), compiled into a pushdown automaton with
per-state context-independent mask caches.

**Invariants.** The grammar is derived from, and testable against, the gold
corpus: any production a gold query violates is wrong and must be relaxed; any
construct the corpus lacks stays out until a gold query adds it (oracle-driven,
never speculative).

**Introduced by.** [`spec/architecture.md`](spec/architecture.md) §3, §4 and [`spec/grammar.md`](spec/grammar.md) §5.

### Schema

**What it is.** The per-database structure the L2 overlay consults: classes →
`{property → (type, multiplicity)}`, plus association navigabilities and enums.
Built **host-side** (never by the decoder) from PMCD/MCP and handed to the session
at init as JSON.

**Invariants.** `PropType` is a three-way split (`Primitive` | `ClassRef` |
`EnumRef`) — load-bearing, because it decides whether `.` after a property
continues navigation, terminates a value, or narrows an enum. Association
navigability is directional (§6.2.3): getting the direction backwards is a
soundness bug.

**Introduced by.** [`spec/schema.md`](spec/schema.md) §6.2.

### Scope

**What it is.** The typed-scope state threaded through the parse: `ClassScope`
(a row of a class) or `RelationScope` (a TDS/relation with named columns). The top
of the scope stack determines which identifiers L2 admits.

**Shipped (M3).** `Schema::from_json` + `DecoderSession::with_schema` deliver L2 as
`src/schema/{model, scope, narrow}`. The scope machine (`ScopeTracker`) advances in
lockstep with `accept_token`, and `allowed_mask` intersects the L1 mask with the
schema-legal set. The shipped rules are N3 (source-class exists), N1/N2 (member/nav
after `.`), N6 (relation-column strings), and T1 (comparison operand type-class);
N5-as-a-distinct-rule, T2/T3/T4/T6/T7, and the inert N4/T5 are deferred (see the
module docs and `specs/m3-schema-overlay.md`). Soundness holds on all 269
fixture-backed gold queries; the load-bearing narrowing surface is the 13 arm-C
queries (arm-A exercises only N6 + a table-exists check).

**Introduced by.** [`spec/schema.md`](spec/schema.md) §6.4.

### DecoderSession

**What it is.** The per-generation object holding PDA state, stack, and scope over
a `CompiledGrammar`. Its surface: `allowed_mask()`, `accept_token()`,
`is_complete()`, `reset()`. The `#[cfg(feature = "python")]` PyO3 bindings wrap
exactly this.

**Introduced by.** [`spec/architecture.md`](spec/architecture.md) §9.

## Workflows

### Compile a grammar (once per model + grammar)

`PureGrammar::from_spec` → `compile(vocab)` builds the PDA and lazy per-state mask
caches. [`spec/architecture.md`](spec/architecture.md) §4.5, §9.1.

### Constrain one generation (per decode step)

`mask = cache[state] ∩ runtime-stack-check ∩ schema-narrow(scope)`; the host
`&`-masks the logits, samples, and calls `accept_token`; stop when `is_complete()`
and EOS is sampled. Constraint applies only to the **final-query span** of a
trajectory. [`spec/architecture.md`](spec/architecture.md) §3.3, §4.3, §9.3.

## Cross-cutting invariants

- **Soundness (the killer property).** The mask must never forbid a token that a
  verified gold query actually emits. Replaying the 5,034-query gold corpus and
  asserting every next token is in `allowed_mask()` is the always-on gate
  ([`spec/testing.md`](spec/testing.md) §8.1) — and it runs offline, with no Legend engine.
- **L2 only narrows, never widens.** The schema overlay intersects L1's terminal
  set; it can only remove admissible tokens, never add them ([`spec/architecture.md`](spec/architecture.md) §3.1).
- **Completeness.** Constrained generations must compile against the real Legend
  engine; a compile failure is a grammar/overlay gap to tighten oracle-driven
  ([`spec/testing.md`](spec/testing.md) §8.2).

## Glossary

- **L1 / L2 / L3** — syntactic / schema-consistent / faithful; see
  [`GuaranteeLevel`](#guaranteelevel).
- **Emitted subset** — the fragment of Pure the trained model actually produces
  (class-anchored relation pipelines); the grammar recognizes only this.
- **PDA** — the byte-level pushdown automaton implementing L1.
- **PMCD** — PureModelContextData, the Legend model structure the host turns into
  a `Schema`.
- **Soundness / completeness** — never mask a valid continuation / never lead the
  model into a dead end. Both are mechanically testable against the corpus and the
  engine oracle.
- **Faithfulness** — the query means what was asked. Out of scope; PureCard never
  claims it.
