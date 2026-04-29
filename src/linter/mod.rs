//! Local linter discovery.
//!
//! Runs `commands.lint`, parses the output according to `commands.lint_format`,
//! and normalizes findings into SonarQube-shaped [`sonar::Issue`] records so
//! the existing fix loop can process them uniformly.
//!
//! A linter finding's synthetic rule key looks like `lint:<format>:<rule>`
//! (e.g. `lint:clippy:unused_imports`). Consumers can detect linter-derived
//! issues with `issue.rule.starts_with("lint:")`.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use crate::runner;
use crate::sonar::{Issue, TextRange};

mod checkstyle;
mod clippy;
mod eslint;
mod ruff;

/// Supported linter output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintFormat {
    /// `cargo clippy --message-format=json` — cargo-style JSON stream.
    Clippy,
    /// `eslint -f json` — array of file results.
    Eslint,
    /// `ruff check --output-format json` — array of diagnostics.
    Ruff,
    /// Checkstyle XML (from `checkstyle -f xml` or
    /// `mvn checkstyle:checkstyle` → `target/checkstyle-result.xml`).
    Checkstyle,
}

impl LintFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "clippy" => Some(LintFormat::Clippy),
            "eslint" => Some(LintFormat::Eslint),
            "ruff" => Some(LintFormat::Ruff),
            "checkstyle" => Some(LintFormat::Checkstyle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LintFormat::Clippy => "clippy",
            LintFormat::Eslint => "eslint",
            LintFormat::Ruff => "ruff",
            LintFormat::Checkstyle => "checkstyle",
        }
    }
}

/// Sniff the format from the configured lint command when `lint_format` is
/// `auto` or absent. Conservative: only recognizes patterns we can reliably parse.
pub fn detect_lint_format(command: &str) -> Option<LintFormat> {
    let c = command.to_lowercase();
    if c.contains("clippy") {
        Some(LintFormat::Clippy)
    } else if c.contains("eslint") {
        Some(LintFormat::Eslint)
    } else if c.contains("ruff") {
        Some(LintFormat::Ruff)
    } else if c.contains("checkstyle") {
        Some(LintFormat::Checkstyle)
    } else {
        None
    }
}

/// A single linter finding after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub rule: String,
    pub severity: String,
    pub message: String,
}

impl LintFinding {
    /// Convert into a synthetic [`Issue`] that flows through the existing
    /// fix loop. `project_component` is the SonarQube project component
    /// (typically `config.sonar_project_id`) used to build `component`
    /// field as `<project>:<file>` — matching SonarQube's convention so
    /// `sonar::component_to_path()` recovers the path cleanly.
    pub fn into_issue(self, format: LintFormat, project_component: &str) -> Issue {
        let sev = normalize_severity(&self.severity);
        let component = if project_component.is_empty() {
            self.file.clone()
        } else {
            format!("{}:{}", project_component, self.file)
        };
        // Synthetic key: must be stable & unique. Include file+line+rule so
        // the same finding across runs gets the same key (idempotent branch
        // names) and two findings on different lines never collide.
        let key = format!(
            "lint:{}:{}:{}:{}",
            format.name(),
            self.rule,
            sanitize_key(&self.file),
            self.start_line
        );
        let rule = format!("lint:{}:{}", format.name(), self.rule);
        Issue {
            key,
            rule,
            severity: sev,
            component,
            issue_type: "CODE_SMELL".to_string(),
            message: self.message,
            text_range: Some(TextRange {
                start_line: self.start_line,
                end_line: self.end_line.max(self.start_line),
                start_offset: None,
                end_offset: None,
            }),
            status: "OPEN".to_string(),
            tags: vec![format.name().to_string(), "linter".to_string()],
            effort: None,
        }
    }
}

fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

/// Canonical severity buckets used by SonarQube. Anything unrecognized
/// becomes `MAJOR` (neutral default).
fn normalize_severity(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "blocker" | "fatal" => "BLOCKER".to_string(),
        "critical" | "error" => "CRITICAL".to_string(),
        "major" | "warn" | "warning" => "MAJOR".to_string(),
        "minor" | "info" | "note" => "MINOR".to_string(),
        "trivial" | "style" | "hint" => "INFO".to_string(),
        _ => "MAJOR".to_string(),
    }
}

