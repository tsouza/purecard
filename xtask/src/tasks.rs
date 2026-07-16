//! Implementations of each `xtask` subcommand.
//!
//! Each task shells out to the underlying tool via [`crate::process`] and
//! propagates exit codes, so `xtask` stays a thin, auditable orchestrator.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::process::{run, run_cargo_steps, run_stdout};

/// Reject empty names and path-escaping input (`/`, `\`, `..`) before it's
/// interpolated into a filesystem or worktree path.
fn validate_name(name: &str, usage: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("usage: xtask {usage} <name>");
    }
    if name.contains(['/', '\\']) || name.contains("..") {
        anyhow::bail!("name must not contain path separators or '..'");
    }
    Ok(())
}

/// Full local CI pipeline, fail-fast: format check, lint (default features),
/// lint with all features, the dep-light core gate, then test.
///
/// Mirrors the ordering used in the CI workflow so a green `xtask ci` locally
/// is a strong predictor of a green pipeline. The second clippy pass runs
/// `--all-features` so feature-gated boundaries (e.g. the `legend` HTTP shim in
/// `tests/support/legend.rs`) are compiled and linted pre-merge with zero infra
/// (constitution §2), even though the live-Legend test lane itself is
/// opt-in/nightly. [`check_core_deplight`] runs before the test step so a
/// packaging regression fails fast, ahead of the slower test run.
pub fn ci() -> Result<()> {
    run_cargo_steps(&[
        &["fmt", "--all", "--check"],
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    ])?;
    check_core_deplight()?;
    check_doc_facts()?;
    run("cargo", &["test", "--workspace", "--all-targets"])?;
    // `--all-targets` (and nextest) SKIP doctests, so the crate-root API example
    // that guards against public-surface drift (L2) needs its own explicit run.
    run("cargo", &["test", "--workspace", "--doc", "--all-features"])
}

/// Run the structural / hygiene sweep: ast-grep guardrail rules over the tree.
///
/// The rules (banned constructs, architecture invariants) live under
/// `ast-grep-rules/` and are wired via `sgconfig.yml`. `ast-grep` is required
/// on PATH; a missing binary surfaces a clear error from
/// [`crate::process::run`].
pub fn sweep() -> Result<()> {
    // `sg scan` reads sgconfig.yml at the repo root and applies every rule.
    run("ast-grep", &["scan"])
}

/// Docker Compose file for the pinned Legend engine stack.
const LEGEND_COMPOSE_FILE: &str = "corpus/legend-stack/docker-compose.yml";

/// Run the opt-in Legend completeness lane with guaranteed teardown.
///
/// Brings the pinned Legend stack up, runs the `legend`-feature tests, then
/// **always** tears the stack down — even when the tests fail — so a red run
/// never leaves containers running. Encoding up→test→teardown here (rather than
/// as a shell `trap` in the recipe) keeps the justfile free of non-trivial shell
/// (constitution §2). The test result is propagated after teardown so a failure
/// still reddens the lane.
///
/// # Errors
///
/// Returns the test failure if the tests fail; otherwise a teardown failure if
/// `docker compose down` fails. If *both* fail, the test failure is returned
/// with the teardown failure attached as context — a leftover-container failure
/// is never silently dropped, since surfacing it is the whole point.
pub fn test_legend() -> Result<()> {
    run(
        "docker",
        &["compose", "-f", LEGEND_COMPOSE_FILE, "up", "-d"],
    )?;
    // Capture, do NOT `?`-return: teardown must run regardless of the outcome.
    let tested = run("cargo", &["nextest", "run", "--features", "legend"]);
    let torn_down = run("docker", &["compose", "-f", LEGEND_COMPOSE_FILE, "down"]);
    match (tested, torn_down) {
        (Err(test_err), Err(teardown_err)) => Err(test_err.context(format!(
            "legend tests failed AND stack teardown failed (containers may be left running): {teardown_err:#}"
        ))),
        (Err(test_err), Ok(())) => Err(test_err),
        (Ok(()), teardown) => teardown,
    }
}

/// The cargo-fuzz targets under `fuzz/fuzz_targets/`, kept in sync with that
/// directory. A named list rather than a directory scan: the loop must run every
/// target, and a new target is added here in the same change that adds the file.
const FUZZ_TARGETS: &[&str] = &["accept_token", "allowed_mask", "schema_from_json"];

/// Time-box every cargo-fuzz target for `secs` seconds each.
///
/// A loop over the target list (real control flow, so it lives in xtask, not the
/// justfile — constitution §2). cargo-fuzz needs a nightly toolchain, invoked via
/// `cargo +nightly fuzz run`; the fuzz crate is excluded from the workspace so the
/// core stays stable-pinned and `forbid(unsafe)`-clean (ADR-0006). A crash in any
/// target fails the whole task (fail-fast).
///
/// # Errors
///
/// Returns the first target's failure (a crash, or a missing nightly / cargo-fuzz).
pub fn fuzz_ci(secs: u64) -> Result<()> {
    let budget = format!("-max_total_time={secs}");
    for target in FUZZ_TARGETS {
        run("cargo", &["+nightly", "fuzz", "run", target, "--", &budget])?;
    }
    Ok(())
}

/// The minimum acceptable line-coverage percentage. Enforced as a hard floor so
/// coverage can only ratchet upward. Tighten with human sign-off; never loosen.
const COVERAGE_FLOOR_PCT: &str = "70";

/// Path regex excluded from coverage measurement: `xtask` is the build/CI
/// orchestrator, not shipped product code, so it is not held to the product
/// coverage floor. This scopes what is measured; it does not lower the floor.
const COVERAGE_IGNORE_REGEX: &str = "xtask/";

/// Produce a coverage report using `cargo-llvm-cov` and enforce a floor.
///
/// `--fail-under-lines` makes the command exit non-zero if line coverage drops
/// below [`COVERAGE_FLOOR_PCT`], so `xtask coverage` doubles as a CI gate. With
/// `html`, also emits a browsable HTML report under `target/llvm-cov`.
pub fn coverage(html: bool) -> Result<()> {
    let mut args = vec![
        "llvm-cov",
        "--workspace",
        "--ignore-filename-regex",
        COVERAGE_IGNORE_REGEX,
        "--fail-under-lines",
        COVERAGE_FLOOR_PCT,
    ];
    if html {
        args.push("--html");
    } else {
        args.push("--summary-only");
    }
    run("cargo", &args)
}

/// Path to the release-plz configuration, relative to the workspace root.
const RELEASE_PLZ_CONFIG: &str = "release-plz.toml";

/// Validate `release-plz.toml` against the real workspace membership.
///
/// release-plz only runs on push to `main` (post-merge), so a config whose
/// `[[package]]` override names a crate that isn't a workspace member — the
/// exact drift that reddened the trunk once already — cannot be caught before
/// merge. release-plz rejects such an override at runtime ("overrides are not
/// present in the workspace"); this reproduces that check offline so it fails a
/// PR instead of the trunk.
///
/// Running release-plz's own CLI as the gate is unfit here: `update` needs a
/// branch upstream and git history that a PR's detached-HEAD checkout lacks, so
/// it fails for reasons unrelated to config. Comparing the config's overrides
/// against `cargo metadata` is deterministic, needs no network or git state,
/// and targets precisely the class of bug that broke the trunk.
pub fn release_plz_check() -> Result<()> {
    let src = std::fs::read_to_string(RELEASE_PLZ_CONFIG)
        .with_context(|| format!("reading {RELEASE_PLZ_CONFIG}"))?;
    let overrides = release_plz_override_names(&src);
    let members = workspace_member_names()?;

    let missing = missing_overrides(&overrides, &members);
    if !missing.is_empty() {
        anyhow::bail!(
            "{RELEASE_PLZ_CONFIG} has [[package]] overrides not present in the workspace: {}. \
             Remove them or fix the name — an override for a non-member crate reddens every \
             push to main.",
            missing.join(", ")
        );
    }
    Ok(())
}

