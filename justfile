# justfile — the human (and agent) frontend for this workspace.
#
# `just` is the ONLY entry point you should need for day-to-day work. Every
# target here is also what CI runs, so "green locally" means "green in CI".
#
# Design rules for this file (see CLAUDE.md):
#   * The justfile is the frontend. If a workflow is missing a target, add it.
#   * Two tiers below a recipe, picked by whether there's real logic:
#       - Simple pass-through to one tool -> call it directly (`cargo deny check`).
#       - Real control flow (branching, loops, sequencing, templating) ->
#         `cargo xtask <subcommand>` (typed Rust), never inline shell here.
#     (See constitution.md §2: nested `cargo xtask` -> `cargo <plugin>` calls can
#     mangle the plugin's argv — reserve xtask for logic, not pass-throughs.)
#   * Every tool referenced in CI has a target here, and vice-versa.

set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := false

# Default: list all available recipes.
default:
    @just --list

# ---------------------------------------------------------------------------
# Formatting & linting
# ---------------------------------------------------------------------------

# Format all code in place.
fmt:
    cargo fmt --all

# Verify formatting without modifying files (CI gate).
fmt-check:
    cargo fmt --all -- --check

# Clippy with warnings denied across all targets and features.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Lint + auto-fix markdown (aligns tables for MD060, then markdownlint --fix).
lint-md:
    bun scripts/lib/align-md-tables.mjs $(git ls-files '*.md')
    bunx markdownlint-cli2 --fix "**/*.md"

# Verify commit messages on this branch follow Conventional Commits.
lint-commits:
    bunx commitlint --from origin/main --to HEAD

# Lint GitHub Actions workflows.
lint-actions:
    actionlint

# Static-analyze GitHub Actions workflows for security issues (SHA-pinned
# `uses:`, template injection, over-broad permissions, …) — the same audit the
# lint.yml `zizmor` CI gate enforces. Reads .github/zizmor.yml (accepted-finding
# ignores). Online audits are skipped locally without a GitHub token.
zizmor:
    zizmor .github/

# ---------------------------------------------------------------------------
# Testing (layered: unit -> integration -> chaos -> mutation -> fuzz)
# ---------------------------------------------------------------------------

# Run the full test suite via nextest (all layers except mutation/fuzz).
test:
    cargo nextest run --workspace --all-features

# Fast inner-loop: unit tests only (lib targets).
test-unit:
    cargo nextest run --workspace --lib

# Run the doctests. nextest (and `cargo test --all-targets`) SKIP doctests, so
# the crate-root API example that guards against public-surface drift (L2) needs
# its own explicit run — this is the load-bearing gate, not a redundant one.
doctest:
    cargo test --workspace --doc --all-features

# Integration tests (testcontainers-backed; may be slower).
test-integration:
    cargo nextest run --workspace --test '*'

# Chaos / deterministic-simulation tests (turmoil/madsim), named `chaos_*`.
# Filtered by test-name substring so it works without a custom harness.
test-chaos:
    cargo nextest run --workspace --all-features -E 'test(/chaos/)'

# Mutation testing — verifies the test suite actually catches regressions.
# Runs in-place (mutates the checked-out tree directly, reverting after each
# trial) for speed on both CI's disposable checkout and a developer's own tree.
test-mutation:
    cargo mutants --workspace --in-place

# ---------------------------------------------------------------------------
# Fuzzing & benchmarking
# ---------------------------------------------------------------------------

# Run a cargo-fuzz target for a bounded time (default 60s). cargo-fuzz needs a
# nightly toolchain (libfuzzer generates `unsafe`), so the fuzz crate is excluded
# from the workspace and driven with `+nightly` here.
# e.g. `just fuzz accept_token`, `just fuzz allowed_mask 300`.
fuzz target time="60":
    cargo +nightly fuzz run {{ target }} -- -max_total_time={{ time }}

