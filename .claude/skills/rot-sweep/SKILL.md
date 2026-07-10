---
name: rot-sweep
description: >-
  Run a periodic deep audit ("rot sweep") that catches decay a single-change
  review misses — dead code, duplication, complexity creep, semantic DRY,
  design smells. Use when the user says "sweep", "rot sweep", "deep audit",
  "code health pass", or on a schedule. Runs deterministic L1 tools first, then
  LLM-judges only the residue, and promotes recurring findings into new L1 rules.
---

# Rot Sweep — Deep Periodic Audit

A rot sweep is the scheduled, whole-repo audit that per-change review can't do:
it looks for accumulated decay across the codebase. The core idea is **cost
discipline** — run cheap deterministic analyzers first, and spend expensive LLM
judgment only on what those can't decide. Every recurring finding then gets
*promoted* into a new deterministic rule, so the next sweep is cheaper. Over time
the LLM's job shrinks and the token cost trends down.

## Two-phase design

### Phase L1 — deterministic tools (run these first, always)

Fast, reproducible, zero-token. Run the whole battery via:

```sh
just sweep
```

which drives (add any not yet wired into the target):

- **cargo-machete** — unused dependencies.
- **ast-grep** — repo-specific structural anti-pattern rules (the promotion
  target below; rules live in the repo's ast-grep config).
- **dylint** (the `lints/` crate) — custom Rust lints for project-specific rules.
- **duplication** — copy-paste detection (e.g. a token-based duplication checker).
- **complexity** — cyclomatic/cognitive complexity thresholds.
- **postponed-marker** — the `postponed-marker.sh` hook logic: TODO/FIXME/
  `todo!()`/`unimplemented!()`/`#[ignore]` should already be zero, but sweep-wide
  catches anything that slipped in.

Fix or file everything L1 surfaces before spending any LLM budget. If L1 is
noisy, tune the rule — deterministic false positives are cheap to fix once.

### Phase L2 — LLM judgment on the residue ONLY

Only after L1 is clean, apply LLM judgment to what tools structurally cannot
decide:

- **Semantic DRY** — logic duplicated in *different shapes* (same idea, different
  code) that a token-diff duplication checker misses.
- **Nonsense / dead intent** — code that "works" but doesn't mean anything: no-op
  abstractions, misleading names, comments that lie.
- **Design smells** — leaky module boundaries, a crate reaching across layers
  (domain depending on infra), god-objects, primitive obsession.

Do **not** re-derive with the LLM anything L1 already decides — that's wasted
budget. Judge the residue, not the repo.

## Record findings → `docs/lessons.md`

Append every material finding as a dated entry:

```md
## <YYYY-MM-DD> — rot-sweep
- Finding: <what>  @ <path(s)>
  Trigger: <how it was found: L1 tool name | L2 judgment>
  Confidence: high | med | low
  Action: <fixed in PR #… | branched | rule promoted>
```

`docs/lessons.md` is the institutional memory; each sweep reads prior entries so
it doesn't re-litigate settled calls.

## Promotion rule (this is the point)

**Any finding class that recurs `N = 3` times gets promoted from L2 (LLM
judgment) into an L1 deterministic rule** — an **ast-grep** pattern or a
**dylint** lint. Once promoted:

- It's caught for free on every future sweep *and* in per-change `just lint`.
- The LLM never has to reason about that class again.
- Record the promotion in `docs/lessons.md` and add the rule with a test.

This is "fix the system, not the instance" applied to review itself: the sweep
teaches the deterministic layer, and the deterministic layer shrinks future token
cost. A sweep that promotes at least one rule has paid for itself twice.

## Output

- L1: list of tool findings (fixed / filed).
- L2: judged residue with confidence.
- `docs/lessons.md` updated.
- Any `N=3` class promoted to an ast-grep/dylint rule (with a test) this sweep.