/// Extract the `name` of every `[[package]]` table in a release-plz config.
///
/// A hand scan rather than a TOML dependency: the only key we need is the
/// override name, and the array-of-tables shape release-plz uses is trivial to
/// walk line by line.
fn release_plz_override_names(toml_src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_package = false;
    for line in toml_src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[[package]]";
            continue;
        }
        if !in_package {
            continue;
        }
        // Take only the first quoted token, so a trailing inline comment
        // (`name = "domain" # note`) doesn't leak into the parsed name.
        if let Some(rest) = trimmed.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
            && let Some(name) = value
                .trim()
                .strip_prefix('"')
                .and_then(|v| v.split('"').next())
        {
            names.push(name.to_string());
        }
    }
    names
}

/// Names of every workspace-member package, via `cargo metadata`. With
/// `--no-deps` the reported packages are exactly the workspace members (the set
/// release-plz resolves overrides against), so excluded crates like `lints` are
/// correctly absent.
fn workspace_member_names() -> Result<Vec<String>> {
    let json = run_stdout("cargo", &["metadata", "--no-deps", "--format-version", "1"])?;
    let meta: serde_json::Value =
        serde_json::from_str(&json).context("parsing `cargo metadata` output")?;
    let packages = meta["packages"]
        .as_array()
        .context("`cargo metadata` has no packages array")?;
    Ok(packages
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_string))
        .collect())
}

/// Override names absent from the workspace-member set.
fn missing_overrides(overrides: &[String], members: &[String]) -> Vec<String> {
    overrides
        .iter()
        .filter(|name| !members.iter().any(|member| member == *name))
        .cloned()
        .collect()
}

/// Path to the published core crate's manifest, relative to the workspace root.
const CORE_MANIFEST: &str = "Cargo.toml";

/// Header of the `[dependencies]` table whose emptiness the dep-light gate
/// enforces (distinct from `[dev-dependencies]` / `[workspace.dependencies]`).
const CORE_DEPS_TABLE: &str = "[dependencies]";

/// Header prefix of a per-dependency sub-table (`[dependencies.serde]`). This is
/// TOML's other spelling for a core dependency — a `[dependencies.<name>]` table
/// declares `<name>` just as a `<name> = …` line in `[dependencies]` does — so
/// the gate must treat it as a runtime dependency, not skip past it.
const CORE_DEPS_SUBTABLE_PREFIX: &str = "[dependencies.";

/// Packaged-path prefixes the published crate must never ship: the oracle
/// harness (`tests/`) and the gold corpus (`corpus/`) are dev-only.
const NON_CORE_PACKAGE_PREFIXES: &[&str] = &["tests/", "corpus/"];

/// The core's runtime-dependency allowlist: the *only* crates permitted in the
/// published `purecard` crate's `[dependencies]` table. `thiserror` is the
/// decoder's library error-type crate (constitution §1; `DecodeError` in
/// `src/error.rs`). `serde` + `serde_json` are the **M3 widening**; the current
/// allowlist `{ thiserror, serde, serde_json }` is recorded authoritatively in
/// ADR-0005. L2 ingests the host `Schema` as JSON at session init
/// (`Schema::from_json`, `docs/spec/schema.md` §6.3, §9), so its parser is shipped
/// host-facing code — a bespoke JSON parser would fail "library before writing"
/// (constitution §4).
/// Every other dependency must be a `[dev-dependency]`. This list is a PROTECTED
/// gate: it may only be widened by a human, with the justification recorded (as
/// here); it never silently disables the check — a dep outside this set still
/// fails the gate.
const CORE_DEP_ALLOWLIST: &[&str] = &["thiserror", "serde", "serde_json"];

/// Fix-the-system gate: assert the published `purecard` core stays dep-light and
/// ships no oracle-harness code (ADR-0003).
///
/// Two invariants, both machine-checked so "src/ is core only" is enforced, not
/// merely documented:
///
/// 1. the crate's `[dependencies]` table holds only allowlisted runtime deps
///    ([`CORE_DEP_ALLOWLIST`] — currently `thiserror`, `serde`, and `serde_json`,
///    the M3 widening per ADR-0005); every remaining harness dependency
///    (`anyhow`, `ureq`, `proptest`, `criterion`) stays a `[dev-dependency]`, so
///    it never enters a downstream consumer's resolution graph;
/// 2. `cargo package --list` names no file under `tests/` or `corpus/`, so a
///    change to the `include` list cannot smuggle the harness into the tarball.
///
/// # Errors
///
/// Returns an error if the manifest cannot be read, the `[dependencies]` table
/// holds a dependency outside [`CORE_DEP_ALLOWLIST`], or `cargo package --list`
/// lists a non-core path.
pub fn check_core_deplight() -> Result<()> {
    let manifest = std::fs::read_to_string(CORE_MANIFEST)
        .with_context(|| format!("reading {CORE_MANIFEST}"))?;
    let deps = core_dependency_entries(&manifest);
    let disallowed = disallowed_core_deps(&deps, CORE_DEP_ALLOWLIST);
    if !disallowed.is_empty() {
        anyhow::bail!(
            "the published `purecard` core's `[dependencies]` table may hold only the \
             allowlisted runtime deps {{ {} }} (ADR-0005), but found: {}. Move harness \
             deps to `[dev-dependencies]`.",
            CORE_DEP_ALLOWLIST.join(", "),
            disallowed.join(", ")
        );
    }

    // `--allow-dirty` so the gate runs in the local pre-commit `just ci` flow (an
    // uncommitted tree) exactly as it does on CI's clean checkout; `--list` only
    // reports the file set, it packages nothing.
    let listed = run_stdout(
        "cargo",
        &["package", "--list", "--allow-dirty", "-p", "purecard"],
    )?;
    let leaked: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|path| {
            NON_CORE_PACKAGE_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
        .collect();
    if !leaked.is_empty() {
        anyhow::bail!(
            "the published `purecard` package must ship no oracle-harness or corpus \
             files, but `cargo package --list` includes: {}. Check the `include` list \
             in {CORE_MANIFEST}.",
            leaked.join(", ")
        );
    }
    Ok(())
}

/// Names of every dependency declared in the crate's `[dependencies]` table.
///
/// A hand scan in the style of [`release_plz_override_names`] rather than a TOML
/// dependency: the check only needs to know whether that one table's body holds
/// any entry, so it walks lines from the table header to the next `[table]`,
/// skipping comments and blanks. It counts both TOML spellings of a core
/// dependency: an inline entry under `[dependencies]` (`name = …` /
/// `name.workspace = …`, the token before the first `=` or `.`) **and** a
/// per-dependency sub-table header `[dependencies.<name>]` — the latter declares
/// `<name>` too, so skipping it would let a runtime dep slip past the gate.
fn core_dependency_entries(toml_src: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    // Parallel to `entries`: whether each dependency carries `optional = true`. An
    // optional dependency compiles only when a feature turns it on (e.g. the
    // `python`-gated pyo3/self_cell), so it is absent from the default build and
    // from `cargo package`'s compiled surface — the dep-light gate must not count
    // it. Tracked alongside the name because `optional = true` can sit on the
    // entry's own inline line OR on a later line of its `[dependencies.<name>]`
    // sub-table body.
    let mut optional: Vec<bool> = Vec::new();
    let mut in_deps = false;
    // When inside a `[dependencies.<alias>]` sub-table body, this is the index in
    // `entries` of the alias name, so a `package = "…"`/`optional = true` line
    // inside the body can rewrite / flag it.
    let mut subtable_entry: Option<usize> = None;
    for raw_line in toml_src.lines() {
        // Strip any trailing comment *before* parsing: a `#` outside a quoted string
        // begins a TOML comment, which Cargo ignores. Parsing it as live TOML lets a
        // comment (`serde = { … } # package = "thiserror"`) spoof a `package =`
        // rename — or a `# optional = true` fake-skip a real dep — past the
        // allowlist (constitution §7).
        let trimmed = strip_toml_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            subtable_entry = None;
            // A `[dependencies.<name>]` header is itself a dependency
            // declaration; record `<name>` and leave `in_deps` false so the
            // sub-table's own fields (`version`, `features`, …) aren't miscounted.
            if let Some(name) = subtable_dependency_name(trimmed) {
                subtable_entry = Some(entries.len());
                entries.push(name);
                optional.push(false);
            }
            in_deps = trimmed == CORE_DEPS_TABLE;
            continue;
        }
        // Inside a `[dependencies.<alias>]` body, a `package = "real"` line renames
        // the dependency (check `real`, not the alias key) and an `optional = true`
        // line marks it feature-gated.
        if let Some(idx) = subtable_entry {
            if let Some(real) = package_override(trimmed) {
                entries[idx] = real;
            }
            if toml_flag_is_true(trimmed, "optional") {
                optional[idx] = true;
            }
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some(name) = trimmed.split(['=', '.']).next() {
            let name = name.trim();
            if !name.is_empty() {
                // An inline `alias = { package = "real", … }` renames the entry to
                // its real crate; without the override the key IS the crate name.
                let real = package_override(trimmed).unwrap_or_else(|| name.to_string());
                entries.push(real);
                optional.push(toml_flag_is_true(trimmed, "optional"));
            }
        }
    }
    // Only non-optional runtime deps face the allowlist gate.
    entries
        .into_iter()
        .zip(optional)
        .filter_map(|(name, is_optional)| (!is_optional).then_some(name))
        .collect()
}

/// The crate name from a Cargo `package = "<name>"` rename, if `line` carries one.
///
/// The check resolves a dependency's *real* package — the `[[bin]]`-style alias
/// trick `alias = { package = "serde" }` (or the sub-table `package = "serde"`
/// field) declares crate `serde` under the key `alias`, so scanning the key alone
/// would let an unallowlisted crate hide behind an allowlisted alias. `package` is
/// matched only as a whole key (`=`-delimited, not a substring of `packages`),
/// then its single- or double-quoted string value is returned.
fn package_override(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = line[from..].find("package") {
        let start = from + rel;
        let end = start + "package".len();
        let key_start = start == 0 || !is_key_byte(bytes[start - 1]);
        let after = line[end..].trim_start();
        if key_start && let Some(value) = after.strip_prefix('=') {
            let value = value.trim_start();
            let quote = match value.as_bytes().first() {
                Some(&b'"') => '"',
                Some(&b'\'') => '\'',
                _ => return None,
            };
            let body = &value[1..];
            let close = body.find(quote)?;
            let name = body[..close].trim();
            return (!name.is_empty()).then(|| name.to_owned());
        }
        from = end;
    }
    None
}

/// The portion of a TOML line before its comment: everything up to the first `#`
/// that is not inside a quoted string. A crate name can hold neither `#` nor a
/// quote, so a simple quote-tracking scan (no escape handling) is exact here, and
/// keeps a trailing comment from being parsed as a live `package =` rename.
fn strip_toml_comment(line: &str) -> &str {
    let mut quote: Option<u8> = None;
    for (idx, &byte) in line.as_bytes().iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'#' => return &line[..idx],
            None => {}
        }
    }
    line
}

