# PureCard specification

The complete, self-contained build spec for **PureCard** — a byte-level
grammar- and schema-constrained decoder for Legend Pure. It is the authoritative
source that [`../domain-model.md`](../domain-model.md) navigates and elaborates.

The spec was one monolith; it is split here by concern. Section numbers (`§N`)
are preserved verbatim as headings, so any `§N` reference resolves to the file
below.

| Sections                          | File                               | Covers                                                                                 |
| --------------------------------- | ---------------------------------- | -------------------------------------------------------------------------------------- |
| §1, §2, §10, §11, §12, Appendix B | [overview.md](overview.md)         | What PureCard is, the guarantee boundary, scope, milestones, risks, roadmap, prior art |
| §3, §4, §9                        | [architecture.md](architecture.md) | Architecture, the masking algorithm, the public Rust + PyO3 API                        |
| §5                                | [grammar.md](grammar.md)           | L1 — the emitted-Pure syntactic grammar                                                |
| §6, §7                            | [schema.md](schema.md)             | L2 — schema-consistency and the L1↔L2 contract                                         |
| §8, §13, §14                      | [testing.md](testing.md)           | The oracle-driven test strategy, the corpus, the Legend engine + CI                    |

For the testing *methodology* (the layered pyramid that operationalizes §8), see
[../methodology/decoder-testing.md](../methodology/decoder-testing.md).
