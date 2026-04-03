use chrono::Utc;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use tracing::info;

/// Result of processing a single issue
#[derive(Debug, Clone)]
pub struct IssueResult {
    pub issue_key: String,
    pub rule: String,
    pub severity: String,
    pub issue_type: String,
    pub message: String,
    pub file: String,
    pub lines: String,
    pub status: FixStatus,
    pub change_description: String,
    pub tests_added: Vec<String>,
    pub pr_url: Option<String>,
    /// Git diff summary for PR body (US-021)
    pub diff_summary: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FixStatus {
    Fixed,
    NeedsReview(String), // reason
    Failed(String),       // error
    Skipped(String),      // reason
}

/// Append an entry to TECHDEBT_CHANGELOG.md (US-013).
///
/// Each entry documents: timestamp, issue details, files affected,
/// change description, tests added, result, and PR reference.
/// The format is both human-readable and machine-parseable (consistent structure).
pub fn append_changelog(project_path: &Path, result: &IssueResult) {
    let changelog_path = project_path.join("TECHDEBT_CHANGELOG.md");
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    // -- Status with PR reference --
    let status_str = match &result.status {
        FixStatus::Fixed => {
            if let Some(pr) = &result.pr_url {
                format!("FIXED - {}", pr)
            } else {
                "FIXED".to_string()
            }
        }
        FixStatus::NeedsReview(reason) => format!("NEEDS_REVIEW - {}", reason),
        FixStatus::Failed(err) => format!("FAILED - {}", err),
        FixStatus::Skipped(reason) => format!("SKIPPED - {}", reason),
    };

    // -- Tests added with count --
    let tests_str = if result.tests_added.is_empty() {
        "None".to_string()
    } else {
        let count = result.tests_added.len();
        let files = result
            .tests_added
            .iter()
            .map(|t| format!("`{}`", t))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} (+{} file{})", files, count, if count == 1 { "" } else { "s" })
    };

    // -- PR reference line (only if available) --
    let pr_line = match &result.pr_url {
        Some(url) => format!("- **PR**: {}\n", url),
        None => String::new(),
    };

    let entry = format!(
        r#"
## [{now}] {key} - {severity} {issue_type}
- **Rule**: `{rule}` - {message}
- **Files**: `{file}:{lines}`
- **Change**: {change}
- **Tests added**: {tests}
- **Result**: {status}
{pr}---
"#,
        now = now,
        key = result.issue_key,
        severity = result.severity,
        issue_type = result.issue_type,
        rule = result.rule,
        message = result.message,
        file = result.file,
        lines = result.lines,
        change = result.change_description,
        tests = tests_str,
        status = status_str,
        pr = pr_line,
    );

    // Create file with header if it doesn't exist
    if !changelog_path.exists() {
        let header = "# Technical Debt Changelog\n\nAutomated changes by [Reparo](https://github.com/reparo).\n\n\
                      <!-- Machine-parseable: each entry is an H2 with format [timestamp] KEY - SEVERITY TYPE -->\n";
        let _ = fs::write(&changelog_path, header);
    }

    let mut content = fs::read_to_string(&changelog_path).unwrap_or_default();
    content.push_str(&entry);
    let _ = fs::write(&changelog_path, content);
}

/// Append a PR reference to the changelog after PR creation (US-013).
///
/// This is called after the PR is created so the URL can be recorded
/// as a note at the end of the batch's changelog entries.
#[allow(dead_code)]
pub fn append_changelog_pr_reference(
    project_path: &Path,
    batch: &[crate::sonar::Issue],
    pr_url: &str,
) {
    let changelog_path = project_path.join("TECHDEBT_CHANGELOG.md");
    if !changelog_path.exists() {
        return;
    }

    let keys: Vec<&str> = batch.iter().map(|i| i.key.as_str()).collect();
    let note = format!(
        "\n> **PR created**: {} (issues: {})\n",
        pr_url,
        keys.join(", "),
    );

    let mut content = fs::read_to_string(&changelog_path).unwrap_or_default();
    content.push_str(&note);
    let _ = fs::write(&changelog_path, content);
}

/// Analysis of why a test failure occurred.
/// Re-exported from orchestrator for use in report generation.
pub struct TestFailureAnalysis {
    pub reason: String,
    pub suggested_action: String,
}

