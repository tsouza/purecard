# PureCard specification

**A Rust grammar/schema-constrained decoder for Legend Pure (a "PICARD-for-Pure"
constrained-decoding library).**

- **OSS project name:** `PureCard` — _Pure_ + _PICARD_ lineage; reads as the
  "reference **card** of legal moves" for Pure generation.
- **Crate / repo:** `purecard` (internal Rust module name in this spec:
  `picard_pure`; the two names are interchangeable — the published crate is
  `purecard`).
- **Status:** Design, ready for implementation.

Together the files below are the _complete_ build spec: a fresh engineer (or a
fresh Claude instance) can build PureCard end-to-end from them alone — no other
design docs. The only external things the reader must fetch are (a) the _test
corpus_ of gold Pure queries and (b) a running _Legend engine_ — both are
data/services, not prose, with locations given in [`testing.md`](testing.md) §8.
General Rust workspace conventions, CI, and agentic dev setup are out of scope.

Context in one line: an upstream project ("pure-lingua") trains an LLM to emit
Legend Pure queries; at single-shot serving we want _guaranteed-valid_ output in
one forward pass (no compile-repair round-trip). PureCard provides that guarantee
via constrained decoding. This spec is the authoritative source that
[`../domain-model.md`](../domain-model.md) navigates and elaborates.

Section numbers (`§N`) are preserved verbatim as headings, so any `§N` reference
resolves to the file below.

| Sections                          | File                               | Covers                                                                                 |
| --------------------------------- | ---------------------------------- | -------------------------------------------------------------------------------------- |
| §1, §2, §10, §11, §12, Appendix B | [overview.md](overview.md)         | What PureCard is, the guarantee boundary, scope, milestones, risks, roadmap, prior art |
| §3, §4, §9                        | [architecture.md](architecture.md) | Architecture, the masking algorithm, the public Rust + PyO3 API                        |
| §5                                | [grammar.md](grammar.md)           | L1 — the emitted-Pure syntactic grammar                                                |
| §6, §7                            | [schema.md](schema.md)             | L2 — schema-consistency and the L1↔L2 contract                                         |
| §8, §13, §14                      | [testing.md](testing.md)           | The oracle-driven test strategy, the corpus, the Legend engine + CI                    |

For the testing *methodology* (the layered pyramid that operationalizes §8), see
[../methodology/decoder-testing.md](../methodology/decoder-testing.md).