/// Whether `byte` may appear in a bare TOML key, so `package` inside a longer key
/// (`packages`, `my_package`) is not mistaken for the `package` rename key.
const fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Whether `line` sets the bare TOML boolean key `flag` to `true` (e.g.
/// `optional = true`, inline or on its own body line).
///
/// `flag` is matched only as a whole key — the same `is_key_byte` boundary check
/// [`package_override`] uses — so a longer key (`optionally`) or a substring
/// inside a quoted value never counts, and the value must be the literal `true`
/// (`optional = false` reads as not-a-flag). Comment stripping happens in the
/// caller, so a `# optional = true` cannot fake-skip a real dependency.
fn toml_flag_is_true(line: &str, flag: &str) -> bool {
    // Match `flag = true` only as a bare key OUTSIDE any quoted string. Comments
    // are stripped first, and quotes are tracked as in `strip_toml_comment`, so a
    // crafted value like `features = ["optional = true"]` cannot spoof the gate
    // (anti-gaming, constitution §7).
    let line = strip_toml_comment(line);
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let byte = bytes[idx];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if line[idx..].starts_with(flag) => {
                let end = idx + flag.len();
                let key_start = idx == 0 || !is_key_byte(bytes[idx - 1]);
                let key_end = end >= bytes.len() || !is_key_byte(bytes[end]);
                if key_start
                    && key_end
                    && let Some(value) = line[end..].trim_start().strip_prefix('=')
                {
                    let token: String = value
                        .trim_start()
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect();
                    if token == "true" {
                        return true;
                    }
                }
            }
            None => {}
        }
        idx += 1;
    }
    false
}

/// Core `[dependencies]` entries not on `allowlist` — the deps that must fail the
/// dep-light gate. An empty result means the table stays within the allowlist.
fn disallowed_core_deps(entries: &[String], allowlist: &[&str]) -> Vec<String> {
    entries
        .iter()
        .filter(|dep| !allowlist.contains(&dep.as_str()))
        .cloned()
        .collect()
}