/// Run the linter and parse its findings.
///
/// Returns an empty vec (not an error) when no lint command is configured —
/// the orchestrator decides what to do with "no-op phase".
///
/// `max_findings` caps the returned vec (0 = no cap). Severity-preserving:
/// findings are ranked BLOCKER → CRITICAL → MAJOR → MINOR → INFO before capping.
pub fn run_lint_scan(
    project_path: &Path,
    command: Option<&str>,
    format_hint: Option<&str>,
    autofix: bool,
    max_findings: u32,
    project_component: &str,
) -> Result<Vec<Issue>> {
    let Some(cmd) = command else {
        info!("Linter phase: no `commands.lint` configured — skipping.");
        return Ok(Vec::new());
    };

    let format = match format_hint.and_then(|f| {
        if f.eq_ignore_ascii_case("auto") {
            None
        } else {
            LintFormat::parse(f)
        }
    }) {
        Some(f) => f,
        None => match detect_lint_format(cmd) {
            Some(f) => {
                info!("Linter phase: auto-detected format `{}` from command", f.name());
                f
            }
            None => {
                warn!(
                    "Linter phase: cannot determine lint format from `{}`. \
                     Set `commands.lint_format` explicitly (clippy / eslint / ruff) \
                     or pass --skip-linter-scan. Skipping phase.",
                    cmd
                );
                return Ok(Vec::new());
            }
        },
    };

    if autofix {
        if let Some(fix_cmd) = autofix_command_for(format, cmd) {
            info!("Linter phase: running autofix `{}`", fix_cmd);
            match runner::run_shell_command(project_path, &fix_cmd, "linter autofix") {
                Ok((true, _)) => info!("Linter autofix completed"),
                Ok((false, out)) => warn!(
                    "Linter autofix returned non-zero (continuing with scan): {}",
                    truncate(&out, 200)
                ),
                Err(e) => warn!("Linter autofix error (continuing with scan): {}", e),
            }
        } else {
            warn!(
                "Linter phase: --linter-autofix requested but no autofix invocation \
                 is known for format `{}`. Skipping autofix.",
                format.name()
            );
        }
    }

    info!("Linter phase: running `{}`", cmd);
    // Linters that find issues typically exit with a non-zero status. We
    // always parse the captured stdout+stderr regardless of the status code;
    // a genuine crash manifests as unparseable output, which yields zero
    // findings and a warning below.
    let (_ok, output) = runner::run_shell_command(project_path, cmd, "linter scan")
        .map_err(|e| anyhow::anyhow!("Failed to execute linter command: {}", e))?;

    let findings = match format {
        LintFormat::Clippy => clippy::parse(&output),
        LintFormat::Eslint => eslint::parse(&output),
        LintFormat::Ruff => ruff::parse(&output),
        LintFormat::Checkstyle => {
            // The Maven plugin writes findings to target/checkstyle-result.xml
            // and only prints a summary to stdout. If stdout doesn't contain
            // the `<checkstyle>` root, fall back to reading the report file(s).
            if output.contains("<checkstyle") {
                checkstyle::parse(&output)
            } else {
                parse_checkstyle_reports(project_path)
            }
        }
    };

    let mut findings = match findings {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "Linter phase: could not parse `{}` output as {} ({}). \
                 No findings queued. Sample output:\n{}",
                cmd,
                format.name(),
                e,
                truncate(&output, 400)
            );
            return Ok(Vec::new());
        }
    };

    info!(
        "Linter phase: {} parsed {} finding(s)",
        format.name(),
        findings.len()
    );

    // Severity-rank and cap.
    findings.sort_by_key(|f| severity_rank(&normalize_severity(&f.severity)));
    if max_findings > 0 && findings.len() as u32 > max_findings {
        warn!(
            "Linter phase: capping findings at {} (from {}). Raise --max-linter-findings to include more.",
            max_findings,
            findings.len()
        );
        findings.truncate(max_findings as usize);
    }

    Ok(findings
        .into_iter()
        .map(|f| f.into_issue(format, project_component))
        .collect())
}

