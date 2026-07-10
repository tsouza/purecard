# PureCard test corpus

Distilled from the **pure-lingua** project (the NL→Legend-Pure training effort) — the correctness
oracle backbone for PureCard. See `DOMAIN.md` for how these are used (§ Test corpus).

## Contents

- `gold_queries.jsonl` — 5,034 unique execution-verified gold Pure queries across 161 databases.
  One JSON object per line: `{db_id, source_id, arm, constructs[], pure_text}`.
  `arm`: "A" = relational/tableToTDS idiom, "C" = class-navigation idiom.
  **SOUNDNESS oracle**: replay each through the L1 decoder — any gold token the mask forbids is a
  grammar bug. Fully OFFLINE (no Legend engine needed). Also the empirical basis of the L1 grammar.
  Distilled from pure-lingua `data/phase2/{armA,armC}_*.jsonl` (accepted records; full dir 231MB → 4.8MB here).
- `schemas/*.md` — 8 database schema context files (autogen Pure classes + associations + exec
  coords) for 5 pilot + 3 out-of-sample dbs. **L2 (schema-consistency) test inputs.** Workspace ids
  inside are stale/ephemeral — only the class/property/association structure matters.
- `legend-stack/` — the Legend engine docker-compose + configs (engine 4.113.0 + fs-SDLC 0.195.0,
  anonymous auth) for the COMPLETENESS oracle (compiling generated queries). See DOMAIN.md § Legend setup.

## Provenance

Origin repo: pure-lingua (`data/phase2/`, `data/pilot/armC_ctx_*.md`, `data/pilot/oos_ctx_*.md`,
`infra/legend-stack/`). Faithfulness of the gold queries: execution-equivalence verified against
real data (see the pure-lingua Gate-2 report). CC BY-SA 4.0 (curated corpus lineage — Spider-derived).