# Compile every fuzz target without running (catches bit-rot at zero run cost) —
# the per-PR fuzz gate.
fuzz-build:
    cargo +nightly fuzz build

# Time-box every fuzz target for `time` seconds each (default 60) — the bounded
# per-PR / nightly fuzz run. Delegates the per-target loop to xtask (real control
# flow → typed Rust, not inline shell; constitution §2).
fuzz-ci time="60":
    cargo xtask fuzz-ci {{ time }}

# Criterion benchmarks. On CI these run under CodSpeed (see ci.yml).
bench:
    cargo bench --workspace

# Build and run the criterion benches under CodSpeed instrumentation — the same
# workflow CI's `bench (codspeed)` job runs (ci.yml). Needs the cargo-codspeed
# subcommand; local regression tracking uploads only when configured.
codspeed:
    cargo codspeed build --workspace
    cargo codspeed run

# Legend-backed completeness lane (opt-in; DOMAIN §8.2/§14.4). Needs docker +
# the pinned Legend stack. Delegates to xtask, which brings the stack up, runs
# the `legend`-feature tests (each health-waits the engine itself), then ALWAYS
# tears the stack down — so a failed run never leaves containers running
# (constitution §2: teardown logic belongs in xtask, not a shell trap). NOT part
# of the hermetic `just ci`; run on demand or nightly on an x86 runner.
test-legend:
    cargo xtask test-legend

# The Qwen2.5-Coder tokenizer, pinned to an immutable model revision so local and
# nightly (`qwen-oracle.yml`) runs are reproducible; keep in sync with that workflow's
# QWEN_REVISION and bump deliberately. Named here (not inlined) so the revision and
# cache path each have a single source in this file (constitution §4, no magic constants).
qwen_revision := "c03e6d358207e414f1eca0bb1891e29f1db0e242"
qwen_tokenizer := "target/qwen/tokenizer.json"

# Real-Qwen L2 soundness oracle (on-demand / local, NOT a per-PR gate — it is
# heavy: it fetches the real Qwen2.5-Coder tokenizer and replays the whole gold
# corpus token-by-token through the real byte-level BPE). This is the gold-standard
# check that L2 stays sound against the *actual* tokenizer merge boundaries — the
# class the synthetic `bpe_split_soundness` reproducer approximates. `curl -z` fetches
# the tokenizer into the gitignored `target/` cache only when it is absent or stale;
# `--fail` makes an HTTP error abort instead of caching an error page as the tokenizer.
# The lane compiles under the `qwen-oracle` feature (optional `tokenizers` dep). It also
# runs nightly / on-demand via the `qwen-oracle.yml` GitHub Actions workflow.
qwen-oracle:
    curl -sSL --fail --create-dirs -z {{ qwen_tokenizer }} -o {{ qwen_tokenizer }} "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct/resolve/{{ qwen_revision }}/tokenizer.json"
    QWEN_TOKENIZER_JSON={{ qwen_tokenizer }} cargo test --features qwen-oracle --test qwen_soundness -- --nocapture

# ---------------------------------------------------------------------------
# Coverage, supply-chain & API-stability gates
# ---------------------------------------------------------------------------

# Coverage report + floor enforcement (delegates the threshold logic to xtask).
coverage:
    cargo xtask coverage

# Advisory / vulnerability scan of the dependency tree.
audit:
    cargo audit

# License, ban, advisory and source policy enforcement.
deny:
    cargo deny check

# Unused-dependency scan.
machete:
    cargo machete

# Assert the published core stays dep-light and harness-free: its `[dependencies]`
# table holds only the allowlisted runtime deps `{ thiserror, serde, serde_json }`
# (ADR-0005 records the current membership; ADR-0003 established the harness-free
# rule) + no tests/ or corpus/ paths in `cargo package --list`.
# Delegates the parse + packaging check to xtask.
check-core-deplight:
    cargo xtask check-core-deplight