/// Extract `<name>` from a `[dependencies.<name>]` sub-table header, or `None`
/// for any other table header. The name is the segment between the prefix and
/// the next `.` or the closing `]`.
fn subtable_dependency_name(header: &str) -> Option<String> {
    let rest = header.strip_prefix(CORE_DEPS_SUBTABLE_PREFIX)?;
    let name = rest.split(['.', ']']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Public library crates whose API surface is snapshotted.
const PUBLIC_API_CRATES: &[&str] = &["purecard"];

/// Directory holding the committed public-API baseline snapshots.
const PUBLIC_API_DIR: &str = "public-api";

/// Snapshot each public crate's API with `cargo public-api` (which needs a
/// nightly toolchain for rustdoc JSON) and, unless `bless` is set, fail if it
/// drifts from the committed baseline under [`PUBLIC_API_DIR`].
///
/// # Errors
///
/// Returns an error if a snapshot cannot be produced or written, or (when not
/// blessing) if the regenerated surface differs from the committed baseline.
pub fn public_api(bless: bool) -> Result<()> {
    if bless {
        std::fs::create_dir_all(PUBLIC_API_DIR)
            .with_context(|| format!("creating {PUBLIC_API_DIR}/"))?;
    }

    let mut drift = Vec::new();
    for krate in PUBLIC_API_CRATES {
        let surface = run_stdout("cargo", &["+nightly", "public-api", "-p", krate])?;
        let path = format!("{PUBLIC_API_DIR}/{krate}.txt");

        if bless {
            std::fs::write(&path, surface).with_context(|| format!("writing {path}"))?;
        } else {
            // Check-only: compare against the committed baseline in memory, never
            // touching the working tree.
            let baseline = std::fs::read_to_string(&path).with_context(|| {
                format!("reading baseline {path} (run `just public-api-bless`)")
            })?;
            if baseline != surface {
                drift.push(krate.to_string());
            }
        }
    }

    if !drift.is_empty() {
        anyhow::bail!(
            "public API drifted for: {}. Review and run `just public-api-bless` if intended.",
            drift.join(", ")
        );
    }
    Ok(())
}

/// Create an isolated git worktree + branch `feature/<name>` for a change.
///
/// One worktree per branch keeps parallel work from stepping on each other.
///
/// # Errors
///
/// Returns an error if `name` is empty or the underlying `git` commands fail.
pub fn new_feature(name: &str) -> Result<()> {
    validate_name(name, "new-feature")?;
    let branch = format!("feature/{name}");
    let repo_dir = std::env::current_dir().context("reading current directory")?;
    let repo_name = repo_dir
        .file_name()
        .context("current directory has no name")?
        .to_string_lossy();
    let worktree = format!("../{repo_name}-{name}");

    // Best-effort: an offline `fetch` shouldn't block creating the worktree.
    let _ = run("git", &["fetch", "--quiet", "origin"]);
    run("git", &["worktree", "add", "-b", &branch, &worktree])?;

    println!("Created worktree at {worktree} on branch {branch}");
    println!("  cd \"{worktree}\" && just spec {name}");
    Ok(())
}

/// Template for a new `specs/<name>.md` file. `{name}` and `{date}` are
/// substituted by [`spec`].
const SPEC_TEMPLATE: &str = "\
# Spec: {name}

- Status: draft
- Created: {date}
- Owner:

## Problem
What user-visible problem does this solve? Why now?

## Goals
- [ ]

## Non-goals
-

## Design
How it works and which modules it touches. For decoder work, tie each
production/rule back to the gold corpus that motivates it (oracle-driven).

## API / contract impact
Rust public-API and/or PyO3-boundary changes (if any) and their stability impact.

## Testing plan
Unit / integration / chaos / mutation / fuzz coverage for this change.

## Risks & rollout
Failure modes, feature-flagging, and how we roll back.
";

/// Scaffold a feature spec at `specs/<name>.md` from [`SPEC_TEMPLATE`].
///
/// # Errors
///
/// Returns an error if `name` is empty, a spec already exists at that path, or
/// the file cannot be written.
pub fn spec(name: &str) -> Result<()> {
    validate_name(name, "spec")?;
    let out = format!("specs/{name}.md");
    if std::path::Path::new(&out).exists() {
        anyhow::bail!("spec already exists: {out}");
    }
    std::fs::create_dir_all("specs").context("creating specs/")?;

    let contents = render_spec(name, &today_utc_ymd());
    std::fs::write(&out, contents).with_context(|| format!("writing {out}"))?;
    println!("Wrote {out}");
    Ok(())
}

/// Render [`SPEC_TEMPLATE`] with `name` and `date` substituted.
fn render_spec(name: &str, date: &str) -> String {
    SPEC_TEMPLATE
        .replace("{name}", name)
        .replace("{date}", date)
}

/// Seconds in a day.
const SECS_PER_DAY: u64 = 86_400;
/// Days in one common year.
const DAYS_PER_YEAR: i64 = 365;
/// Years in a 400-year proleptic-Gregorian era — the leap cycle the algorithm
/// folds on.
const YEARS_PER_ERA: i64 = 400;
/// Days in a 400-year era (its `YEARS_PER_ERA` years plus 97 leap days).
const DAYS_PER_ERA: i64 = 146_097;
/// Days in a 4-year cycle — a leap correction in the year-of-era formula.
const DAYS_PER_4_YEARS: i64 = 1_460;
/// Days in a 100-year cycle — a leap correction in the year-of-era formula.
const DAYS_PER_100_YEARS: i64 = 36_524;
/// Days from the algorithm's shifted epoch (0000-03-01) to the Unix epoch.
const EPOCH_SHIFT_DAYS: i64 = 719_468;

/// Today's UTC date as `YYYY-MM-DD`, computed in-process — no shell-out to the
/// platform `date` binary (absent/inconsistent across OSes; constitution §2
/// "portable automation").
fn today_utc_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day) = civil_from_days((secs / SECS_PER_DAY) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert days since the Unix epoch to a proleptic-Gregorian `(year, month,
/// day)`. Howard Hinnant's exact, dependency-free algorithm, documented at
/// <https://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + EPOCH_SHIFT_DAYS;
    let era = (if z >= 0 { z } else { z - (DAYS_PER_ERA - 1) }) / DAYS_PER_ERA;
    let doe = z - era * DAYS_PER_ERA;
    let yoe = (doe - doe / DAYS_PER_4_YEARS + doe / DAYS_PER_100_YEARS - doe / (DAYS_PER_ERA - 1))
        / DAYS_PER_YEAR;
    let year = yoe + era * YEARS_PER_ERA;
    let doy = doe - (DAYS_PER_YEAR * yoe + yoe / 4 - yoe / 100);
    // Hinnant's month-from-day-of-year fit; 5/2/153/3/9 are the algorithm's
    // polynomial coefficients, meaningful only within it.
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ---------------------------------------------------------------------------
// Doc-fact assertions (L3): every discrete fact a doc cites is checked against
// its ONE authoritative source — a test const, the gold corpus, the src/ tree,
// or `CORE_DEP_ALLOWLIST`. The source is *read*, never re-declared here, so there
// is exactly one value per fact. Portable/in-process (constitution §2): plain
// std::fs + string scanning in the style of the dep-light gate, no shell-out.
// ---------------------------------------------------------------------------

/// The scanned docs — `README.md` and the `docs/` tree (which carries the
/// architecture module tree) — checked against their single sources of truth: the
/// corpus/in-scope counts (test consts + the corpus itself), the `src/` module
/// layout, and [`CORE_DEP_ALLOWLIST`].
const DOC_README: &str = "README.md";
const DOC_DIR: &str = "docs";
const SOUNDNESS_REPLAY_SRC: &str = "tests/soundness_replay.rs";
const SELFCHECK_CORPUS_SRC: &str = "tests/selfcheck_corpus.rs";
const L2_SOUNDNESS_SRC: &str = "tests/l2_soundness.rs";
const L2_PROPERTIES_SRC: &str = "tests/l2_properties.rs";
const GOLD_CORPUS: &str = "corpus/gold_queries.jsonl";
const ARCHITECTURE_DOC: &str = "docs/spec/architecture.md";
/// The heading that precedes the fenced `src/` module tree in the architecture
/// doc; the gate reads the tree from the first code fence after it.
const MODULE_TREE_HEADING: &str = "### 3.2 Crate layout";
/// The crate root file, shown as the tree's root node rather than a leaf module,
/// so it is excluded from the module-name comparison.
const CRATE_ROOT_STEM: &str = "lib";

/// Assert every discrete doc fact agrees with its single source (see module note
/// above). Collects *all* violations before failing, so one run reports the full
/// drift set rather than the first mismatch.
///
/// # Errors
///
/// Returns an error if any source cannot be read, the module tree cannot be
/// located, or any cited fact contradicts its source.
pub fn check_doc_facts() -> Result<()> {
    let mut errors: Vec<String> = Vec::new();

    // Fact 1 — gold corpus count. SoT: the test consts AND the corpus record
    // count, which must agree with each other.
    let arm_a = read_usize_const(SOUNDNESS_REPLAY_SRC, "ARM_A")?;
    let arm_c = read_usize_const(SOUNDNESS_REPLAY_SRC, "ARM_C")?;
    let gold = read_usize_const(SELFCHECK_CORPUS_SRC, "EXPECTED_GOLD_RECORDS")?;
    if arm_a + arm_c != gold {
        errors.push(format!(
            "gold-count consts disagree: {SOUNDNESS_REPLAY_SRC} ARM_A+ARM_C = {} but \
             {SELFCHECK_CORPUS_SRC} EXPECTED_GOLD_RECORDS = {gold}",
            arm_a + arm_c
        ));
    }
    let corpus = count_corpus_records(GOLD_CORPUS)?;
    if corpus != gold {
        errors.push(format!(
            "{GOLD_CORPUS} holds {corpus} records but EXPECTED_GOLD_RECORDS = {gold}"
        ));
    }

    // Fact 2 — in-scope split. SoT: l2_soundness consts, cross-checked against
    // the duplicated `IN_SCOPE_TOTAL` in l2_properties.
    let in_a = read_usize_const(L2_SOUNDNESS_SRC, "IN_SCOPE_ARM_A")?;
    let in_c = read_usize_const(L2_SOUNDNESS_SRC, "IN_SCOPE_ARM_C")?;
    let in_total = read_usize_const(L2_SOUNDNESS_SRC, "IN_SCOPE_TOTAL")?;
    if in_a + in_c != in_total {
        errors.push(format!(
            "in-scope consts disagree in {L2_SOUNDNESS_SRC}: {in_a} + {in_c} != {in_total}"
        ));
    }
    let in_total_props = read_usize_const(L2_PROPERTIES_SRC, "IN_SCOPE_TOTAL")?;
    if in_total_props != in_total {
        errors.push(format!(
            "IN_SCOPE_TOTAL drifted: {L2_SOUNDNESS_SRC} = {in_total}, \
             {L2_PROPERTIES_SRC} = {in_total_props}"
        ));
    }

    // Fact 3 — module tree in architecture.md §3.2 vs the real src/ layout. The
    // highest-value fact: a ghost module (an `engine.rs`/`picard_pure` that never
    // shipped) or an omitted one fails here.
    let arch = std::fs::read_to_string(ARCHITECTURE_DOC)
        .with_context(|| format!("reading {ARCHITECTURE_DOC}"))?;
    let doc_modules = module_names_in_tree(&arch)
        .with_context(|| format!("locating the module tree in {ARCHITECTURE_DOC} §3.2"))?;
    let src_modules = src_module_names()?;
    let ghosts: Vec<String> = doc_modules.difference(&src_modules).cloned().collect();
    let missing: Vec<String> = src_modules.difference(&doc_modules).cloned().collect();
    if !ghosts.is_empty() {
        errors.push(format!(
            "{ARCHITECTURE_DOC} §3.2 lists modules absent from src/: {}",
            ghosts.join(", ")
        ));
    }
    if !missing.is_empty() {
        errors.push(format!(
            "{ARCHITECTURE_DOC} §3.2 omits src/ modules: {}",
            missing.join(", ")
        ));
    }

    // Doc set for the text scans below.
    let docs = collect_docs()?;

    // Fact 4 — core-dep allowlist. SoT: `CORE_DEP_ALLOWLIST`. Any doc enumeration
    // that claims the *widened* set (a `{ … }` naming both `thiserror` and
    // `serde`) must equal it exactly; the historical single-crate `{ thiserror }`
    // is exempt. Scanned over the doc set only — Rust source is dense with braces
    // (format strings, structs, closures) that no line-scanner can tell from a
    // crate set, and the enumeration that matters lives in the ADRs.
    let allow: BTreeSet<String> = CORE_DEP_ALLOWLIST
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for (path, text) in &docs {
        for set in allowlist_sets(text) {
            let got: BTreeSet<String> = set.iter().cloned().collect();
            if got != allow {
                errors.push(format!(
                    "{path} states a core-dep allowlist {{ {} }} that contradicts \
                     CORE_DEP_ALLOWLIST {{ {} }}",
                    set.join(", "),
                    CORE_DEP_ALLOWLIST.join(", ")
                ));
            }
        }
    }

    // Fact 1b (targeted, unambiguous) — the `gold stays <N>/<N>` soundness ratio
    // always names the whole corpus, so both sides must equal the gold total.
    // Bare arm/partition counts are NOT gated: the docs cite the gold split
    // (4639/395), the in-scope split (256/13), the ~4-query smoke set, and
    // historical partition sizes (395, 1791) all in the same "<N> query/-query"
    // form, so a number-adjacency rule can't isolate the gold total without false
    // positives — that fact is anchored by the const/corpus checks above.
    for (path, text) in &docs {
        for n in gold_ratio_citations(text) {
            if n != gold {
                errors.push(format!(
                    "{path} cites a gold ratio {n}/…; the gold total is {gold}"
                ));
            }
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "doc-fact drift — each cited fact must match its single source:\n{}",
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(())
}

/// The value of a `const <name>: usize = <int>;` declaration in `src`.
fn read_usize_const(src: &str, name: &str) -> Result<usize> {
    let content = std::fs::read_to_string(src).with_context(|| format!("reading {src}"))?;
    parse_usize_const(&content, name)
        .with_context(|| format!("no integer `const {name}: usize` in {src}"))
}

/// Parse the integer literal of a `const <name>: usize = <int>` line, tolerating
/// `_` digit separators. Returns `None` if the const is absent or its value is
/// not an integer literal (e.g. `ARM_A + ARM_C`), which the caller derives.
fn parse_usize_const(content: &str, name: &str) -> Option<usize> {
    for line in content.lines() {
        let Some(rest) = line.trim_start().strip_prefix("const ") else {
            continue;
        };
        let Some(after_name) = rest.trim_start().strip_prefix(name) else {
            continue;
        };
        // The char right after the name must end the identifier (`:` or space),
        // so `ARM_A` does not match `ARM_ABC`.
        let after = after_name.trim_start();
        if !after.starts_with(':') {
            continue;
        }
        let Some(eq) = after.find('=') else { continue };
        let digits: String = after[eq + 1..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '_')
            .filter(|c| *c != '_')
            .collect();
        if let Ok(value) = digits.parse::<usize>() {
            return Some(value);
        }
    }
    None
}

/// Count the records in the gold corpus: non-empty lines. `str::lines` yields the
/// final record whether or not the file ends in a newline, so the count is exact
/// without a `wc`-style shell-out (constitution §2, portable).
fn count_corpus_records(path: &str) -> Result<usize> {
    let content = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    Ok(content.lines().filter(|l| !l.trim().is_empty()).count())
}

/// The set of `.rs` module basenames named in the fenced tree under §3.2.
fn module_names_in_tree(arch_src: &str) -> Result<BTreeSet<String>> {
    let heading = arch_src
        .find(MODULE_TREE_HEADING)
        .context("module-tree heading not found")?;
    let after_heading = &arch_src[heading..];
    let fence_open = after_heading
        .find("```")
        .context("no code fence after the heading")?;
    let body = &after_heading[fence_open + 3..];
    let fence_close = body.find("```").context("unterminated code fence")?;
    let tree = &body[..fence_close];
    Ok(tree.lines().flat_map(rs_stems_in_line).collect())
}

/// The `.rs` file stems named on one tree line (e.g. `compiled.rs …` → `compiled`).
fn rs_stems_in_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut stems = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(".rs") {
        let dot = search_from + rel;
        let start = line[..dot]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let stem = &line[start..dot];
        // The char after `.rs` must not continue an identifier (so `.rsx` is not
        // an `.rs` file), and the stem must be a real name.
        let after = bytes.get(dot + 3).copied();
        let boundary = after.is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_');
        if !stem.is_empty() && boundary {
            stems.push(stem.to_string());
        }
        search_from = dot + 3;
    }
    stems
}

/// The `.rs` module basenames under `src/`, excluding the crate root `lib.rs`.
fn src_module_names() -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let mut stack = vec![PathBuf::from("src")];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && stem != CRATE_ROOT_STEM
            {
                names.insert(stem.to_string());
            }
        }
    }
    Ok(names)
}

