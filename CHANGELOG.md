# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/) and this project adheres to
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-07-16

The first feature release since the `v0.1.0` M0 skeleton: the full L1/L2 decoder
(M1–M5), the PyO3 boundary, and the modern-dialect grammar gaps. Because it renames
a public Python symbol, this is a **breaking** release (minor bump on 0.x).

### Changed

- **BREAKING (Python API):** the boundary exception is renamed
  `purecard.PureCardError` → `purecard.PureCARDError`, matching the **PureCARD**
  display name. Update `except purecard.PureCardError:` call sites accordingly.
  The lowercase `purecard` module / `import purecard` name is unchanged. (#33)
- Standardize the display name to **PureCARD** across docs and doc-comments. (#33)

### Added

- *(l1)* admit the Relation/Function API (arm-R, `~`-columns) — gap report G1 (#32)
- *(l1)* admit %latest/%latestdate milestoning literal (gap report G2) (#31)
- *(m5)* hardening — self-check, EOS finalization, error split, fuzz, benches (#14)
- *(m4)* python bindings via a PyO3 boundary + maturin wheel (#12)
- *(m3)* schema-consistency overlay (L2) (#10)
- *(m2)* per-step token mask + lazy per-state cache (#9)
- *(m1)* L1 emitted-Pure grammar — byte-PDA at 100% gold soundness (#7)
- *(m0)* oracle harness — corpus soundness wiring + Legend completeness probe (#3)

### Fixed

- *(l2)* drive scope off PDA lexeme boundaries (audit-2 H1/H2/M3) (#26)
- *(l2)* make schema narrowing BPE-prefix-aware (adversarial-review B1/M3/M4) (#23)

### Other

- exact action pins + named steps; fix the merge-time mutation gate (#29)
- *(l2)* kill the is_two_byte_op mutant in scope.rs (0-missed) (#28)
- *(qwen)* real-Qwen L2 soundness oracle (just target, not CI) — audit-2 C1/M2 (#27)
- *(l2)* kill the missed mutants in the BPE-prefix L2 code (0 missed) (#25)
- anti-doc-drift gates (stale-phrase lint, API doctests, fact assertions) (#22)
- sync all docs + in-code comments with the shipped M0-M5 implementation (#18)
- split DOMAIN.md into docs/spec/ (#4)
