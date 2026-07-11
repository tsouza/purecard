//! Implementations of each `xtask` subcommand.
//!
//! Each task shells out to the underlying tool via [`crate::process`] and
//! propagates exit codes, so `xtask` stays a thin, auditable orchestrator.

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
    run("cargo", &["test", "--workspace", "--all-targets"])
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
/// `src/error.rs`, ADR-0004). `serde` + `serde_json` are the **M3 widening**: L2
/// ingests the host `Schema` as JSON at session init (`Schema::from_json`,
/// `docs/spec/schema.md` §6.3, §9), so its parser is shipped host-facing code —
/// a bespoke JSON parser would fail "library before writing" (constitution §4).
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
///    ([`CORE_DEP_ALLOWLIST`] — currently just `thiserror`); every harness
///    dependency (`serde`, `serde_json`, `anyhow`, `ureq`) stays a
///    `[dev-dependency]`, so it never enters a downstream consumer's resolution
///    graph;
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
             allowlisted runtime deps {{ {} }} (ADR-0004), but found: {}. Move harness \
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
    let mut entries = Vec::new();
    let mut in_deps = false;
    // When inside a `[dependencies.<alias>]` sub-table body, this is the index in
    // `entries` of the alias name, so a `package = "…"` override line inside the
    // body can rewrite it to the real crate name the allowlist must be checked
    // against.
    let mut subtable_entry: Option<usize> = None;
    for raw_line in toml_src.lines() {
        // Strip any trailing comment *before* parsing: a `#` outside a quoted string
        // begins a TOML comment, which Cargo ignores. Parsing it as live TOML lets a
        // comment (`serde = { … } # package = "thiserror"`) spoof a `package =`
        // rename and slip a disallowed crate past the allowlist (constitution §7).
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
            }
            in_deps = trimmed == CORE_DEPS_TABLE;
            continue;
        }
        // Inside a `[dependencies.<alias>]` body, a `package = "real"` line renames
        // the dependency: the gate must check `real`, not the alias key.
        if let Some(idx) = subtable_entry {
            if let Some(real) = package_override(trimmed) {
                entries[idx] = real;
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
            }
        }
    }
    entries
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