/// The README plus every `*.md` under `docs/`, as `(path, contents)` pairs.
fn collect_docs() -> Result<Vec<(String, String)>> {
    let mut docs = Vec::new();
    let readme =
        std::fs::read_to_string(DOC_README).with_context(|| format!("reading {DOC_README}"))?;
    docs.push((DOC_README.to_string(), readme));

    let mut stack = vec![PathBuf::from(DOC_DIR)];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                docs.push((path.to_string_lossy().into_owned(), text));
            }
        }
    }
    Ok(docs)
}

/// English stopwords that can appear inside a `{ … }` alongside crate names in
/// prose; excluded so a prose brace is not mistaken for a crate-set enumeration.
const BRACE_STOPWORDS: &[&str] = &["and", "the", "for", "not", "but", "with", "plus"];

/// Upper bound on the inside of a `{ … }` treated as a crate-set enumeration; a
/// genuine allowlist is a handful of names, so anything longer is prose.
const MAX_ALLOWLIST_BRACE_LEN: usize = 80;

/// The crate-set enumerations in `text`: the lowercase token set inside each
/// `{ … }` that names `thiserror` together with `serde` (the widened-allowlist
/// shape). A brace naming only `thiserror` is historical and skipped.
fn allowlist_sets(text: &str) -> Vec<Vec<String>> {
    let mut sets = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find('{') {
        let open = from + rel;
        // An unmatched `{` (no `}` in the rest of the doc) is a stray prose brace,
        // not a set: step past just it and keep scanning, so a genuine set later in
        // the same doc is still evaluated — never abandon the rest of the file.
        let Some(rel_close) = text[open + 1..].find('}') else {
            from = open + 1;
            continue;
        };
        let inner = &text[open + 1..open + 1 + rel_close];
        // A crate set is short; a longer span is prose that merely happens to
        // pair `{` with a distant `}`. Resume just past this `{` — not past the
        // far `}` — so a genuine `{ … }` nested inside that prose is still reached
        // instead of being swallowed with the whole span.
        if inner.len() >= MAX_ALLOWLIST_BRACE_LEN {
            from = open + 1;
            continue;
        }
        from = open + 1 + rel_close + 1;
        if !(inner.contains("thiserror") && inner.contains("serde")) {
            continue;
        }
        let tokens: Vec<String> = inner
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|t| {
                t.len() >= 3
                    && t.chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            })
            .filter(|t| !BRACE_STOPWORDS.contains(t))
            .map(str::to_string)
            .collect();
        if !tokens.is_empty() {
            sets.push(tokens);
        }
    }
    sets
}