/// Aggregate findings from every `target/checkstyle-result.xml` under the
/// project — covers multi-module Maven layouts where each module writes its
/// own report. Paths inside the XML are absolute; we rewrite them to be
/// project-relative so the downstream fix loop's `component_to_path` works.
fn parse_checkstyle_reports(project_path: &Path) -> Result<Vec<LintFinding>> {
    let pattern = format!("{}/**/target/checkstyle-result.xml", project_path.display());
    let mut all: Vec<LintFinding> = Vec::new();
    let reports = match glob::glob(&pattern) {
        Ok(r) => r,
        Err(e) => {
            warn!("Checkstyle glob error ({}): {}", pattern, e);
            return Ok(Vec::new());
        }
    };
    let mut found_any = false;
    for entry in reports {
        let Ok(path) = entry else { continue };
        found_any = true;
        info!("Linter phase: reading checkstyle report {}", path.display());
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        match checkstyle::parse(&content) {
            Ok(mut findings) => {
                for f in &mut findings {
                    f.file = relativize(&f.file, project_path);
                }
                all.extend(findings);
            }
            Err(e) => warn!("Could not parse {}: {}", path.display(), e),
        }
    }
    if !found_any {
        warn!(
            "Linter phase: no target/checkstyle-result.xml found under {} — \
             is the checkstyle-maven-plugin configured to run?",
            project_path.display()
        );
    }
    Ok(all)
}

/// Make an absolute path project-relative; leave unchanged if already relative
/// or outside the project root.
fn relativize(file: &str, project_path: &Path) -> String {
    let p = Path::new(file);
    if let Ok(rel) = p.strip_prefix(project_path) {
        return rel.to_string_lossy().to_string();
    }
    // Try parent-walk: in multi-module builds the checkstyle report is written
    // by the module's target/, but the file path can still be anchored at the
    // repo root. Strip any leading absolute-path segment.
    file.to_string()
}

/// Lower number = more severe. Matches SonarQube ordering.
fn severity_rank(sev: &str) -> u8 {
    match sev {
        "BLOCKER" => 0,
        "CRITICAL" => 1,
        "MAJOR" => 2,
        "MINOR" => 3,
        "INFO" => 4,
        _ => 5,
    }
}

/// Run a single-file, single-rule autofix for a `lint:<format>:<rule>` issue.
///
/// Returns `Ok(true)` if the linter reports success AND at least one of the
/// target file's bytes changed. Returns `Ok(false)` when the linter has no
/// opinion on that rule for that file (or made no change). Returns `Err`
/// for invocation failures the caller should propagate.
///
/// This is the per-issue fast-path that lets us skip the Claude call for
/// the large class of linter findings the linter itself can fix in ~1s.
pub fn autofix_single(format: LintFormat, rule: &str, file: &Path, project_path: &Path) -> Result<bool> {
    // Snapshot file bytes so we can detect whether the linter actually edited it.
    let before = std::fs::read(file).unwrap_or_default();

    let rel = file
        .strip_prefix(project_path)
        .unwrap_or(file)
        .to_string_lossy()
        .to_string();

    let cmd = match format {
        LintFormat::Clippy => {
            // Clippy can only fix at crate granularity, not per-file. We accept
            // that: it's still ~2-5s versus a ~30-60s Claude call, and any
            // collateral fixes for the same rule in the crate are a free win.
            format!("cargo clippy --fix --allow-dirty --allow-staged -- -W clippy::{}", rule)
        }
        LintFormat::Eslint => {
            format!("eslint --fix --rule '{}: error' {}", rule, shell_escape(&rel))
        }
        LintFormat::Ruff => {
            format!("ruff check --fix --select {} {}", rule, shell_escape(&rel))
        }
        LintFormat::Checkstyle => {
            // Checkstyle is a reporter — it has no autofix mode. Signal
            // "no opinion" so the caller falls back to the AI engine.
            return Ok(false);
        }
    };

    info!("Linter fast-path ({}): {}", format.name(), cmd);
    let (ok, output) = runner::run_shell_command(project_path, &cmd, "linter-fastpath")?;
    if !ok {
        // Non-zero is NOT fatal — eslint/ruff return non-zero when findings
        // remain after --fix. Inspect the file mutation as ground truth.
        let after = std::fs::read(file).unwrap_or_default();
        if before == after {
            warn!(
                "Linter fast-path ({}) exited non-zero and made no change: {}",
                format.name(),
                truncate(&output, 200)
            );
            return Ok(false);
        }
    }
    let after = std::fs::read(file).unwrap_or_default();
    Ok(before != after)
}

fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Build the autofix invocation for the given format, deriving flags from
/// the base scan command where possible.
fn autofix_command_for(format: LintFormat, base: &str) -> Option<String> {
    match format {
        LintFormat::Clippy => {
            // Assume the user's command is `cargo clippy …`. Injecting `--fix
            // --allow-dirty --allow-staged` is safe since we already required
            // a clean (or staged-WIP) tree at startup.
            if base.to_lowercase().contains("clippy") {
                Some(format!("{} --fix --allow-dirty --allow-staged", base))
            } else {
                None
            }
        }
        LintFormat::Eslint => {
            if base.to_lowercase().contains("eslint") {
                Some(format!("{} --fix", base))
            } else {
                None
            }
        }
        LintFormat::Ruff => {
            if base.to_lowercase().contains("ruff") {
                Some(format!("{} --fix", base))
            } else {
                None
            }
        }
        LintFormat::Checkstyle => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let trunc: String = s.chars().take(max).collect();
        format!("{}…", trunc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_format_from_command() {
        assert_eq!(detect_lint_format("cargo clippy -- -D warnings"), Some(LintFormat::Clippy));
        assert_eq!(detect_lint_format("npx eslint . -f json"), Some(LintFormat::Eslint));
        assert_eq!(detect_lint_format("ruff check --output-format json ."), Some(LintFormat::Ruff));
        assert_eq!(detect_lint_format("pylint src/"), None);
    }

    #[test]
    fn parse_format_from_string() {
        assert_eq!(LintFormat::parse("clippy"), Some(LintFormat::Clippy));
        assert_eq!(LintFormat::parse("ESLint"), Some(LintFormat::Eslint));
        assert_eq!(LintFormat::parse("bogus"), None);
    }

    #[test]
    fn finding_to_issue_produces_stable_key() {
        let f = LintFinding {
            file: "src/main.rs".to_string(),
            start_line: 10,
            end_line: 12,
            rule: "unused_imports".to_string(),
            severity: "warning".to_string(),
            message: "unused import: `Bar`".to_string(),
        };
        let issue = f.clone().into_issue(LintFormat::Clippy, "my-project");
        assert_eq!(issue.rule, "lint:clippy:unused_imports");
        assert_eq!(issue.severity, "MAJOR");
        assert_eq!(issue.issue_type, "CODE_SMELL");
        assert_eq!(issue.component, "my-project:src/main.rs");
        assert!(issue.key.contains("unused_imports"));
        assert!(issue.key.contains("10"));
        let tr = issue.text_range.unwrap();
        assert_eq!(tr.start_line, 10);
        assert_eq!(tr.end_line, 12);

        // Re-running on the same finding yields the same key (idempotency).
        let again = f.into_issue(LintFormat::Clippy, "my-project");
        assert_eq!(again.key, issue.key);
    }

    #[test]
    fn finding_end_line_is_at_least_start() {
        let f = LintFinding {
            file: "a.py".to_string(),
            start_line: 5,
            end_line: 0,
            rule: "X".to_string(),
            severity: "error".to_string(),
            message: "x".to_string(),
        };
        let issue = f.into_issue(LintFormat::Ruff, "proj");
        let tr = issue.text_range.unwrap();
        assert_eq!(tr.start_line, 5);
        assert_eq!(tr.end_line, 5);
    }

    #[test]
    fn severity_normalization() {
        assert_eq!(normalize_severity("error"), "CRITICAL");
        assert_eq!(normalize_severity("WARNING"), "MAJOR");
        assert_eq!(normalize_severity("note"), "MINOR");
        assert_eq!(normalize_severity("hint"), "INFO");
        assert_eq!(normalize_severity("fatal"), "BLOCKER");
        assert_eq!(normalize_severity("bogus"), "MAJOR");
    }

    #[test]
    fn run_scan_no_command_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let issues = run_lint_scan(tmp.path(), None, None, false, 200, "proj").unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn autofix_command_uses_base() {
        let cmd = autofix_command_for(LintFormat::Clippy, "cargo clippy --all-targets").unwrap();
        assert!(cmd.contains("--fix"));
        assert!(cmd.contains("--allow-dirty"));
        assert_eq!(autofix_command_for(LintFormat::Eslint, "cargo clippy"), None);
    }

    #[test]
    fn severity_rank_ordering() {
        assert!(severity_rank("BLOCKER") < severity_rank("CRITICAL"));
        assert!(severity_rank("CRITICAL") < severity_rank("MAJOR"));
        assert!(severity_rank("MAJOR") < severity_rank("MINOR"));
        assert!(severity_rank("MINOR") < severity_rank("INFO"));
    }
}