/// Append to REVIEW_NEEDED.md for issues that need manual review (US-007).
pub fn append_review_needed(
    project_path: &Path,
    result: &IssueResult,
    failing_tests: &[String],
    analysis: &TestFailureAnalysis,
    test_output: &str,
) {
    let review_path = project_path.join("REVIEW_NEEDED.md");
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    let reason = match &result.status {
        FixStatus::NeedsReview(r) => r.as_str(),
        _ => &analysis.reason,
    };

    let failing_tests_str = if failing_tests.is_empty() {
        "Could not determine specific failing test(s)".to_string()
    } else {
        failing_tests
            .iter()
            .map(|t| format!("  - `{}`", t))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let entry = format!(
        r#"
## [{now}] {key} - {severity} {issue_type}
- **Rule**: {rule}
- **Message**: {message}
- **File**: `{file}:{lines}`
- **Attempted change**: {change}
- **Failing test(s)**:
{failing_tests}
- **Reason for manual review**: {reason}
- **Suggested action**: {action}
- **Test output** (truncated):
```
{test_output}
```
---
"#,
        now = now,
        key = result.issue_key,
        severity = result.severity,
        issue_type = result.issue_type,
        rule = result.rule,
        message = result.message,
        file = result.file,
        lines = result.lines,
        change = result.change_description,
        failing_tests = failing_tests_str,
        reason = reason,
        action = analysis.suggested_action,
        test_output = truncate(test_output, 500),
    );

    if !review_path.exists() {
        let header = "# Issues Needing Manual Review\n\nThese SonarQube issues could not be automatically fixed because the fix would require modifying existing tests.\n";
        let _ = fs::write(&review_path, header);
    }

    let mut content = fs::read_to_string(&review_path).unwrap_or_default();
    content.push_str(&entry);
    let _ = fs::write(&review_path, content);
}

/// Generate the final REPORT.md (US-011).
pub fn generate_report(
    project_path: &Path,
    results: &[IssueResult],
    total_issues_found: usize,
    elapsed_secs: u64,
) {
    let report_path = project_path.join("REPORT.md");
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    let processed = results.len();
    let fixed = results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
    let needs_review = results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))).count();
    let failed = results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))).count();
    let skipped = results.iter().filter(|r| matches!(r.status, FixStatus::Skipped(_))).count();
    let not_processed = total_issues_found.saturating_sub(processed);
    let total_tests: usize = results.iter().map(|r| r.tests_added.len()).sum();

    let prs: Vec<&str> = results
        .iter()
        .filter_map(|r| r.pr_url.as_deref())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let mut report = String::new();

    // -- Header --
    let _ = writeln!(report, "# Reparo Report");
    let _ = writeln!(report, "\nGenerated: {}\n", now);

    // -- Executive Summary --
    let _ = writeln!(report, "## Executive Summary\n");
    let _ = writeln!(report, "| Metric | Count |");
    let _ = writeln!(report, "|--------|------:|");
    let _ = writeln!(report, "| Total issues found | {} |", total_issues_found);
    let _ = writeln!(report, "| Issues processed | {} |", processed);
    let _ = writeln!(report, "| Fixed | {} |", fixed);
    let _ = writeln!(report, "| Needs manual review | {} |", needs_review);
    let _ = writeln!(report, "| Failed | {} |", failed);
    let _ = writeln!(report, "| Skipped (already processed) | {} |", skipped);
    if not_processed > 0 {
        let _ = writeln!(report, "| **Not processed** | **{}** |", not_processed);
    }
    let _ = writeln!(report, "| Test files generated | {} |", total_tests);
    let _ = writeln!(report, "| PRs created | {} |", prs.len());
    let _ = writeln!(report, "| Total time | {}m {}s |", elapsed_secs / 60, elapsed_secs % 60);

    // -- Fixed Issues --
    if fixed > 0 {
        let _ = writeln!(report, "\n## Fixed Issues\n");
        let _ = writeln!(report, "| Issue | Severity | Type | File | Rule | PR |");
        let _ = writeln!(report, "|-------|----------|------|------|------|----|");
        for r in results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)) {
            let pr = r.pr_url.as_deref().unwrap_or("-");
            let _ = writeln!(
                report,
                "| {} | {} | {} | `{}` | `{}` | {} |",
                r.issue_key, r.severity, r.issue_type, r.file, r.rule, pr
            );
        }
    }

    // -- Needs Manual Review --
    if needs_review > 0 {
        let _ = writeln!(report, "\n## Needs Manual Review\n");
        let _ = writeln!(
            report,
            "These issues could not be fixed automatically. See [`REVIEW_NEEDED.md`](REVIEW_NEEDED.md) for full details.\n"
        );
        let _ = writeln!(report, "| Issue | Severity | Type | File | Reason |");
        let _ = writeln!(report, "|-------|----------|------|------|--------|");
        for r in results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))) {
            let reason = match &r.status {
                FixStatus::NeedsReview(s) => s.clone(),
                _ => String::new(),
            };
            let _ = writeln!(
                report,
                "| {} | {} | {} | `{}` | {} |",
                r.issue_key, r.severity, r.issue_type, r.file, reason
            );
        }
    }

    // -- Failed --
    if failed > 0 {
        let _ = writeln!(report, "\n## Failed\n");
        let _ = writeln!(report, "| Issue | Severity | Type | File | Error |");
        let _ = writeln!(report, "|-------|----------|------|------|-------|");
        for r in results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))) {
            let err = match &r.status {
                FixStatus::Failed(s) => s.clone(),
                _ => String::new(),
            };
            let _ = writeln!(
                report,
                "| {} | {} | {} | `{}` | {} |",
                r.issue_key, r.severity, r.issue_type, r.file, err
            );
        }
    }

    // -- Not Processed --
    if not_processed > 0 {
        let _ = writeln!(report, "\n## Not Processed\n");
        let _ = writeln!(
            report,
            "{} issues were not processed. This may be due to `--max-issues` limit or an interrupted execution.\n",
            not_processed
        );
    }

    // -- Statistics --
    let _ = writeln!(report, "\n## Statistics\n");

    // By severity
    let _ = writeln!(report, "### By severity\n");
    let _ = writeln!(report, "| Severity | Fixed | Review | Failed | Skipped |");
    let _ = writeln!(report, "|----------|------:|-------:|-------:|--------:|");
    for sev in &["BLOCKER", "CRITICAL", "MAJOR", "MINOR", "INFO"] {
        let sev_results: Vec<&IssueResult> = results.iter().filter(|r| r.severity == *sev).collect();
        if sev_results.is_empty() {
            continue;
        }
        let sf = sev_results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
        let sr = sev_results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))).count();
        let sfa = sev_results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))).count();
        let ss = sev_results.iter().filter(|r| matches!(r.status, FixStatus::Skipped(_))).count();
        let _ = writeln!(report, "| {} | {} | {} | {} | {} |", sev, sf, sr, sfa, ss);
    }

    // By type
    let _ = writeln!(report, "\n### By type\n");
    let _ = writeln!(report, "| Type | Fixed | Review | Failed | Skipped |");
    let _ = writeln!(report, "|------|------:|-------:|-------:|--------:|");
    for typ in &["BUG", "VULNERABILITY", "SECURITY_HOTSPOT", "CODE_SMELL"] {
        let type_results: Vec<&IssueResult> = results.iter().filter(|r| r.issue_type == *typ).collect();
        if type_results.is_empty() {
            continue;
        }
        let tf = type_results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
        let tr = type_results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))).count();
        let tfa = type_results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))).count();
        let ts = type_results.iter().filter(|r| matches!(r.status, FixStatus::Skipped(_))).count();
        let _ = writeln!(report, "| {} | {} | {} | {} | {} |", typ, tf, tr, tfa, ts);
    }

    // Tests generated
    if total_tests > 0 {
        let _ = writeln!(report, "\n### Test files generated\n");
        for r in results.iter().filter(|r| !r.tests_added.is_empty()) {
            for t in &r.tests_added {
                let _ = writeln!(report, "- `{}` (for {})", t, r.issue_key);
            }
        }
    }

    // -- PRs --
    if !prs.is_empty() {
        let _ = writeln!(report, "\n## Pull Requests\n");
        for pr in &prs {
            let _ = writeln!(report, "- {}", pr);
        }
    }

    let _ = writeln!(report, "\n---\n*Generated by [Reparo](https://github.com/reparo)*");

    let _ = fs::write(&report_path, report);
    info!("Report written to {}", report_path.display());
}