/// Every `<N>/<N>` ratio appearing on a line that mentions "gold" — the
/// `gold stays 5034/5034` soundness form, unambiguously the gold total.
///
/// Scans by `char`, not by byte index: a `gold`-mentioning line may also carry a
/// multibyte char (a `§` section reference such as `§5/G2`) right beside a digit
/// run, and byte-index arithmetic (`rfind(..).map(|i| i + 1)`) would land inside
/// that char and panic. A ratio is a maximal digit/comma run, a `/`, then another
/// digit/comma run; anything else (a `§5/G2` cross-reference, a `corpus/…` path)
/// yields no digits on one side and is ignored.
fn gold_ratio_citations(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.to_ascii_lowercase().contains("gold") {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let is_run = |c: char| c.is_ascii_digit() || c == ',';
        for slash in 0..chars.len() {
            if chars[slash] != '/' {
                continue;
            }
            let mut left = slash;
            while left > 0 && is_run(chars[left - 1]) {
                left -= 1;
            }
            let mut right = slash + 1;
            while right < chars.len() && is_run(chars[right]) {
                right += 1;
            }
            // Both sides must carry at least one digit/comma char, else this `/`
            // is a path or cross-reference, not a ratio.
            if left == slash || right == slash + 1 {
                continue;
            }
            let l: String = chars[left..slash].iter().collect();
            let r: String = chars[slash + 1..right].iter().collect();
            if let (Some(l), Some(r)) = (parse_grouped(&l), parse_grouped(&r)) {
                out.push(l);
                out.push(r);
            }
        }
    }
    out
}