# Assert every discrete doc fact (gold counts, in-scope split, core-dep
# allowlist, the src/ module tree) matches its single source in code/corpus/FS,
# so a stale citation fails a PR instead of rotting silently (L3 anti-drift).
check-doc-facts:
    cargo xtask check-doc-facts

# Validate release-plz.toml against the workspace, so config drift fails a PR
# instead of the post-merge trunk run. Delegates to xtask.
release-plz-check:
    cargo xtask release-plz-check

# Semantic-versioning check for the public API of the libraries.
semver:
    cargo semver-checks check-release --workspace

# Verify each public crate's API against its committed baseline under
# public-api/ (needs a nightly toolchain). Run `just public-api-bless` after an
# intended API change to refresh the baselines.
public-api:
    cargo xtask public-api

# Refresh the committed public-API baselines after an intended change.
public-api-bless:
    cargo xtask public-api --bless

# ---------------------------------------------------------------------------
# Docs
# ---------------------------------------------------------------------------

# Build docs with warnings denied (missing-docs is a hard error in libs).
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# ---------------------------------------------------------------------------
# Python boundary (M4): PyO3 ffi + maturin wheel
# ---------------------------------------------------------------------------

# Type-check the feature-gated PyO3 boundary (src/ffi.rs) without building a
# wheel. `cargo xtask ci`'s `clippy --all-features` already type-checks this on
# every PR; this is the fast, targeted inner-loop gate (constitution §1: prove
# the binding compiles under `#![forbid(unsafe_code)]`).
check-ffi:
    cargo check --features python

# Build the abi3 Python wheel via maturin. One forward-compatible wheel serves
# CPython >= 3.9 (pyo3 abi3-py39). Needs `maturin` on PATH (`mise install`).
wheel:
    maturin build --release --features python

# Build the extension in-place into the active virtualenv and run the hermetic
# pytest suite over a synthetic vocabulary (no model, no engine). Needs maturin +
# a Python with pytest available.
test-python:
    maturin develop --features python
    python -m pytest python/tests

# ---------------------------------------------------------------------------
# Structural / hygiene checks
# ---------------------------------------------------------------------------

# Reject time-frozen self-description (scaffold/stub/"later milestone"/…) in
# shipped src/** doc-comments — a shipped crate must not describe itself as
# unbuilt work (constitution §5). Pure Bun regex over doc-comment lines only.
lint-stale:
    bun scripts/checks/stale-selfdescription.mjs --all

# ast-grep structural rules (banned constructs, architecture guardrails).
sweep:
    cargo xtask sweep

# Local pre-PR hygiene gate: structural rules + unused deps + secret scan.
# `review` runs the same underlying tools CI does, so it fails fast locally.
review: sweep
    cargo machete
    gitleaks detect --no-banner --redact

# ---------------------------------------------------------------------------
# Feature / spec scaffolding
# ---------------------------------------------------------------------------

# Create an isolated git worktree + branch `feature/<name>` for a change.
# One worktree per branch keeps parallel work from stepping on each other.
new-feature name:
    cargo xtask new-feature {{ name }}

# Scaffold a feature spec at specs/<name>.md from the template.
spec name:
    cargo xtask spec {{ name }}

# ---------------------------------------------------------------------------
# Aggregate / meta targets
# ---------------------------------------------------------------------------

# The one-shot local gate mirroring CI. Delegates the heavy lifting to xtask.
ci:
    cargo xtask ci

# Install git hooks (managed by lefthook.yml). Also run automatically by the
# `install-cargo-tools` onboarding step, so a fresh clone is never left unwired;
# this target is the manual re-install.
hooks-install:
    lefthook install

# NOTE: there is deliberately no `setup` target. Environment bootstrap was a
# one-time, self-deleting agent runbook; the kit is already bootstrapped.
# Re-provisioning a tool is just: `mise install && mise run install-cargo-tools`.