fn truncate(s: &str, max: usize) -> String {
    crate::orchestrator::helpers::truncate(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_result(status: FixStatus) -> IssueResult {
        IssueResult {
            issue_key: "AX-123".to_string(),
            rule: "python:S1234".to_string(),
            severity: "CRITICAL".to_string(),
            issue_type: "BUG".to_string(),
            message: "Null pointer".to_string(),
            file: "src/service.py".to_string(),
            lines: "10-12".to_string(),
            status,
            change_description: "Added null check".to_string(),
            tests_added: vec!["tests/test_service.py".to_string()],
            pr_url: None,
            diff_summary: None,
        }
    }

    #[test]
    fn test_append_review_needed_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = sample_result(FixStatus::NeedsReview("Tests fail".to_string()));
        let analysis = TestFailureAnalysis {
            reason: "Null check changed return value".to_string(),
            suggested_action: "Update test expectations".to_string(),
        };
        let failing = vec!["test_service::test_get".to_string()];

        append_review_needed(tmp.path(), &result, &failing, &analysis, "AssertionError: expected None");

        let review_path = tmp.path().join("REVIEW_NEEDED.md");
        assert!(review_path.exists());
        let content = fs::read_to_string(&review_path).unwrap();
        assert!(content.contains("AX-123"));
        assert!(content.contains("CRITICAL"));
        assert!(content.contains("python:S1234"));
        assert!(content.contains("test_service::test_get"));
        assert!(content.contains("Update test expectations"));
        assert!(content.contains("AssertionError"));
    }

    #[test]
    fn test_append_review_needed_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let analysis = TestFailureAnalysis {
            reason: "reason".to_string(),
            suggested_action: "action".to_string(),
        };

        let r1 = sample_result(FixStatus::NeedsReview("fail1".to_string()));
        append_review_needed(tmp.path(), &r1, &[], &analysis, "out1");

        let mut r2 = sample_result(FixStatus::NeedsReview("fail2".to_string()));
        r2.issue_key = "AX-456".to_string();
        append_review_needed(tmp.path(), &r2, &[], &analysis, "out2");

        let content = fs::read_to_string(tmp.path().join("REVIEW_NEEDED.md")).unwrap();
        assert!(content.contains("AX-123"));
        assert!(content.contains("AX-456"));
    }

    #[test]
    fn test_append_changelog_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = sample_result(FixStatus::Fixed);

        append_changelog(tmp.path(), &result);

        let cl = tmp.path().join("TECHDEBT_CHANGELOG.md");
        assert!(cl.exists());
        let content = fs::read_to_string(&cl).unwrap();
        // Header
        assert!(content.contains("# Technical Debt Changelog"));
        assert!(content.contains("Machine-parseable"));
        // Entry
        assert!(content.contains("AX-123"));
        assert!(content.contains("CRITICAL BUG"));
        assert!(content.contains("`python:S1234`")); // rule in backticks
        assert!(content.contains("Added null check"));
        assert!(content.contains("tests/test_service.py"));
        assert!(content.contains("+1 file")); // test count
        assert!(content.contains("FIXED"));
    }

    #[test]
    fn test_append_changelog_with_pr_url() {
        let tmp = tempfile::tempdir().unwrap();
        let mut result = sample_result(FixStatus::Fixed);
        result.pr_url = Some("https://github.com/org/repo/pull/42".to_string());

        append_changelog(tmp.path(), &result);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("FIXED - https://github.com/org/repo/pull/42"));
        assert!(content.contains("**PR**: https://github.com/org/repo/pull/42"));
    }

    #[test]
    fn test_append_changelog_failed_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let result = sample_result(FixStatus::Failed("Claude made no changes".to_string()));

        append_changelog(tmp.path(), &result);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("FAILED - Claude made no changes"));
    }

    #[test]
    fn test_append_changelog_needs_review_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let result = sample_result(FixStatus::NeedsReview("Tests fail after fix".to_string()));

        append_changelog(tmp.path(), &result);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("NEEDS_REVIEW - Tests fail after fix"));
    }

    #[test]
    fn test_append_changelog_incremental() {
        let tmp = tempfile::tempdir().unwrap();

        let r1 = sample_result(FixStatus::Fixed);
        append_changelog(tmp.path(), &r1);

        let mut r2 = sample_result(FixStatus::Failed("err".to_string()));
        r2.issue_key = "AX-456".to_string();
        append_changelog(tmp.path(), &r2);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("AX-123"));
        assert!(content.contains("AX-456"));
        // Header should appear only once
        assert_eq!(content.matches("# Technical Debt Changelog").count(), 1);
    }

    #[test]
    fn test_append_changelog_no_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let mut result = sample_result(FixStatus::Fixed);
        result.tests_added = vec![];

        append_changelog(tmp.path(), &result);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("**Tests added**: None"));
    }

    #[test]
    fn test_append_changelog_multiple_test_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut result = sample_result(FixStatus::Fixed);
        result.tests_added = vec![
            "tests/test_a.py".to_string(),
            "tests/test_b.py".to_string(),
            "tests/test_c.py".to_string(),
        ];

        append_changelog(tmp.path(), &result);

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("+3 files"));
        assert!(content.contains("test_a.py"));
        assert!(content.contains("test_b.py"));
        assert!(content.contains("test_c.py"));
    }

    #[test]
    fn test_append_changelog_pr_reference() {
        let tmp = tempfile::tempdir().unwrap();

        // First write a changelog entry
        let result = sample_result(FixStatus::Fixed);
        append_changelog(tmp.path(), &result);

        // Then add PR reference
        let issue = crate::sonar::Issue {
            key: "AX-123".to_string(),
            rule: "python:S1234".to_string(),
            severity: "CRITICAL".to_string(),
            component: "proj:src/service.py".to_string(),
            issue_type: "BUG".to_string(),
            message: "Null pointer".to_string(),
            text_range: None,
            status: "OPEN".to_string(),
            tags: vec![],
        };
        append_changelog_pr_reference(
            tmp.path(),
            &[issue],
            "https://github.com/org/repo/pull/99",
        );

        let content = fs::read_to_string(tmp.path().join("TECHDEBT_CHANGELOG.md")).unwrap();
        assert!(content.contains("PR created"));
        assert!(content.contains("pull/99"));
        assert!(content.contains("AX-123"));
    }

    #[test]
    fn test_generate_report_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let results = vec![
            sample_result(FixStatus::Fixed),
            sample_result(FixStatus::NeedsReview("Tests fail".to_string())),
            sample_result(FixStatus::Failed("Claude error".to_string())),
        ];

        generate_report(tmp.path(), &results, 5, 125);

        let rp = tmp.path().join("REPORT.md");
        assert!(rp.exists());
        let content = fs::read_to_string(&rp).unwrap();
        assert!(content.contains("Total issues found | 5"));
        assert!(content.contains("Issues processed | 3"));
        assert!(content.contains("Fixed | 1"));
        assert!(content.contains("Needs manual review | 1"));
        assert!(content.contains("Failed | 1"));
        assert!(content.contains("2m 5s"));
    }

    #[test]
    fn test_generate_report_not_processed() {
        let tmp = tempfile::tempdir().unwrap();
        let results = vec![sample_result(FixStatus::Fixed)];

        generate_report(tmp.path(), &results, 10, 60);

        let content = fs::read_to_string(tmp.path().join("REPORT.md")).unwrap();
        assert!(content.contains("Not processed"));
        assert!(content.contains("9")); // 10 found - 1 processed = 9 not processed
    }

    #[test]
    fn test_generate_report_includes_rule_column() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = sample_result(FixStatus::Fixed);
        r.pr_url = Some("https://github.com/org/repo/pull/42".to_string());
        let results = vec![r];

        generate_report(tmp.path(), &results, 1, 30);

        let content = fs::read_to_string(tmp.path().join("REPORT.md")).unwrap();
        assert!(content.contains("python:S1234")); // rule in Fixed Issues table
        assert!(content.contains("pull/42"));
    }

    #[test]
    fn test_generate_report_severity_breakdown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r1 = sample_result(FixStatus::Fixed);
        r1.severity = "BLOCKER".to_string();
        let mut r2 = sample_result(FixStatus::Failed("err".to_string()));
        r2.severity = "BLOCKER".to_string();
        r2.issue_key = "AX-456".to_string();
        let results = vec![r1, r2];

        generate_report(tmp.path(), &results, 2, 10);

        let content = fs::read_to_string(tmp.path().join("REPORT.md")).unwrap();
        assert!(content.contains("By severity"));
        assert!(content.contains("BLOCKER"));
    }

    #[test]
    fn test_generate_report_test_files_section() {
        let tmp = tempfile::tempdir().unwrap();
        let r = sample_result(FixStatus::Fixed);
        // r already has tests_added: ["tests/test_service.py"]
        let results = vec![r];

        generate_report(tmp.path(), &results, 1, 5);

        let content = fs::read_to_string(tmp.path().join("REPORT.md")).unwrap();
        assert!(content.contains("Test files generated"));
        assert!(content.contains("tests/test_service.py"));
    }

    #[test]
    fn test_generate_report_review_table() {
        let tmp = tempfile::tempdir().unwrap();
        let r = sample_result(FixStatus::NeedsReview("Fix breaks test_foo".to_string()));
        let results = vec![r];

        generate_report(tmp.path(), &results, 1, 5);

        let content = fs::read_to_string(tmp.path().join("REPORT.md")).unwrap();
        assert!(content.contains("Needs Manual Review"));
        assert!(content.contains("Fix breaks test_foo"));
        assert!(content.contains("REVIEW_NEEDED.md"));
    }
}