/// Parse a possibly comma-grouped integer (`5,034` or `5034`), or `None`.
fn parse_grouped(token: &str) -> Option<usize> {
    let digits: String = token.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usize_const_reads_a_literal_and_tolerates_separators() {
        assert_eq!(
            parse_usize_const("const ARM_A: usize = 4639;", "ARM_A"),
            Some(4639)
        );
        assert_eq!(
            parse_usize_const("const N: usize = 1_024;", "N"),
            Some(1024)
        );
    }

    #[test]
    fn parse_usize_const_ignores_a_prefix_name_and_a_derived_value() {
        // `ARM_A` must not match `ARM_ABC`, and a derived (non-literal) value has
        // no integer to read — the caller derives it from its parts instead.
        assert_eq!(
            parse_usize_const("const ARM_ABC: usize = 7;", "ARM_A"),
            None
        );
        assert_eq!(
            parse_usize_const(
                "const EXPECTED_GOLD_RECORDS: usize = ARM_A + ARM_C;",
                "EXPECTED_GOLD_RECORDS"
            ),
            None
        );
    }

    #[test]
    fn rs_stems_in_line_extracts_module_basenames() {
        assert_eq!(
            rs_stems_in_line("    compiled.rs     CompiledGrammar"),
            ["compiled"]
        );
        assert_eq!(
            rs_stems_in_line("  grammar/        L1 automaton"),
            Vec::<String>::new()
        );
        // A `.rs` that continues into an identifier is not a module file.
        assert_eq!(rs_stems_in_line("see foo.rsx here"), Vec::<String>::new());
    }

    #[test]
    fn module_names_in_tree_reads_only_the_fenced_tree_after_the_heading() {
        let arch = "\
intro\n\n### 3.2 Crate layout\n\n```\npurecard/\n  vocab.rs   the vocab\n  session.rs the session\n```\n\nProse mentioning a ghost engine.rs must be ignored.\n";
        let got = module_names_in_tree(arch).expect("tree parses");
        let want: BTreeSet<String> = ["vocab".to_string(), "session".to_string()]
            .into_iter()
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn allowlist_sets_flags_the_widened_form_and_exempts_the_historical_one() {
        // The historical single-crate brace is exempt (no `serde`).
        assert!(allowlist_sets("M1 widened it to `{ thiserror }`.").is_empty());
        // A prose brace with an English stopword and other words is not a set.
        assert_eq!(
            allowlist_sets("the widened `{ thiserror, serde, serde_json }` set")[0],
            ["thiserror", "serde", "serde_json"]
        );
        // A long prose span that merely pairs braces is not an enumeration.
        let long = format!("{{ thiserror {} serde }}", "x".repeat(100));
        assert!(allowlist_sets(&long).is_empty());
    }

    #[test]
    fn allowlist_sets_keeps_scanning_past_a_brace_that_cannot_form_a_set() {
        // Regression: a stray `{` whose only closing `}` is the genuine set's own
        // spans an over-long prose window; a *trailing* `{` past the set has no
        // close at all. The old control flow jumped past the far `}` (swallowing
        // the nested set) and then `break`ed on the trailing `{` (abandoning the
        // file) — so a genuine `{ thiserror, serde, tokio }` drift went unchecked.
        // The scan must step past each such brace and keep going, still surfacing
        // the later mismatch.
        let filler = "prose ".repeat(20); // pushes the stray span over the prose bound
        let text = format!(
            "a stray {{ {filler}then `{{ thiserror, serde, tokio }}` widens it, trailing {{"
        );
        assert_eq!(
            allowlist_sets(&text),
            vec![vec![
                "thiserror".to_string(),
                "serde".to_string(),
                "tokio".to_string(),
            ]]
        );
    }

    #[test]
    fn gold_ratio_citations_reads_only_ratios_on_gold_lines() {
        assert_eq!(
            gold_ratio_citations("the is_accepting change keeps gold at 5034/5034."),
            [5034, 5034]
        );
        // A slash between non-numbers, or on a non-gold line, is ignored.
        assert!(gold_ratio_citations("see src/grammar for the gold path").is_empty());
        assert!(gold_ratio_citations("the ratio 12/12 on a plain line").is_empty());
    }

    #[test]
    fn gold_ratio_citations_survives_multibyte_section_refs() {
        // A `gold`-mentioning line carrying a `§N/…` section cross-reference (a
        // multibyte `§` immediately before a digit-run/slash) must not panic on a
        // char boundary, and the `§5/G2` is not a ratio (its right side is `G2`,
        // no digits before it) — regression for the byte-index slicing bug.
        assert!(
            gold_ratio_citations(
                "not in the gold corpus; oracle'd by the seed corpus (gap report §5/G2)."
            )
            .is_empty()
        );
        // A real ratio on the same kind of line is still read.
        assert_eq!(
            gold_ratio_citations("§8 gold soundness stays 5034/5034 (see §5.8)"),
            [5034, 5034]
        );
    }

    #[test]
    fn gold_ratio_citations_handles_comments_whitespace_and_quotes() {
        // Inline `#` (or `//`) comment prose that mentions gold and a ratio is
        // scanned like any other line — the parser keys on the word "gold", not on
        // being outside a comment.
        assert_eq!(
            gold_ratio_citations("assert_eq!(n, 5034); // gold stays 5034/5034"),
            [5034, 5034]
        );
        assert_eq!(
            gold_ratio_citations("# gold soundness note: 5,034/5,034 replayed"),
            [5034, 5034]
        );
        // Quotes around the ratio do not block it: a quote is neither a digit nor a
        // comma, so it simply bounds the digit runs.
        assert_eq!(
            gold_ratio_citations(r#"the gold ratio is "5034/5034" today"#),
            [5034, 5034]
        );
        // Whitespace around the slash breaks the ratio: a ratio is a *maximal*
        // digit/comma run, a `/`, then another — `5034 / 5034` has an empty run on
        // each side of the slash, so it is intentionally NOT read (the canonical
        // citation form is the un-spaced `N/N`; the const/corpus checks are the
        // real gate, this is a best-effort prose anchor).
        assert!(gold_ratio_citations("gold stays 5034 / 5034").is_empty());
        assert!(gold_ratio_citations("gold 5034/ 5034").is_empty());
        assert!(gold_ratio_citations("gold 5034 /5034").is_empty());
    }

    #[test]
    fn parse_grouped_strips_separators() {
        assert_eq!(parse_grouped("5,034"), Some(5034));
        assert_eq!(parse_grouped("395"), Some(395));
        assert_eq!(parse_grouped("nope"), None);
    }

    #[test]
    fn civil_from_days_matches_known_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1)); // Unix epoch
        assert_eq!(civil_from_days(10_957), (2000, 1, 1)); // 30 years + 7 leap days
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // exercises the leap day
        assert_eq!(civil_from_days(-1), (1969, 12, 31)); // day before the epoch
    }

    #[test]
    fn today_utc_ymd_is_well_formed() {
        let today = today_utc_ymd();
        assert_eq!(today.len(), 10);
        assert_eq!(today.matches('-').count(), 2);
        assert!(today.starts_with("20"));
    }

    #[test]
    fn render_spec_substitutes_name_and_date() {
        let out = render_spec("widget", "2026-07-05");
        assert!(out.contains("# Spec: widget"));
        assert!(out.contains("Created: 2026-07-05"));
    }

    #[test]
    fn validate_name_accepts_plain_names() {
        assert!(validate_name("widget", "spec").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("", "spec").is_err());
    }

    #[test]
    fn validate_name_rejects_path_escapes() {
        assert!(validate_name("../../etc/passwd", "spec").is_err());
        assert!(validate_name("foo/bar", "spec").is_err());
        assert!(validate_name("foo\\bar", "spec").is_err());
        assert!(validate_name("..", "spec").is_err());
    }

    #[test]
    fn release_plz_override_names_extracts_package_names() {
        let src = "\
[workspace]
changelog_update = true

[changelog]
header = \"x\"

[[package]]
name = \"domain\"
publish = false

[[package]]
name=\"xtask\"
release = false
";
        assert_eq!(release_plz_override_names(src), ["domain", "xtask"]);
    }

    #[test]
    fn release_plz_override_names_ignores_non_package_name_keys() {
        // A `name = ` under a non-`[[package]]` table must not be collected.
        let src = "[workspace]\nname = \"not-a-package\"\n";
        assert!(release_plz_override_names(src).is_empty());
    }

    #[test]
    fn release_plz_override_names_strips_trailing_inline_comment() {
        // A trailing comment must not leak into the parsed name, or a valid
        // config would falsely trip the drift gate.
        let src = "[[package]]\nname = \"domain\" # keep in sync\n";
        assert_eq!(release_plz_override_names(src), ["domain"]);
    }

    #[test]
    fn missing_overrides_flags_non_members() {
        let overrides = ["domain".to_string(), "lints".to_string()];
        let members = ["domain".to_string(), "xtask".to_string()];
        assert_eq!(missing_overrides(&overrides, &members), ["lints"]);
    }

    #[test]
    fn missing_overrides_empty_when_all_present() {
        let overrides = ["domain".to_string(), "xtask".to_string()];
        let members = ["domain".to_string(), "xtask".to_string()];
        assert!(missing_overrides(&overrides, &members).is_empty());
    }

    #[test]
    fn core_dependency_entries_reads_both_key_forms() {
        // `name = …` and `name.workspace = …` both yield the bare dependency
        // name; the scan stops at the next table so `[dev-dependencies]` is out.
        let src = "\
[package]
name = \"purecard\"

[dependencies]
serde = { version = \"1\", features = [\"derive\"] }
anyhow.workspace = true

[dev-dependencies]
ureq = \"3\"
";
        assert_eq!(core_dependency_entries(src), ["serde", "anyhow"]);
    }

    #[test]
    fn core_dependency_entries_is_empty_for_a_comment_only_table() {
        // A comment/blank-only `[dependencies]` body is the dep-light core state
        // the gate demands — it must parse to no entries.
        let src = "\
[dependencies]
# empty by design — the core is dependency-free.

[dev-dependencies]
serde = \"1\"
";
        assert!(core_dependency_entries(src).is_empty());
    }

    #[test]
    fn core_dependency_entries_detects_subtable_form() {
        // `[dependencies.serde]` is a valid TOML spelling of a core dependency;
        // the gate must catch it, not walk past the header. Its own fields
        // (`version`, `features`) are the sub-table's body, not new deps.
        let src = "\
[package]
name = \"purecard\"

[dependencies.serde]
version = \"1\"
features = [\"derive\"]

[dev-dependencies]
ureq = \"3\"
";
        assert_eq!(core_dependency_entries(src), ["serde"]);
    }

    #[test]
    fn a_package_alias_is_resolved_to_the_real_crate() {
        // The gate must check a dependency's REAL package, not the key it hides
        // behind: `thiserror = { package = "tokio" }` is `tokio` (not on the
        // allowlist), and pointing an allowlisted `thiserror` key at `tokio` must
        // NOT sneak it past. Both TOML spellings of the rename are covered.
        let inline = "\
[dependencies]
thiserror = { package = \"tokio\", version = \"1\" }
";
        assert_eq!(core_dependency_entries(inline), ["tokio"]);
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(inline), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );

        let subtable = "\
[dependencies.thiserror]
package = \"tokio\"
version = \"1\"
";
        assert_eq!(core_dependency_entries(subtable), ["tokio"]);
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(subtable), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );
    }

    #[test]
    fn the_m3_serde_widening_is_allowlisted() {
        // serde + serde_json are the M3 addition to the core allowlist (L2 JSON
        // ingress); together with thiserror they must all pass the gate, while an
        // unrelated runtime dep still fails it.
        let src = "\
[dependencies]
thiserror = \"2\"
serde = { version = \"1\", features = [\"derive\"] }
serde_json = \"1\"
tokio = \"1\"
";
        assert_eq!(
            core_dependency_entries(src),
            ["thiserror", "serde", "serde_json", "tokio"]
        );
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(src), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );
    }

    #[test]
    fn an_optional_dependency_is_excluded_from_the_core_gate() {
        // pyo3/self_cell (the M4 PyO3 boundary) land only behind the `python`
        // feature (`optional = true`), so they are absent from the default build
        // and `cargo package`'s compiled surface — the dep-light gate must not
        // count them. Both TOML spellings, inline and sub-table.
        let inline = "\
[dependencies]
thiserror = \"2\"
pyo3 = { version = \"0.29.0\", optional = true }
self_cell = { version = \"1.2.2\", optional = true }
";
        assert_eq!(core_dependency_entries(inline), ["thiserror"]);
        assert!(
            disallowed_core_deps(&core_dependency_entries(inline), CORE_DEP_ALLOWLIST).is_empty()
        );

        let subtable = "\
[dependencies]
thiserror = \"2\"

[dependencies.pyo3]
version = \"0.29.0\"
optional = true
";
        assert_eq!(core_dependency_entries(subtable), ["thiserror"]);
        assert!(
            disallowed_core_deps(&core_dependency_entries(subtable), CORE_DEP_ALLOWLIST).is_empty()
        );
    }

    #[test]
    fn a_non_optional_non_allowlisted_dependency_still_fails_the_gate() {
        // The optional-skip must not become a blanket bypass: a dependency that is
        // NOT optional (or is `optional = false`) and off the allowlist still
        // reddens the gate. `optional` matched only as a whole key.
        let explicit_false = "\
[dependencies]
tokio = { version = \"1\", optional = false }
";
        assert_eq!(core_dependency_entries(explicit_false), ["tokio"]);
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(explicit_false), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );

        // A longer key containing `optional` is not the flag.
        assert!(!toml_flag_is_true("optionally = true", "optional"));
        // A quoted string cannot spoof the flag: `optional = true` inside a value
        // (e.g. a features array) must not skip the dep past the allowlist (§7).
        assert!(!toml_flag_is_true(
            "tokio = { version = \"1\", features = [\"optional = true\"] }",
            "optional"
        ));
        assert!(!toml_flag_is_true("name = \"optional = true\"", "optional"));
        // A genuine bare key still reads true.
        assert!(toml_flag_is_true(
            "pyo3 = { version = \"0.29\", optional = true }",
            "optional"
        ));
        // A commented flag cannot skip a dep: the caller strips comments first, so
        // the live line the gate sees has no `optional = true`.
        let commented = "\
[dependencies.tokio]
version = \"1\"
# optional = true
";
        assert_eq!(core_dependency_entries(commented), ["tokio"]);
    }

    #[test]
    fn a_commented_out_package_rename_cannot_spoof_the_allowlist() {
        // A trailing comment is not live TOML: `serde = { … } # package = "thiserror"`
        // must resolve to the real crate `serde` (disallowed), never to the
        // commented-out `thiserror` alias — otherwise a comment bypasses the gate
        // (constitution §7, anti-gaming). Both key forms are covered.
        let inline = "\
[dependencies]
tokio = { version = \"1\" } # package = \"thiserror\"
";
        assert_eq!(core_dependency_entries(inline), ["tokio"]);
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(inline), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );

        // A comment-only line inside a sub-table body must not be read as a rename.
        let subtable = "\
[dependencies.tokio]
version = \"1\"
# package = \"thiserror\"
";
        assert_eq!(core_dependency_entries(subtable), ["tokio"]);
        assert_eq!(
            disallowed_core_deps(&core_dependency_entries(subtable), CORE_DEP_ALLOWLIST),
            ["tokio"]
        );

        // A `#` *inside* the quoted package value is not a comment delimiter.
        assert_eq!(
            package_override("package = \"ser#de\"").as_deref(),
            Some("ser#de")
        );
    }

    #[test]
    fn a_package_substring_key_is_not_a_rename() {
        // A key that merely contains `package` (or a value that does) is not a
        // `package = "…"` rename: the real crate stays the declared key.
        assert!(package_override("packages = [\"a\"]").is_none());
        assert!(package_override("my_package = \"1\"").is_none());
        assert_eq!(
            core_dependency_entries("[dependencies]\nserde = { version = \"1\" }\n"),
            ["serde"]
        );
    }

    #[test]
    fn disallowed_core_deps_admits_the_allowlist_and_flags_the_rest() {
        // The M3 allowlist is `{ thiserror, serde, serde_json }`; an unrelated
        // runtime dep (`tokio`) is not on it, so only `tokio` is reported. This
        // pins the gate to permit the intended core dep set exactly, not "any
        // dep" and not "no dep".
        let entries = [
            "thiserror".to_string(),
            "serde".to_string(),
            "serde_json".to_string(),
            "tokio".to_string(),
        ];
        assert_eq!(
            disallowed_core_deps(&entries, CORE_DEP_ALLOWLIST),
            ["tokio"]
        );
    }

    #[test]
    fn disallowed_core_deps_is_empty_for_an_allowlisted_only_table() {
        // The exact intended M1 state — `[dependencies]` holds only `thiserror` —
        // must pass the gate (empty disallowed set).
        let entries = ["thiserror".to_string()];
        assert!(disallowed_core_deps(&entries, CORE_DEP_ALLOWLIST).is_empty());
    }

    #[test]
    fn disallowed_core_deps_is_empty_for_an_empty_table() {
        // A dep-free `[dependencies]` is trivially within the allowlist.
        assert!(disallowed_core_deps(&[], CORE_DEP_ALLOWLIST).is_empty());
    }

    #[test]
    fn core_dependency_entries_ignores_other_dependency_tables() {
        // Only the bare `[dependencies]` table is the core surface; a
        // `[dev-dependencies]` or `[workspace.dependencies]` entry must not count.
        let src = "[dev-dependencies]\nserde = \"1\"\n\n[workspace.dependencies]\nanyhow = \"1\"\n";
        assert!(core_dependency_entries(src).is_empty());
    }

    /// The fuzz-target names from the `target: [ … ]` build-matrix line of a
    /// `fuzz.yml` workflow. A single-line scan (the matrix is written inline), used
    /// only by the drift gate below.
    fn fuzz_matrix_targets(workflow_src: &str) -> Vec<String> {
        workflow_src
            .lines()
            .find_map(|line| {
                let inner = line
                    .trim()
                    .strip_prefix("target:")?
                    .trim()
                    .strip_prefix('[')?
                    .strip_suffix(']')?;
                Some(
                    inner
                        .split(',')
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    #[test]
    fn fuzz_targets_stay_in_sync_across_the_list_dir_and_workflow() {
        // FUZZ_TARGETS is hand-mirrored in three places: this const, the
        // `fuzz/fuzz_targets/*.rs` files, and the fuzz.yml build matrix. Deriving one
        // from another across a Rust const, a directory, and a YAML file isn't clean,
        // so this test makes the three agree — closing the drift class (constitution
        // §5) the moment a target is added or removed in only one of them.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent workspace root");

        let mut declared: Vec<String> = FUZZ_TARGETS.iter().map(|t| (*t).to_string()).collect();
        declared.sort();

        let mut on_disk: Vec<String> = std::fs::read_dir(root.join("fuzz/fuzz_targets"))
            .expect("reading fuzz/fuzz_targets/")
            .map(|entry| entry.expect("a dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .map(|path| {
                path.file_stem()
                    .expect("a .rs file has a stem")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        on_disk.sort();
        assert_eq!(
            declared, on_disk,
            "FUZZ_TARGETS must list exactly the *.rs files under fuzz/fuzz_targets/"
        );

        let workflow = std::fs::read_to_string(root.join(".github/workflows/fuzz.yml"))
            .expect("reading .github/workflows/fuzz.yml");
        let mut in_matrix = fuzz_matrix_targets(&workflow);
        in_matrix.sort();
        assert_eq!(
            declared, in_matrix,
            "the fuzz.yml build matrix must list exactly FUZZ_TARGETS"
        );
    }
}
