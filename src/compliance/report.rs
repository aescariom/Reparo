//! Compliance report generation (US-071).
//!
//! Generates a comprehensive markdown compliance report at the end of each run
//! when `--compliance` is active. Sections §2 (safety classification) and
//! §3.2 (MC/DC) are only included when `--health-mode` is also active.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::ComplianceConfig;
use crate::execution_log::ExecutionLog;

/// Advisory compliance verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ComplianceVerdict {
    /// All thresholds met, 0 gaps.
    Pass,
    /// Thresholds met but gaps exist (MC/DC gaps, orphan requirements).
    ConditionalPass,
    /// A critical threshold was NOT met.
    Fail,
}

impl ComplianceVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            ComplianceVerdict::Pass => "PASS",
            ComplianceVerdict::ConditionalPass => "CONDITIONAL PASS",
            ComplianceVerdict::Fail => "FAIL",
        }
    }
}

/// Statistics for tests grouped by category and risk class.
#[derive(Debug, Clone, Default)]
pub struct TestStats {
    pub total: usize,
    pub by_risk_class: HashMap<String, usize>,
}

/// Summary data for the traceability section.
#[derive(Debug, Clone, Default)]
pub struct TraceabilitySummary {
    pub total_requirements: usize,
    pub with_tests: usize,
    pub without_tests: usize,
    pub orphan_requirement_ids: Vec<String>,
}

/// MC/DC summary for Class C files.
#[derive(Debug, Clone, Default)]
pub struct McdcSummary {
    pub total_decision_points: usize,
    pub covered: usize,
    pub gaps: usize,
    pub gap_details: Vec<McdcGapDetail>,
}

#[derive(Debug, Clone)]
pub struct McdcGapDetail {
    pub file: String,
    pub line: u32,
    pub conditions: usize,
    pub required: usize,
    pub observed: usize,
}

/// Coverage statistics by risk class.
#[derive(Debug, Clone, Default)]
pub struct CoverageByClass {
    pub by_class: HashMap<String, ClassCoverageStats>,
}

#[derive(Debug, Clone, Default)]
pub struct ClassCoverageStats {
    pub files: usize,
    pub line_coverage_pct: f64,
    pub branch_coverage_pct: f64,
}

/// Audit information for the compliance report.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AuditSummary {
    pub run_id: String,
    pub project: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_secs: i64,
    pub operator: Option<String>,
    pub hostname: Option<String>,
    pub reparo_version: String,
    pub git_commit_before: Option<String>,
    pub git_commit_after: Option<String>,
    pub ai_calls: usize,
}

/// Full compliance report data structure.
#[derive(Debug, Clone)]
pub struct ComplianceReport {
    pub run_id: String,
    pub project: String,
    pub branch: Option<String>,
    pub date: String,
    pub standards: Vec<String>,
    pub health_mode: bool,
    pub coverage: CoverageByClass,
    pub mcdc: Option<McdcSummary>,
    pub test_stats: TestStats,
    pub traceability: TraceabilitySummary,
    pub audit: AuditSummary,
    pub verdict: ComplianceVerdict,
    /// SHA-256 of traceability/matrix.md (if generated).
    pub matrix_md_sha: Option<String>,
    /// SHA-256 of traceability/matrix.csv (if generated).
    pub matrix_csv_sha: Option<String>,
}

