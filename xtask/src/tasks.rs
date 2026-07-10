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
/// lint with all features, then test.
///
/// Mirrors the ordering used in the CI workflow so a green `xtask ci` locally
/// is a strong predictor of a green pipeline. The second clippy pass runs
/// `--all-features` so feature-gated boundaries (e.g. the `engine` HTTP shim)
/// are compiled and linted pre-merge with zero infra (constitution §2), even
/// though the live-engine test lane itself is opt-in/nightly.
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
        &["test", "--workspace", "--all-targets"],
    ])
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
        if let Some(rest) = trimmed.strip_prefix("name") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                // Take only the first quoted token, so a trailing inline comment
                // (`name = "domain" # note`) doesn't leak into the parsed name.
                if let Some(name) = value
                    .trim()
                    .strip_prefix('"')
                    .and_then(|v| v.split('"').next())
                {
                    names.push(name.to_string());
                }
            }
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
}