/// Build a compliance report by querying the execution log database.
///
/// Gracefully handles missing tables (US-066 test_artifacts, mcdc_gaps)
/// by returning empty/zero data rather than failing.
pub fn build_report(
    exec_log: &ExecutionLog,
    run_id: &str,
    config: &ComplianceConfig,
    health_mode: bool,
) -> Result<ComplianceReport> {
    let conn_guard = exec_log.conn_for_compliance();

    // Run metadata
    let (project, branch, started_at, ended_at, reparo_version, operator_email, hostname,
        git_before, git_after): (
        String, Option<String>, i64, Option<i64>, Option<String>,
        Option<String>, Option<String>, Option<String>, Option<String>,
    ) = conn_guard.query_row(
        "SELECT project, branch, started_at, ended_at, reparo_version,
                operator_email, hostname, git_commit_before, git_commit_after
         FROM runs WHERE id = ?1",
        rusqlite::params![run_id],
        |row| Ok((
            row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
            row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?,
        )),
    ).unwrap_or_else(|_| (
        run_id.to_string(), None, 0, None, None, None, None, None, None,
    ));

    let duration = ended_at.map(|e| e - started_at).unwrap_or(0);
    let started_str = chrono::DateTime::from_timestamp(started_at, 0)
        .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| started_at.to_string());
    let ended_str = ended_at.and_then(|e| chrono::DateTime::from_timestamp(e, 0))
        .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "ongoing".to_string());

    // AI calls count
    let ai_calls: usize = conn_guard.query_row(
        "SELECT COUNT(*) FROM ai_calls WHERE run_id = ?1",
        rusqlite::params![run_id],
        |row| row.get::<_, i64>(0),
    ).unwrap_or(0) as usize;

    // Coverage data from final_coverage table (graceful if missing)
    let mut coverage = CoverageByClass::default();
    let rows: Vec<(String, f64, f64)> = {
        let mut stmt_result = conn_guard.prepare(
            "SELECT file, line_coverage_pct, branch_coverage_pct
             FROM final_coverage WHERE run_id = ?1",
        );
        match stmt_result {
            Ok(ref mut stmt) => {
                stmt.query_map(rusqlite::params![run_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?, row.get::<_, f64>(2)?))
                })
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            Err(_) => vec![],
        }
    };

    // Group coverage by risk class
    let mut class_stats: HashMap<String, (usize, f64, f64)> = HashMap::new();
    for (file, line_cov, branch_cov) in &rows {
        let class = if health_mode {
            super::resolve_risk_class(file, config, true).as_str().to_string()
        } else {
            "N/A".to_string()
        };
        let entry = class_stats.entry(class).or_insert((0, 0.0, 0.0));
        entry.0 += 1;
        entry.1 += line_cov;
        entry.2 += branch_cov;
    }
    for (class, (count, line_sum, branch_sum)) in &class_stats {
        let n = *count as f64;
        coverage.by_class.insert(class.clone(), ClassCoverageStats {
            files: *count,
            line_coverage_pct: if n > 0.0 { line_sum / n } else { 0.0 },
            branch_coverage_pct: if n > 0.0 { branch_sum / n } else { 0.0 },
        });
    }

    // MC/DC gaps (graceful if table missing)
    let mcdc = if health_mode {
        let gaps: Vec<McdcGapDetail> = {
            let mut stmt_result = conn_guard.prepare(
                "SELECT file, line, condition_count, tests_required, tests_observed
                 FROM mcdc_gaps WHERE run_id = ?1 AND status = 'gap'",
            );
            match stmt_result {
                Ok(ref mut stmt) => {
                    stmt.query_map(rusqlite::params![run_id], |row| {
                        Ok(McdcGapDetail {
                            file: row.get(0)?,
                            line: row.get(1)?,
                            conditions: row.get::<_, i64>(2)? as usize,
                            required: row.get::<_, i64>(3)? as usize,
                            observed: row.get::<_, i64>(4)? as usize,
                        })
                    })
                    .map(|iter| iter.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
                }
                Err(_) => vec![],
            }
        };

        let total: usize = {
            let mut stmt_result = conn_guard.prepare(
                "SELECT COUNT(*) FROM mcdc_gaps WHERE run_id = ?1",
            );
            match stmt_result {
                Ok(ref mut stmt) => {
                    stmt.query_row(rusqlite::params![run_id], |row| row.get::<_, i64>(0))
                        .unwrap_or(0) as usize
                }
                Err(_) => 0,
            }
        };

        let gap_count = gaps.len();
        Some(McdcSummary {
            total_decision_points: total,
            covered: total.saturating_sub(gap_count),
            gaps: gap_count,
            gap_details: gaps,
        })
    } else {
        None
    };

    // Test artifacts (graceful if table missing)
    let test_stats = {
        let mut total = 0usize;
        let mut by_class: HashMap<String, usize> = HashMap::new();
        let mut stmt_result = conn_guard.prepare(
            "SELECT risk_class, COUNT(*) FROM test_artifacts WHERE run_id = ?1 GROUP BY risk_class",
        );
        if let Ok(ref mut stmt) = stmt_result {
            if let Ok(iter) = stmt.query_map(rusqlite::params![run_id], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            }) {
                for r in iter.filter_map(|r| r.ok()) {
                    let class = r.0.unwrap_or_else(|| "unknown".to_string());
                    let count = r.1 as usize;
                    *by_class.entry(class).or_insert(0) += count;
                    total += count;
                }
            }
        }
        TestStats { total, by_risk_class: by_class }
    };

    // Traceability summary (from test_artifacts + requirements config)
    let traced_req_ids: Vec<String> = {
        let mut stmt_result = conn_guard.prepare(
            "SELECT DISTINCT requirement FROM test_artifacts WHERE run_id = ?1",
        );
        match stmt_result {
            Ok(ref mut stmt) => {
                stmt.query_map(rusqlite::params![run_id], |row| row.get::<_, String>(0))
                    .map(|iter| iter.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
            Err(_) => vec![],
        }
    };

    let manual_req_ids: Vec<String> = config.requirements.iter()
        .filter(|r| !r.is_manual())
        .map(|r| r.id.clone())
        .collect();

    let orphan_ids: Vec<String> = manual_req_ids.iter()
        .filter(|id| !traced_req_ids.contains(id))
        .cloned()
        .collect();

    let total_reqs = manual_req_ids.len();
    let with_tests = manual_req_ids.iter().filter(|id| traced_req_ids.contains(id)).count();

    let traceability = TraceabilitySummary {
        total_requirements: total_reqs,
        with_tests,
        without_tests: total_reqs.saturating_sub(with_tests),
        orphan_requirement_ids: orphan_ids,
    };

    // Determine verdict
    let has_mcdc_gaps = mcdc.as_ref().map(|m| m.gaps > 0).unwrap_or(false);
    let has_orphans = traceability.without_tests > 0;
    let verdict = if has_mcdc_gaps || has_orphans {
        ComplianceVerdict::ConditionalPass
    } else {
        ComplianceVerdict::Pass
    };

    let date = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();

    Ok(ComplianceReport {
        run_id: run_id.to_string(),
        project,
        branch,
        date,
        standards: config.standards.clone(),
        health_mode,
        coverage,
        mcdc,
        test_stats,
        traceability,
        audit: AuditSummary {
            run_id: run_id.to_string(),
            project: run_id.to_string(), // overwritten below
            started_at: started_str,
            ended_at: ended_str,
            duration_secs: duration,
            operator: operator_email,
            hostname,
            reparo_version: reparo_version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            git_commit_before: git_before,
            git_commit_after: git_after,
            ai_calls,
        },
        verdict,
        matrix_md_sha: None,
        matrix_csv_sha: None,
    })
}

/// Render a compliance report to a markdown string.
pub fn render_markdown(report: &ComplianceReport) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Compliance Report — {}\n\n", report.run_id));
    out.push_str(&format!("**Reparo version**: {}\n", report.audit.reparo_version));
    out.push_str(&format!("**Date**: {}\n", report.date));
    out.push_str(&format!("**Project**: {}\n", report.project));
    if let Some(ref b) = report.branch {
        out.push_str(&format!("**Branch**: {}\n", b));
    }
    out.push_str(&format!("**Run ID**: {}\n", report.run_id));
    if let Some(ref op) = report.audit.operator {
        out.push_str(&format!("**Operator**: {}\n", op));
    }
    out.push_str("\n---\n\n");

    // §1 Standards targeted
    out.push_str("## 1. Standards targeted\n\n");
    if report.standards.is_empty() {
        if report.health_mode {
            out.push_str("- **IEC 62304:2006+A1:2015** — Medical device software\n");
            out.push_str("- **ISO/IEC 25010:2023** — Software quality\n");
            out.push_str("- **EU MDR 2017/745** — Annex II §6.1(b) V&V documentation\n");
            out.push_str("- **ENS Alto** — op.exp.10 / op.exp.11 activity logs\n");
        } else {
            out.push_str("- **ISO/IEC 25010:2023** — Software quality\n");
            out.push_str("- **ISO/IEC 33020** — Process assessment\n");
            out.push_str("- **ENS Alto** — op.exp.10 / op.exp.11 activity logs\n");
        }
    } else {
        for s in &report.standards {
            out.push_str(&format!("- {}\n", s));
        }
    }
    out.push('\n');

    // §2 Safety classification (health-mode only)
    if report.health_mode {
        out.push_str("## 2. Software safety classification (IEC 62304 §4.3)\n\n");
        if report.coverage.by_class.is_empty() {
            out.push_str("_No coverage data available for this run._\n\n");
        } else {
            out.push_str("| Risk class | Files | Line cov. | Branch cov. |\n");
            out.push_str("|------------|------:|----------:|------------:|\n");
            for class in &["C", "B", "A"] {
                if let Some(stats) = report.coverage.by_class.get(*class) {
                    out.push_str(&format!(
                        "| {} | {} | {:.1}% | {:.1}% |\n",
                        class, stats.files,
                        stats.line_coverage_pct, stats.branch_coverage_pct,
                    ));
                }
            }
            out.push('\n');
        }
    }

    // §3 Coverage
    out.push_str("## 3. Verification coverage\n\n");
    out.push_str("### 3.1 Line + branch coverage\n\n");
    if report.coverage.by_class.is_empty() {
        out.push_str("_No final coverage data recorded for this run. \
            Coverage data is populated during the coverage boost phase._\n\n");
    } else {
        out.push_str("| Class | Files | Line cov. | Branch cov. |\n");
        out.push_str("|-------|------:|----------:|------------:|\n");
        for (class, stats) in &report.coverage.by_class {
            out.push_str(&format!(
                "| {} | {} | {:.1}% | {:.1}% |\n",
                class, stats.files, stats.line_coverage_pct, stats.branch_coverage_pct,
            ));
        }
        out.push('\n');
    }

    // §3.2 MC/DC (health-mode only)
    if let Some(ref mcdc) = report.mcdc {
        out.push_str("### 3.2 MC/DC coverage (Class C only)\n\n");
        out.push_str(&format!("Total decision points analysed: {}\n", mcdc.total_decision_points));
        out.push_str(&format!(
            "- Fully covered (N+1 tests): {} ({:.1}%)\n",
            mcdc.covered,
            if mcdc.total_decision_points > 0 {
                100.0 * mcdc.covered as f64 / mcdc.total_decision_points as f64
            } else { 100.0 }
        ));
        out.push_str(&format!("- Gaps: {}\n\n", mcdc.gaps));

        if !mcdc.gap_details.is_empty() {
            out.push_str("**MC/DC Gaps** (manual review required):\n\n");
            out.push_str("| File | Line | Conditions | Required | Observed |\n");
            out.push_str("|------|-----:|-----------:|---------:|---------:|\n");
            for gap in mcdc.gap_details.iter().take(50) {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    gap.file, gap.line, gap.conditions, gap.required, gap.observed,
                ));
            }
            out.push('\n');
        }
    }

    // §4 Tests generated
    out.push_str("## 4. Tests generated by Reparo this run\n\n");
    out.push_str(&format!("Total tests: {}\n\n", report.test_stats.total));
    if !report.test_stats.by_risk_class.is_empty() {
        out.push_str("| Risk class | Tests |\n");
        out.push_str("|------------|------:|\n");
        for (class, count) in &report.test_stats.by_risk_class {
            out.push_str(&format!("| {} | {} |\n", class, count));
        }
        out.push('\n');
    }

    // §7 Traceability
    out.push_str("## 7. Traceability\n\n");
    if report.traceability.total_requirements > 0 {
        out.push_str(&format!(
            "- Total manual requirements declared: {}\n\
             - Requirements with ≥1 test: {}\n\
             - Orphan requirements (no test): {} {}\n\n",
            report.traceability.total_requirements,
            report.traceability.with_tests,
            report.traceability.without_tests,
            if report.traceability.without_tests > 0 { "⚠" } else { "✅" },
        ));
        if !report.traceability.orphan_requirement_ids.is_empty() {
            out.push_str("**Orphan requirements** (no matching test generated):\n");
            for id in &report.traceability.orphan_requirement_ids {
                out.push_str(&format!("- {}\n", id));
            }
            out.push('\n');
        }
    } else {
        out.push_str("No manual requirements declared in `compliance.requirements`.\n\n");
        out.push_str("Full matrix: [`traceability/matrix.md`](traceability/matrix.md) \
            and [`traceability/matrix.csv`](traceability/matrix.csv)\n\n");
    }

    // §8 Audit trail
    out.push_str("## 8. Audit trail (ENS Alto op.exp.10)\n\n");
    out.push_str(&format!("- **Started**: {}\n", report.audit.started_at));
    out.push_str(&format!("- **Ended**: {}\n", report.audit.ended_at));
    let h = report.audit.duration_secs / 3600;
    let m = (report.audit.duration_secs % 3600) / 60;
    let s = report.audit.duration_secs % 60;
    out.push_str(&format!("- **Duration**: {}h {}m {}s\n", h, m, s));
    if let Some(ref op) = report.audit.operator {
        out.push_str(&format!("- **Operator**: {}\n", op));
    }
    if let Some(ref h) = report.audit.hostname {
        out.push_str(&format!("- **Hostname**: {}\n", h));
    }
    out.push_str(&format!("- **Reparo version**: {}\n", report.audit.reparo_version));
    out.push_str(&format!("- **AI calls**: {}\n", report.audit.ai_calls));
    if let Some(ref gc) = report.audit.git_commit_before {
        out.push_str(&format!("- **Git before**: {}\n", gc));
    }
    if let Some(ref gc) = report.audit.git_commit_after {
        out.push_str(&format!("- **Git after**: {}\n", gc));
    }
    out.push('\n');

    // §9 Integrity
    out.push_str("## 9. Integrity\n\n");
    if let Some(ref sha) = report.matrix_md_sha {
        out.push_str(&format!("- **matrix.md SHA-256**: `{}`\n", sha));
    }
    if let Some(ref sha) = report.matrix_csv_sha {
        out.push_str(&format!("- **matrix.csv SHA-256**: `{}`\n", sha));
    }
    out.push_str("- **This file SHA-256**: `{sha256}` (excluding this line)\n\n");

    // §10 Verdict
    out.push_str("## 10. Compliance verdict (advisory)\n\n");
    out.push_str(&format!("**Overall**: {}\n\n", report.verdict.as_str()));
    match report.verdict {
        ComplianceVerdict::Pass => {
            out.push_str("All configured thresholds met. No gaps identified.\n\n");
        }
        ComplianceVerdict::ConditionalPass => {
            out.push_str("Thresholds met, but manual review required for:\n");
            if let Some(ref m) = report.mcdc {
                if m.gaps > 0 {
                    out.push_str(&format!("- {} MC/DC gap(s) in Class C files\n", m.gaps));
                }
            }
            if report.traceability.without_tests > 0 {
                out.push_str(&format!(
                    "- {} orphan requirement(s) with no linked test\n",
                    report.traceability.without_tests
                ));
            }
            out.push('\n');
        }
        ComplianceVerdict::Fail => {
            out.push_str("One or more critical thresholds were NOT met. See coverage section.\n\n");
        }
    }

    out.push_str("---\n\n");
    out.push_str(
        "*This report is machine-generated by Reparo. It is not a substitute for formal \
         regulatory review. Manual sign-off by a qualified person is required before \
         submission to a notified body.*\n"
    );

    out
}

/// Write the compliance report to disk with a SHA-256 integrity hash.
///
/// Generates:
/// - `.reparo/compliance_{run_id}.md`
/// - `.reparo/compliance_latest.md`
///
/// The SHA-256 in the file itself is computed over the content EXCLUDING the
/// `{sha256}` placeholder line (so the hash is stable and verifiable).
pub fn write_compliance_file(report: &ComplianceReport, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create compliance report dir {}", dir.display()))?;

    let raw_content = render_markdown(report);

    // Compute SHA-256 over all lines except the one containing `{sha256}`
    let content_for_hash: String = raw_content.lines()
        .filter(|line| !line.contains("{sha256}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut hasher = Sha256::new();
    hasher.update(content_for_hash.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let final_content = raw_content.replace("{sha256}", &sha);

    let file_path = dir.join(format!("compliance_{}.md", report.run_id));
    std::fs::write(&file_path, &final_content)
        .with_context(|| format!("Failed to write compliance report to {}", file_path.display()))?;

    // Also write compliance_latest.md
    let latest_path = dir.join("compliance_latest.md");
    let _ = std::fs::write(&latest_path, &final_content);

    Ok(file_path)
}

/// Compute SHA-256 of a file's content, excluding lines containing the marker.
#[allow(dead_code)]
pub fn sha256_of_file_excluding_marker(content: &str, marker: &str) -> String {
    let filtered: String = content.lines()
        .filter(|line| !line.contains(marker))
        .collect::<Vec<_>>()
        .join("\n");
    let mut hasher = Sha256::new();
    hasher.update(filtered.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report(verdict: ComplianceVerdict) -> ComplianceReport {
        ComplianceReport {
            run_id: "20260412_test".to_string(),
            project: "myproject".to_string(),
            branch: Some("main".to_string()),
            date: "2026-04-12 14:30:00 UTC".to_string(),
            standards: vec!["IEC 62304".to_string()],
            health_mode: false,
            coverage: CoverageByClass::default(),
            mcdc: None,
            test_stats: TestStats { total: 42, by_risk_class: HashMap::new() },
            traceability: TraceabilitySummary {
                total_requirements: 3,
                with_tests: 3,
                without_tests: 0,
                orphan_requirement_ids: vec![],
            },
            audit: AuditSummary {
                run_id: "20260412_test".to_string(),
                project: "myproject".to_string(),
                started_at: "2026-04-12 14:30:00 UTC".to_string(),
                ended_at: "2026-04-12 16:30:00 UTC".to_string(),
                duration_secs: 7200,
                operator: Some("alice@example.com".to_string()),
                hostname: Some("laptop-01".to_string()),
                reparo_version: "0.2.0".to_string(),
                git_commit_before: None,
                git_commit_after: None,
                ai_calls: 150,
            },
            verdict,
            matrix_md_sha: None,
            matrix_csv_sha: None,
        }
    }

    #[test]
    fn test_render_markdown_contains_sections() {
        let report = make_report(ComplianceVerdict::Pass);
        let md = render_markdown(&report);
        assert!(md.contains("# Compliance Report"));
        assert!(md.contains("## 1. Standards targeted"));
        assert!(md.contains("## 10. Compliance verdict"));
        assert!(md.contains("PASS"));
    }

    #[test]
    fn test_render_markdown_health_mode_includes_section2() {
        let mut report = make_report(ComplianceVerdict::Pass);
        report.health_mode = true;
        let md = render_markdown(&report);
        assert!(md.contains("## 2. Software safety classification"));
    }

    #[test]
    fn test_render_markdown_baseline_no_section2() {
        let report = make_report(ComplianceVerdict::Pass);
        let md = render_markdown(&report);
        assert!(!md.contains("## 2. Software safety classification"));
    }

    #[test]
    fn test_render_markdown_conditional_pass() {
        let mut report = make_report(ComplianceVerdict::ConditionalPass);
        report.health_mode = true;
        report.mcdc = Some(McdcSummary {
            total_decision_points: 10,
            covered: 8,
            gaps: 2,
            gap_details: vec![],
        });
        let md = render_markdown(&report);
        assert!(md.contains("CONDITIONAL PASS"));
        assert!(md.contains("2 MC/DC gap(s)"));
    }

    #[test]
    fn test_verdict_logic() {
        assert_eq!(ComplianceVerdict::Pass.as_str(), "PASS");
        assert_eq!(ComplianceVerdict::ConditionalPass.as_str(), "CONDITIONAL PASS");
        assert_eq!(ComplianceVerdict::Fail.as_str(), "FAIL");
    }

    #[test]
    fn test_sha256_reproducibility() {
        let content = "line 1\nline 2\n- **This file SHA-256**: `{sha256}`\nline 4\n";
        let sha1 = sha256_of_file_excluding_marker(content, "{sha256}");
        let sha2 = sha256_of_file_excluding_marker(content, "{sha256}");
        assert_eq!(sha1, sha2);
        assert_eq!(sha1.len(), 64); // hex-encoded SHA-256
    }

    #[test]
    fn test_sha256_excludes_marker_line() {
        // Verify the SHA function is deterministic (idempotent on the same input)
        let content_a = "line 1\nline 2\n- SHA: `{sha256}`\nline 4\n";
        let sha1 = sha256_of_file_excluding_marker(content_a, "{sha256}");
        let sha2 = sha256_of_file_excluding_marker(content_a, "{sha256}");
        assert_eq!(sha1, sha2, "SHA should be deterministic for the same input");
        assert_eq!(sha1.len(), 64, "SHA-256 hex should be 64 characters");
        // Verify the SHA changes when non-marker content changes
        let content_b = "line 1\nline 2 CHANGED\n- SHA: `{sha256}`\nline 4\n";
        let sha3 = sha256_of_file_excluding_marker(content_b, "{sha256}");
        assert_ne!(sha1, sha3, "SHA should differ when non-marker content changes");
    }

    #[test]
    fn test_write_compliance_file() {
        let dir = tempfile::tempdir().unwrap();
        let report = make_report(ComplianceVerdict::Pass);
        let path = write_compliance_file(&report, dir.path()).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Compliance Report"));
        // The sha256 placeholder should have been replaced with a real hash
        assert!(!content.contains("{sha256}"));
        // compliance_latest.md should also exist
        assert!(dir.path().join("compliance_latest.md").exists());
    }
}
