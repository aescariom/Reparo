use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

use crate::config::{ScannerKind, ValidatedConfig};

// --- SonarQube API types ---

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub key: String,
    pub rule: String,
    pub severity: String,
    pub component: String,
    #[serde(rename = "type")]
    pub issue_type: String,
    pub message: String,
    #[serde(rename = "textRange")]
    pub text_range: Option<TextRange>,
    pub status: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Sonar-estimated remediation effort (e.g. "5min", "1h30min", "2h", "1d").
    /// Present for server-returned issues; `None` for synthetic linter findings.
    #[serde(default)]
    pub effort: Option<String>,
}

/// Parse a SonarQube effort duration string into total minutes.
///
/// SonarQube returns effort/debt as compact tokens like `"5min"`, `"1h30min"`,
/// `"2h"`, or `"1d"` (a workday is 8h by Sonar convention). Returns `None` for
/// empty input or unrecognized units.
pub fn parse_effort_minutes(effort: &str) -> Option<u32> {
    let s = effort.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let mut total: u32 = 0;
    let mut num_buf = String::new();
    let mut chars = s.chars().peekable();
    let mut saw_any_unit = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num_buf.push(c);
            continue;
        }
        if c.is_ascii_alphabetic() {
            let mut unit = String::from(c);
            while let Some(&p) = chars.peek() {
                if p.is_ascii_alphabetic() {
                    unit.push(p);
                    chars.next();
                } else {
                    break;
                }
            }
            let n: u32 = num_buf.parse().ok()?;
            num_buf.clear();
            let mult = match unit.as_str() {
                "min" => 1,
                "h" => 60,
                "d" => 8 * 60,
                _ => return None,
            };
            total = total.saturating_add(n.saturating_mul(mult));
            saw_any_unit = true;
            continue;
        }
        return None;
    }
    if !saw_any_unit {
        return None;
    }
    Some(total)
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextRange {
    #[serde(rename = "startLine")]
    pub start_line: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(rename = "startOffset")]
    pub start_offset: Option<u32>,
    #[serde(rename = "endOffset")]
    pub end_offset: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct IssuesResponse {
    issues: Vec<Issue>,
    paging: Paging,
}

#[derive(Debug, Deserialize)]
struct Paging {
    total: u32,
    #[serde(rename = "pageIndex")]
    page_index: u32,
    #[serde(rename = "pageSize")]
    page_size: u32,
}

#[derive(Debug, Deserialize)]
struct CeTaskResponse {
    task: CeTask,
}

#[derive(Debug, Deserialize)]
struct CeTask {
    status: String,
}

#[derive(Debug, Deserialize)]
struct ComponentMeasures {
    component: ComponentWithMeasures,
}

#[derive(Debug, Deserialize)]
struct ComponentWithMeasures {
    #[serde(default)]
    measures: Vec<Measure>,
}

#[derive(Debug, Deserialize)]
pub struct Measure {
    pub metric: String,
    pub value: Option<String>,
}

/// Response from /api/sources/lines
#[derive(Debug, Deserialize)]
struct SourceLinesResponse {
    sources: Vec<SourceLine>,
}

#[derive(Debug, Deserialize)]
struct SourceLine {
    line: u32,
    /// true if the line is covered by at least one test, false if not, absent if not coverable
    #[serde(rename = "lineHits")]
    line_hits: Option<i32>,
    /// Whether the line has associated conditions (branch coverage)
    #[serde(default)]
    conditions: Option<i32>,
    #[serde(rename = "coveredConditions")]
    #[serde(default)]
    covered_conditions: Option<i32>,
}

/// Result of checking line-level coverage for an issue's affected lines.
#[derive(Debug, Clone)]
pub struct CoverageResult {
    /// Lines that are covered by tests (lineHits > 0)
    pub covered_lines: Vec<u32>,
    /// Lines that are coverable but NOT covered (lineHits == 0)
    pub uncovered_lines: Vec<u32>,
    /// Lines that are not coverable (no lineHits data — comments, blank, declarations)
    pub non_coverable_lines: Vec<u32>,
    /// Coverage percentage over coverable lines only
    pub coverage_pct: f64,
    /// Whether all coverable lines are covered
    pub fully_covered: bool,
}

impl CoverageResult {
    pub fn log_summary(&self, file: &str, start_line: u32, end_line: u32) {
        let total_coverable = self.covered_lines.len() + self.uncovered_lines.len();
        tracing::info!(
            "Coverage for {}:{}-{}: {:.1}% ({}/{} coverable lines covered)",
            file,
            start_line,
            end_line,
            self.coverage_pct,
            self.covered_lines.len(),
            total_coverable,
        );
        if !self.uncovered_lines.is_empty() {
            let lines_str: Vec<String> = self.uncovered_lines.iter().map(|l| l.to_string()).collect();
            tracing::info!(
                "  Uncovered lines: {}",
                lines_str.join(", ")
            );
        }
        if self.fully_covered {
            tracing::info!("  All affected lines are covered");
        }
    }
}

#[derive(Debug, Deserialize)]
struct RuleResponse {
    rule: RuleDetail,
}

#[derive(Debug, Deserialize)]
struct RuleDetail {
    #[serde(rename = "htmlDesc")]
    html_desc: Option<String>,
    #[serde(rename = "mdDesc")]
    md_desc: Option<String>,
    name: String,
}

// --- Client ---

#[derive(Clone)]
pub struct SonarClient {
    base_url: String,
    token: String,
    project_id: String,
    client: reqwest::Client,
    /// Whether the server supports branch analysis (Developer Edition+)
    supports_branches: bool,
    /// Whether to include test-scope issues when querying. Default (false)
    /// matches the SonarQube web UI's default view, which hides TEST-scope
    /// issues and shows only MAIN.
    include_test_issues: bool,
    /// Glob patterns matched against each issue's file path (relative to
    /// project root). Any matching issue is dropped from `fetch_issues`.
    /// Populated from:
    ///   1. `sonar-project.properties` (`sonar.exclusions`, `sonar.test.exclusions`)
    ///   2. reparo's own `--exclude` CLI flag / YAML `sonar.exclusions`
    exclusion_globs: Vec<String>,
}

impl SonarClient {
    pub fn new(config: &ValidatedConfig) -> Self {
        // `reqwest::Client::new()` has NO timeout by default — a stalled TCP
        // connection or a SonarQube server under load makes any `.await` on a
        // response block forever, silently, with no heartbeat because we're
        // not in a subprocess. Parallel wave workers hit this regularly: 4
        // workers all call `get_line_coverage` at the start of `process_issue`,
        // and one stuck HTTP call freezes the whole wave.
        //
        // Three layered limits:
        //   - connect_timeout(30s): TCP + TLS handshake must finish.
        //   - timeout(300s):        whole-request budget (connect + headers +
        //                           body). Long enough for slow `issues/search`
        //                           pages on large projects but firm enough to
        //                           surface a stuck request instead of hanging.
        //   - tcp_keepalive(30s):   detects dropped connections that would
        //                           otherwise leave the worker parked on a
        //                           silently dead socket.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "Failed to build reqwest client with timeouts: {} — falling back to default (no timeouts!)",
                    e
                );
                reqwest::Client::new()
            });
        // Merge exclusions from (a) reparo's config (CLI/YAML) and (b) the
        // project's own `sonar-project.properties` so reparo drops the same
        // issues the user already tells SonarQube to ignore.
        let mut exclusion_globs: Vec<String> = config.sonar_exclusions.clone();
        let props_file = config.path.join("sonar-project.properties");
        if props_file.exists() {
            match std::fs::read_to_string(&props_file) {
                Ok(contents) => {
                    let from_props = parse_properties_exclusions(&contents);
                    if !from_props.is_empty() {
                        info!(
                            "Loaded {} exclusion glob(s) from {}",
                            from_props.len(),
                            props_file.display()
                        );
                        exclusion_globs.extend(from_props);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Could not read {}: {} — skipping properties-based exclusions",
                        props_file.display(),
                        e
                    );
                }
            }
        }
        // De-duplicate while preserving order.
        let mut seen = std::collections::HashSet::new();
        exclusion_globs.retain(|g| seen.insert(g.clone()));
        if !exclusion_globs.is_empty() {
            info!(
                "SonarClient will drop issues matching {} exclusion glob(s)",
                exclusion_globs.len()
            );
        }

        Self {
            base_url: config.sonar_url.clone(),
            token: config.sonar_token.clone(),
            project_id: config.sonar_project_id.clone(),
            client,
            supports_branches: false,
            include_test_issues: config.include_test_issues,
            exclusion_globs,
        }
    }

    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let req = self.client.get(&url);
        if !self.token.is_empty() {
            req.basic_auth(&self.token, Some(""))
        } else {
            req
        }
    }

    /// Run the appropriate scanner on the project (US-002).
    ///
    /// Returns the CE task ID if it can be parsed from the scanner report,
    /// which is then used by `wait_for_analysis` for precise polling.
    pub fn run_scanner(
        &self,
        project_path: &Path,
        scanner: &ScannerKind,
        branch: &str,
    ) -> Result<Option<String>> {
        let start = chrono::Utc::now();
        info!(
            "[{}] Running {} on {}",
            start.format("%Y-%m-%dT%H:%M:%SZ"),
            scanner.display_name(),
            project_path.display()
        );

        let mut common_args = vec![
            format!("-Dsonar.projectKey={}", self.project_id),
            format!("-Dsonar.host.url={}", self.base_url),
            format!("-Dsonar.token={}", self.token),
        ];
        if self.supports_branches {
            common_args.push(format!("-Dsonar.branch.name={}", branch));
        }

        let output = match scanner {
            ScannerKind::SonarScanner(bin) => {
                Command::new(bin)
                    .current_dir(project_path)
                    .args(&common_args)
                    .output()
                    .context("Failed to execute sonar-scanner")?
            }
            ScannerKind::Maven(bin) => {
                let mut args = vec!["sonar:sonar".to_string()];
                args.extend(common_args.iter().cloned());
                Command::new(bin)
                    .current_dir(project_path)
                    .args(&args)
                    .output()
                    .context("Failed to execute mvn sonar:sonar")?
            }
            ScannerKind::Gradle(bin) => {
                let mut args = vec!["sonarqube".to_string()];
                args.extend(common_args.iter().cloned());
                Command::new(bin)
                    .current_dir(project_path)
                    .args(&args)
                    .output()
                    .context("Failed to execute gradle sonarqube")?
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            // Show last 30 lines of output for diagnosis
            let tail: String = stderr
                .lines()
                .chain(stdout.lines())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take(30)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "Scanner failed (exit {}):\n{}",
                output.status,
                tail
            );
        }

        let end = chrono::Utc::now();
        let elapsed = (end - start).num_seconds();
        info!(
            "[{}] Scanner completed in {}s",
            end.format("%Y-%m-%dT%H:%M:%SZ"),
            elapsed
        );

        // Try to extract the CE task ID from the report-task.txt file
        // sonar-scanner writes it to .scannerwork/report-task.txt
        let task_id = read_ce_task_id(project_path);
        if let Some(ref id) = task_id {
            info!("CE task ID: {}", id);
        }

        Ok(task_id)
    }

    /// Wait for SonarQube to finish processing the analysis (US-002).
    ///
    /// If `ce_task_id` is provided, polls that specific task (precise).
    /// Otherwise, falls back to polling the latest CE activity for the project.
    pub async fn wait_for_analysis(&self, ce_task_id: Option<&str>) -> Result<()> {
        info!("Waiting for SonarQube server to process results...");

        let max_attempts = 120; // ~6 minutes with 3s interval
        for attempt in 0..max_attempts {
            let (status_str, error_msg) = if let Some(task_id) = ce_task_id {
                self.poll_ce_task(task_id).await?
            } else {
                self.poll_ce_activity().await?
            };

            match status_str.as_str() {
                "SUCCESS" => {
                    info!("SonarQube analysis completed successfully");
                    return Ok(());
                }
                "FAILED" => {
                    let detail = error_msg.unwrap_or_default();
                    anyhow::bail!("SonarQube analysis FAILED: {}", detail);
                }
                "CANCELED" => {
                    anyhow::bail!("SonarQube analysis was CANCELED");
                }
                "PENDING" | "IN_PROGRESS" => {
                    if attempt % 10 == 0 {
                        info!("Analysis status: {} (attempt {}/{})", status_str, attempt + 1, max_attempts);
                    }
                }
                other => {
                    warn!("Unknown analysis status: '{}' (attempt {})", other, attempt);
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        anyhow::bail!(
            "Timed out after {} attempts waiting for SonarQube analysis to complete",
            max_attempts
        );
    }

    /// Poll a specific CE task by ID. Returns (status, optional error message).
    async fn poll_ce_task(&self, task_id: &str) -> Result<(String, Option<String>)> {
        let resp = self
            .request("/api/ce/task")
            .query(&[("id", task_id)])
            .send()
            .await
            .context("Failed to poll CE task")?;

        if !resp.status().is_success() {
            return Ok(("UNKNOWN".to_string(), None));
        }

        let body: serde_json::Value = resp.json().await?;
        let status = body["task"]["status"]
            .as_str()
            .unwrap_or("UNKNOWN")
            .to_string();
        let error = body["task"]["errorMessage"]
            .as_str()
            .map(String::from);

        Ok((status, error))
    }

    /// Fallback: poll the latest CE activity for the project component.
    async fn poll_ce_activity(&self) -> Result<(String, Option<String>)> {
        let resp = self
            .request("/api/ce/activity")
            .query(&[
                ("component", self.project_id.as_str()),
                ("ps", "1"),
                ("onlyCurrents", "true"),
            ])
            .send()
            .await
            .context("Failed to check CE activity")?;

        if !resp.status().is_success() {
            return Ok(("UNKNOWN".to_string(), None));
        }

        let body: serde_json::Value = resp.json().await?;
        if let Some(tasks) = body["tasks"].as_array() {
            if let Some(task) = tasks.first() {
                let status = task["status"]
                    .as_str()
                    .unwrap_or("UNKNOWN")
                    .to_string();
                let error = task["errorMessage"].as_str().map(String::from);
                return Ok((status, error));
            }
        }

        // No tasks found — might still be queued
        Ok(("PENDING".to_string(), None))
    }

    /// Fetch all open issues, sorted by severity
    pub async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        let mut all_issues = Vec::new();
        let mut page = 1u32;
        let page_size = 100u32;

        // SonarQube's `/api/issues/search` returns BOTH MAIN-scope (source) and
        // TEST-scope (test code) issues when `scopes` is omitted. The SonarQube
        // web UI defaults to showing only MAIN, which is why reparo's count
        // (no filter → 1900) was massively larger than the scanner-UI count
        // (MAIN only → ~600). Match the UI by default; users who want to fix
        // test-code issues too pass `--include-test-issues`.
        let scopes = if self.include_test_issues {
            "MAIN,TEST"
        } else {
            "MAIN"
        };
        info!("Fetching SonarQube issues with scopes={}", scopes);

        loop {
            let resp = self
                .request("/api/issues/search")
                .query(&[
                    ("componentKeys", self.project_id.as_str()),
                    ("statuses", "OPEN,REOPENED"),
                    ("scopes", scopes),
                    ("ps", &page_size.to_string()),
                    ("p", &page.to_string()),
                ])
                .send()
                .await
                .context("Failed to fetch issues from SonarQube")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("SonarQube API error ({}): {}", status, body);
            }

            let issues_resp: IssuesResponse = resp.json().await
                .context("Failed to parse SonarQube issues response")?;

            all_issues.extend(issues_resp.issues);

            let total_pages = (issues_resp.paging.total + page_size - 1) / page_size;
            if page >= total_pages {
                break;
            }
            page += 1;
        }

        // Apply `sonar.exclusions` / reparo `--exclude` filtering client-side.
        // Sonar is supposed to exclude these at scan time via
        // `sonar-project.properties`, but in practice older analyses,
        // forgotten properties files, or excludes added AFTER an analysis
        // leave excluded issues in the server's DB. Filter them here so
        // reparo never burns AI budget on files the user marked off-limits.
        if !self.exclusion_globs.is_empty() {
            let before = all_issues.len();
            all_issues.retain(|issue| {
                let path = component_to_path(&issue.component);
                let excluded = self
                    .exclusion_globs
                    .iter()
                    .any(|g| matches_exclusion(&path, g));
                !excluded
            });
            let dropped = before - all_issues.len();
            if dropped > 0 {
                info!(
                    "Dropped {} issue(s) matching sonar.exclusions / --exclude globs",
                    dropped
                );
            }
        }

        // Sort by: category (Security > Reliability > Maintainability)
        //          → severity (BLOCKER > CRITICAL > MAJOR > MINOR > INFO)
        //          → complexity descending (most complex first)
        all_issues.sort_by(|a, b| {
            let cat_a = category_rank(&a.issue_type);
            let cat_b = category_rank(&b.issue_type);
            cat_a.cmp(&cat_b)
                .then_with(|| {
                    let sev_a = severity_rank(&a.severity);
                    let sev_b = severity_rank(&b.severity);
                    sev_a.cmp(&sev_b)
                })
                .then_with(|| {
                    // Most complex first (higher complexity = earlier)
                    let cplx_a = extract_complexity(&a.message);
                    let cplx_b = extract_complexity(&b.message);
                    cplx_b.cmp(&cplx_a)
                })
        });

        info!("Fetched {} issues from SonarQube", all_issues.len());
        Ok(all_issues)
    }

    /// Get coverage for a specific file component
    pub async fn get_file_coverage(&self, component_key: &str) -> Result<Option<f64>> {
        let resp = self
            .request("/api/measures/component")
            .query(&[
                ("component", component_key),
                ("metricKeys", "line_coverage,coverage"),
            ])
            .send()
            .await
            .context("Failed to fetch coverage from SonarQube")?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let measures: ComponentMeasures = resp.json().await?;
        for m in &measures.component.measures {
            if m.metric == "line_coverage" || m.metric == "coverage" {
                if let Some(val) = &m.value {
                    if let Ok(v) = val.parse::<f64>() {
                        return Ok(Some(v));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Get line-level coverage for specific lines of a file component (US-004).
    ///
    /// Queries `/api/sources/lines` for the affected line range and classifies
    /// each line as covered, uncovered, or non-coverable.
    pub async fn get_line_coverage(
        &self,
        component_key: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<CoverageResult> {
        let resp = self
            .request("/api/sources/lines")
            .query(&[
                ("key", component_key),
                ("from", &start_line.to_string()),
                ("to", &end_line.to_string()),
            ])
            .send()
            .await
            .context("Failed to fetch source lines from SonarQube")?;

        if !resp.status().is_success() {
            // Fall back to file-level coverage if line API is not available
            let file_cov = self.get_file_coverage(component_key).await?;
            return Ok(build_coverage_result_from_file_level(
                file_cov,
                start_line,
                end_line,
            ));
        }

        let body: serde_json::Value = resp.json().await?;

        // Parse sources array — the API returns it directly or nested
        let sources: Vec<SourceLine> = if let Some(arr) = body.get("sources") {
            serde_json::from_value(arr.clone()).unwrap_or_default()
        } else {
            Vec::new()
        };

        if sources.is_empty() {
            // No line data — fall back to file-level
            let file_cov = self.get_file_coverage(component_key).await?;
            return Ok(build_coverage_result_from_file_level(
                file_cov,
                start_line,
                end_line,
            ));
        }

        let mut covered = Vec::new();
        let mut uncovered = Vec::new();
        let mut non_coverable = Vec::new();

        for src in &sources {
            match src.line_hits {
                Some(hits) if hits > 0 => covered.push(src.line),
                Some(_) => uncovered.push(src.line), // lineHits == 0
                None => non_coverable.push(src.line), // not coverable
            }
        }

        let total_coverable = covered.len() + uncovered.len();
        let coverage_pct = if total_coverable == 0 {
            0.0 // no coverage data → treat as uncovered, not as 100%
        } else {
            (covered.len() as f64 / total_coverable as f64) * 100.0
        };

        // If there's no coverage data at all (all lines non-coverable),
        // treat the affected lines as uncovered to trigger test generation.
        let fully_covered = if total_coverable == 0 && !non_coverable.is_empty() {
            false
        } else {
            uncovered.is_empty()
        };

        if total_coverable == 0 && !non_coverable.is_empty() {
            warn!(
                "No coverage instrumentation for {} lines — treating as uncovered",
                non_coverable.len()
            );
        }

        Ok(CoverageResult {
            fully_covered,
            covered_lines: covered,
            uncovered_lines: if total_coverable == 0 { non_coverable.clone() } else { uncovered },
            non_coverable_lines: non_coverable,
            coverage_pct,
        })
    }

    /// Get the rule description for a sonar rule
    pub async fn get_rule_description(&self, rule_key: &str) -> Result<String> {
        let resp = self
            .request("/api/rules/show")
            .query(&[("key", rule_key)])
            .send()
            .await
            .context("Failed to fetch rule description")?;

        if !resp.status().is_success() {
            return Ok(format!("Rule: {}", rule_key));
        }

        let rule_resp: RuleResponse = resp.json().await?;
        let desc = rule_resp
            .rule
            .md_desc
            .or(rule_resp.rule.html_desc)
            .unwrap_or_default();

        Ok(format!("# {}\n\n{}", rule_resp.rule.name, desc))
    }

    /// Detect the SonarQube edition and configure branch support accordingly.
    /// Must be called before `run_scanner` to avoid passing unsupported parameters.
    pub async fn detect_edition(&mut self) {
        if let Ok(resp) = self.request("/api/navigation/global").send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let edition = json["edition"].as_str().unwrap_or("community");
                        self.supports_branches = edition != "community";
                        if !self.supports_branches {
                            info!("SonarQube Community Edition detected — branch analysis disabled");
                        }
                    }
                }
            }
        }
    }

    /// Check if the SonarQube server is reachable and the project exists.
    pub async fn check_connection(&self) -> Result<()> {
        let resp = self
            .request("/api/components/show")
            .query(&[("component", self.project_id.as_str())])
            .send()
            .await
            .context("Failed to connect to SonarQube server")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "SonarQube project '{}' not accessible ({}): {}",
                self.project_id,
                status,
                body
            );
        }

        info!("SonarQube connection OK, project '{}' found", self.project_id);
        Ok(())
    }

    /// Get the project-wide duplication percentage from SonarQube.
    pub async fn get_duplication_percentage(&self) -> Result<f64> {
        let resp = self
            .request("/api/measures/component")
            .query(&[
                ("component", self.project_id.as_str()),
                ("metricKeys", "duplicated_lines_density"),
            ])
            .send()
            .await
            .context("Failed to fetch duplication metrics")?;

        if !resp.status().is_success() {
            anyhow::bail!("SonarQube returned {} for duplication metrics", resp.status());
        }

        let measures: ComponentMeasures = resp.json().await?;
        for m in &measures.component.measures {
            if m.metric == "duplicated_lines_density" {
                if let Some(val) = &m.value {
                    if let Ok(v) = val.parse::<f64>() {
                        return Ok(v);
                    }
                }
            }
        }

        Ok(0.0)
    }

    /// Fetch duplicated blocks for a specific file.
    /// Returns a list of DuplicatedBlock with line ranges.
    pub async fn get_file_duplications(&self, component_key: &str) -> Result<Vec<DuplicatedBlock>> {
        let resp = self
            .request("/api/duplications/show")
            .query(&[("key", component_key)])
            .send()
            .await
            .context("Failed to fetch duplications")?;

        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let body: DuplicationsResponse = resp.json().await.unwrap_or_default();
        let mut blocks = Vec::new();

        for dup in &body.duplications {
            for block in &dup.blocks {
                // Only include blocks from this file (not cross-file duplicates for now)
                if let Some(ref key) = block._ref_key {
                    blocks.push(DuplicatedBlock {
                        component: key.clone(),
                        from: block.from,
                        size: block.size,
                    });
                } else {
                    // _ref points to a files entry — use the component key
                    blocks.push(DuplicatedBlock {
                        component: component_key.to_string(),
                        from: block.from,
                        size: block.size,
                    });
                }
            }
        }

        Ok(blocks)
    }

    /// Fetch the list of files with duplications, sorted by most duplicated lines first.
    pub async fn get_files_with_duplications(&self) -> Result<Vec<FileDuplication>> {
        let mut page: u32 = 1;
        let mut all_files = Vec::new();

        loop {
            let resp = self
                .request("/api/measures/component_tree")
                .query(&[
                    ("component", self.project_id.as_str()),
                    ("metricKeys", "duplicated_lines,duplicated_lines_density"),
                    ("metricSort", "duplicated_lines"),
                    ("s", "metric"),
                    ("asc", "false"),
                    ("ps", "100"),
                    ("p", &page.to_string()),
                    ("qualifiers", "FIL"),
                ])
                .send()
                .await
                .context("Failed to fetch files with duplications")?;

            if !resp.status().is_success() {
                break;
            }

            let tree: ComponentTreeResponse = resp.json().await.unwrap_or_default();

            for comp in &tree.components {
                let mut dup_lines = 0u64;
                let mut dup_pct = 0.0f64;

                for m in &comp.measures {
                    if m.metric == "duplicated_lines" {
                        if let Some(ref v) = m.value {
                            dup_lines = v.parse().unwrap_or(0);
                        }
                    }
                    if m.metric == "duplicated_lines_density" {
                        if let Some(ref v) = m.value {
                            dup_pct = v.parse().unwrap_or(0.0);
                        }
                    }
                }

                if dup_lines > 0 {
                    all_files.push(FileDuplication {
                        component_key: comp.key.clone(),
                        file_path: component_to_path(&comp.key),
                        duplicated_lines: dup_lines,
                        duplication_pct: dup_pct,
                    });
                }
            }

            let total = tree.paging.map(|p| p.total).unwrap_or(0);
            if page * 100 >= total {
                break;
            }
            page += 1;
        }

        Ok(all_files)
    }
}

/// A duplicated block within a file.
#[derive(Debug, Clone)]
pub struct DuplicatedBlock {
    pub component: String,
    pub from: u32,
    pub size: u32,
}

/// Per-file duplication info.
#[derive(Debug, Clone)]
pub struct FileDuplication {
    pub component_key: String,
    pub file_path: String,
    pub duplicated_lines: u64,
    pub duplication_pct: f64,
}

/// Response from /api/duplications/show
#[derive(Debug, Default, Deserialize)]
struct DuplicationsResponse {
    #[serde(default)]
    duplications: Vec<Duplication>,
}

#[derive(Debug, Deserialize)]
struct Duplication {
    blocks: Vec<DuplicationBlock>,
}

#[derive(Debug, Deserialize)]
struct DuplicationBlock {
    from: u32,
    size: u32,
    #[serde(rename = "_ref")]
    _ref_key: Option<String>,
}

/// Response from /api/measures/component_tree
#[derive(Debug, Default, Deserialize)]
struct ComponentTreeResponse {
    #[serde(default)]
    components: Vec<ComponentTreeEntry>,
    paging: Option<Paging>,
}

#[derive(Debug, Deserialize)]
struct ComponentTreeEntry {
    key: String,
    #[serde(default)]
    measures: Vec<Measure>,
}


/// Extract the relative file path from a SonarQube component key
/// Component keys look like: "project-id:src/main/java/Foo.java"
pub fn component_to_path(component: &str) -> String {
    if let Some(pos) = component.find(':') {
        component[pos + 1..].to_string()
    } else {
        component.to_string()
    }
}

/// Compute an affinity key for wave-sharding: groups files that are likely to
/// conflict in cherry-picks together so they get placed in different waves.
///
/// `depth = 0` returns the file path itself (previous behavior — only the same
/// file conflicts with itself).  `depth = 1` returns the file's parent
/// directory (fixes to any file in `.../security/auth/` serialize).  Higher
/// depths climb further up the tree.
///
/// When the file has fewer components than `depth`, the whole path is returned
/// (so we never collapse to an empty string).
pub fn component_to_affinity_key(component: &str, depth: usize) -> String {
    let path = component_to_path(component);
    if depth == 0 {
        return path;
    }
    // Split on both native separators; SonarQube emits forward slashes even on
    // Windows, but be defensive.
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    if parts.len() <= depth + 1 {
        // File is at the root (or too shallow for the requested depth): fall
        // back to the file path so every such file gets its own bucket.
        return path;
    }
    // Keep everything except the last `depth` path components (which include
    // the file name for depth=1, parent+file for depth=2, etc.).
    let keep = parts.len() - depth;
    parts[..keep].join("/")
}

/// Parse `sonar.exclusions`, `sonar.test.exclusions`, and `sonar.coverage.exclusions`
/// entries from the text of a `sonar-project.properties` file.
///
/// Each property is a comma-separated list of Ant-style globs (e.g.
/// `src/generated/**,**/*Generated.java`). Lines starting with `#` or `!` are
/// comments. Continuation via trailing `\\` is not supported (rare in practice).
pub(crate) fn parse_properties_exclusions(contents: &str) -> Vec<String> {
    const KEYS: &[&str] = &[
        "sonar.exclusions",
        "sonar.test.exclusions",
        "sonar.coverage.exclusions",
        "sonar.cpd.exclusions",
    ];
    let mut out = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let eq = match line.find('=') {
            Some(i) => i,
            None => continue,
        };
        let key = line[..eq].trim();
        if !KEYS.iter().any(|k| *k == key) {
            continue;
        }
        let value = line[eq + 1..].trim();
        for pat in value.split(',') {
            let p = pat.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Match a file path against a sonar-style Ant glob pattern.
///
/// Supports the patterns that actually appear in `sonar.exclusions`:
///   - `**` matches any number of path segments
///   - `*`  matches any sequence within a single segment
///   - `?`  matches a single character
///
/// Implemented by translating to a Rust `glob::Pattern`; `**` is preserved
/// because `glob` already understands it.
pub(crate) fn matches_exclusion(path: &str, pattern: &str) -> bool {
    // Sonar patterns frequently are expressed relative to src root without a
    // leading `**/`, e.g. `*Generated.java` is expected to match every file
    // with that name. Normalize such patterns by also trying a `**/` prefix.
    let glob_pat = match ::glob::Pattern::new(pattern) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if glob_pat.matches(path) {
        return true;
    }
    if !pattern.starts_with("**/") && !pattern.starts_with('/') {
        if let Ok(p2) = ::glob::Pattern::new(&format!("**/{}", pattern)) {
            if p2.matches(path) {
                return true;
            }
        }
    }
    false
}

/// Build a CoverageResult from file-level coverage when line-level API is not available.
fn build_coverage_result_from_file_level(
    file_cov: Option<f64>,
    start_line: u32,
    end_line: u32,
) -> CoverageResult {
    let all_lines: Vec<u32> = (start_line..=end_line).collect();
    match file_cov {
        Some(pct) if pct >= 100.0 => CoverageResult {
            covered_lines: all_lines,
            uncovered_lines: vec![],
            non_coverable_lines: vec![],
            coverage_pct: 100.0,
            fully_covered: true,
        },
        Some(pct) => {
            // We don't know which specific lines are uncovered, so mark all as uncovered
            // to be safe — this triggers test generation
            CoverageResult {
                covered_lines: vec![],
                uncovered_lines: all_lines,
                non_coverable_lines: vec![],
                coverage_pct: pct,
                fully_covered: false,
            }
        }
        None => {
            // No coverage data at all — treat as unknown/uncovered
            CoverageResult {
                covered_lines: vec![],
                uncovered_lines: all_lines,
                non_coverable_lines: vec![],
                coverage_pct: 0.0,
                fully_covered: false,
            }
        }
    }
}

/// Try to read the CE task ID from the scanner's report-task.txt.
///
/// sonar-scanner writes to `.scannerwork/report-task.txt`.
/// Maven/Gradle write to `target/sonar/report-task.txt` or `build/sonar/report-task.txt`.
///
/// Picks the most recently modified candidate so that a stale file from a
/// previous scanner (e.g. a leftover `.scannerwork/` from standalone sonar-scanner
/// in a project now scanned with Maven) cannot shadow the fresh task ID.
fn read_ce_task_id(project_path: &Path) -> Option<String> {
    let candidates = [
        project_path.join(".scannerwork/report-task.txt"),
        project_path.join("target/sonar/report-task.txt"),
        project_path.join("build/sonar/report-task.txt"),
    ];

    let freshest = candidates
        .iter()
        .filter_map(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .map(|mtime| (p, mtime))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(p, _)| p.clone())?;

    let content = std::fs::read_to_string(&freshest).ok()?;
    for line in content.lines() {
        if let Some(id) = line.strip_prefix("ceTaskId=") {
            let id = id.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }

    None
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "BLOCKER" => 0,
        "CRITICAL" => 1,
        "MAJOR" => 2,
        "MINOR" => 3,
        "INFO" => 4,
        _ => 5,
    }
}

/// Rank by category: Security > Reliability > Maintainability
fn category_rank(issue_type: &str) -> u8 {
    match issue_type {
        "VULNERABILITY" | "SECURITY_HOTSPOT" => 0, // Security
        "BUG" => 1,                                // Reliability
        "CODE_SMELL" => 2,                         // Maintainability
        _ => 3,
    }
}

/// Legacy type_rank for backward compatibility in tests
fn type_rank(t: &str) -> u8 {
    category_rank(t)
}

/// Extract cognitive complexity from SonarQube message.
/// e.g. "Refactor this function to reduce its Cognitive Complexity from 45 to the 15 allowed."
/// Returns the complexity value, or 0 if not found.
fn extract_complexity(message: &str) -> u32 {
    if let Some(from_idx) = message.find("from ") {
        let after_from = &message[from_idx + 5..];
        if let Some(end) = after_from.find(|c: char| !c.is_ascii_digit()) {
            return after_from[..end].parse().unwrap_or(0);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_properties_exclusions_basic() {
        let props = r#"
# comment
sonar.projectKey=foo
sonar.exclusions=src/generated/**,**/*Generated.java
sonar.test.exclusions=src/test/legacy/**
sonar.coverage.exclusions=**/*Dto.java
sonar.cpd.exclusions=**/*Builder.java
other.setting=ignored
"#;
        let got = parse_properties_exclusions(props);
        assert_eq!(
            got,
            vec![
                "src/generated/**".to_string(),
                "**/*Generated.java".to_string(),
                "src/test/legacy/**".to_string(),
                "**/*Dto.java".to_string(),
                "**/*Builder.java".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_properties_exclusions_empty_and_comments() {
        let props = "# nothing here\n!also a comment\n\nsonar.exclusions=";
        assert!(parse_properties_exclusions(props).is_empty());
    }

    #[test]
    fn test_matches_exclusion_anchored() {
        assert!(matches_exclusion(
            "src/main/java/com/legacy/Foo.java",
            "src/main/java/com/legacy/**"
        ));
        assert!(!matches_exclusion(
            "src/main/java/com/fresh/Foo.java",
            "src/main/java/com/legacy/**"
        ));
    }

    #[test]
    fn test_matches_exclusion_auto_anchored_bare_name() {
        // A pattern without `**/` prefix like `*Generated.java` should match
        // at any depth — this is how Sonar users typically write it.
        assert!(matches_exclusion("src/main/java/FooGenerated.java", "*Generated.java"));
        assert!(matches_exclusion("src/gen/BarGenerated.java", "*Generated.java"));
        assert!(!matches_exclusion("src/main/java/Foo.java", "*Generated.java"));
    }

    #[test]
    fn test_matches_exclusion_double_star() {
        assert!(matches_exclusion(
            "src/main/java/com/a/b/c/X.java",
            "**/X.java"
        ));
        assert!(matches_exclusion(
            "src/main/java/deep/pkg/Y.java",
            "src/main/java/**/Y.java"
        ));
    }

    #[test]
    fn test_read_ce_task_id_scannerwork() {
        let tmp = tempfile::tempdir().unwrap();
        let sw = tmp.path().join(".scannerwork");
        fs::create_dir_all(&sw).unwrap();
        fs::write(
            sw.join("report-task.txt"),
            "projectKey=test\nserverUrl=http://localhost:9000\nceTaskId=AYZ_abc123\nceTaskUrl=http://localhost:9000/api/ce/task?id=AYZ_abc123\n",
        )
        .unwrap();
        assert_eq!(
            read_ce_task_id(tmp.path()),
            Some("AYZ_abc123".to_string())
        );
    }

    #[test]
    fn test_read_ce_task_id_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_ce_task_id(tmp.path()), None);
    }

    #[test]
    fn test_read_ce_task_id_maven() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("target/sonar");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report-task.txt"), "ceTaskId=MVN_task_42\n").unwrap();
        assert_eq!(
            read_ce_task_id(tmp.path()),
            Some("MVN_task_42".to_string())
        );
    }

    #[test]
    fn test_read_ce_task_id_gradle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build/sonar");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report-task.txt"), "ceTaskId=GRD_555\n").unwrap();
        assert_eq!(
            read_ce_task_id(tmp.path()),
            Some("GRD_555".to_string())
        );
    }

    #[test]
    fn test_component_to_path() {
        assert_eq!(
            component_to_path("my-project:src/main/Foo.java"),
            "src/main/Foo.java"
        );
        assert_eq!(component_to_path("bare-file.py"), "bare-file.py");
    }

    #[test]
    fn test_severity_rank_order() {
        assert!(severity_rank("BLOCKER") < severity_rank("CRITICAL"));
        assert!(severity_rank("CRITICAL") < severity_rank("MAJOR"));
        assert!(severity_rank("MAJOR") < severity_rank("MINOR"));
        assert!(severity_rank("MINOR") < severity_rank("INFO"));
        assert!(severity_rank("INFO") < severity_rank("UNKNOWN"));
    }

    #[test]
    fn test_category_rank_order() {
        // Security > Reliability > Maintainability
        assert!(category_rank("VULNERABILITY") < category_rank("BUG"));
        assert!(category_rank("SECURITY_HOTSPOT") < category_rank("BUG"));
        assert!(category_rank("BUG") < category_rank("CODE_SMELL"));
    }

    #[test]
    fn test_extract_complexity() {
        assert_eq!(extract_complexity("Refactor this function to reduce its Cognitive Complexity from 45 to the 15 allowed."), 45);
        assert_eq!(extract_complexity("Refactor this function to reduce its Cognitive Complexity from 108 to the 15 allowed."), 108);
        assert_eq!(extract_complexity("Remove this unused import of 'NgClass'."), 0);
    }

    /// Helper to create a test Issue with minimal fields.
    fn make_issue(key: &str, severity: &str, issue_type: &str) -> Issue {
        Issue {
            key: key.to_string(),
            rule: "test:rule".to_string(),
            severity: severity.to_string(),
            component: "proj:src/file.py".to_string(),
            issue_type: issue_type.to_string(),
            message: "test message".to_string(),
            text_range: Some(TextRange {
                start_line: 1,
                end_line: 1,
                start_offset: None,
                end_offset: None,
            }),
            status: "OPEN".to_string(),
            tags: vec![],
            effort: None,
        }
    }

    #[test]
    fn test_issues_sort_category_then_severity() {
        let mut issues = vec![
            make_issue("1", "MINOR", "CODE_SMELL"),
            make_issue("2", "BLOCKER", "VULNERABILITY"),
            make_issue("3", "BLOCKER", "BUG"),
            make_issue("4", "CRITICAL", "CODE_SMELL"),
            make_issue("5", "CRITICAL", "BUG"),
            make_issue("6", "MAJOR", "SECURITY_HOTSPOT"),
            make_issue("7", "INFO", "CODE_SMELL"),
        ];

        // Apply the same sorting as fetch_issues
        issues.sort_by(|a, b| {
            category_rank(&a.issue_type).cmp(&category_rank(&b.issue_type))
                .then_with(|| severity_rank(&a.severity).cmp(&severity_rank(&b.severity)))
                .then_with(|| extract_complexity(&b.message).cmp(&extract_complexity(&a.message)))
        });

        let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
        // Expected order:
        //   Security:  BLOCKER VULN (2), MAJOR SECURITY_HOTSPOT (6)
        //   Reliability: BLOCKER BUG (3), CRITICAL BUG (5)
        //   Maintainability: CRITICAL CODE_SMELL (4), MINOR CODE_SMELL (1), INFO CODE_SMELL (7)
        assert_eq!(keys, vec!["2", "6", "3", "5", "4", "1", "7"]);
    }

    #[test]
    fn test_issues_sort_same_severity_by_category() {
        let mut issues = vec![
            make_issue("a", "MAJOR", "CODE_SMELL"),
            make_issue("b", "MAJOR", "SECURITY_HOTSPOT"),
            make_issue("c", "MAJOR", "BUG"),
            make_issue("d", "MAJOR", "VULNERABILITY"),
        ];

        issues.sort_by(|a, b| {
            category_rank(&a.issue_type).cmp(&category_rank(&b.issue_type))
                .then_with(|| severity_rank(&a.severity).cmp(&severity_rank(&b.severity)))
        });

        let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
        // Security (VULN + HOTSPOT, same rank) > Reliability (BUG) > Maintainability (CODE_SMELL)
        // b and d are both Security (rank 0), stable sort preserves input order
        assert_eq!(keys[2], "c"); // BUG
        assert_eq!(keys[3], "a"); // CODE_SMELL
        assert!(keys[0] == "b" || keys[0] == "d"); // both Security
    }

    #[test]
    fn test_issues_sort_same_category_severity_by_complexity() {
        let mut issues = vec![
            Issue { message: "Refactor this function to reduce its Cognitive Complexity from 15 to the 15 allowed.".to_string(), ..make_issue("low", "CRITICAL", "CODE_SMELL") },
            Issue { message: "Refactor this function to reduce its Cognitive Complexity from 108 to the 15 allowed.".to_string(), ..make_issue("high", "CRITICAL", "CODE_SMELL") },
            Issue { message: "Refactor this function to reduce its Cognitive Complexity from 45 to the 15 allowed.".to_string(), ..make_issue("mid", "CRITICAL", "CODE_SMELL") },
        ];

        issues.sort_by(|a, b| {
            category_rank(&a.issue_type).cmp(&category_rank(&b.issue_type))
                .then_with(|| severity_rank(&a.severity).cmp(&severity_rank(&b.severity)))
                .then_with(|| extract_complexity(&b.message).cmp(&extract_complexity(&a.message)))
        });

        let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
        // Most complex first: 108, 45, 15
        assert_eq!(keys, vec!["high", "mid", "low"]);
    }

    #[test]
    fn test_issues_response_deserialization() {
        let json = r#"{
            "issues": [
                {
                    "key": "AX1",
                    "rule": "python:S1234",
                    "severity": "CRITICAL",
                    "component": "proj:src/main.py",
                    "type": "BUG",
                    "message": "Fix this null deref",
                    "textRange": {
                        "startLine": 10,
                        "endLine": 12,
                        "startOffset": 4,
                        "endOffset": 20
                    },
                    "status": "OPEN",
                    "tags": ["cwe", "owasp"]
                },
                {
                    "key": "AX2",
                    "rule": "python:S5678",
                    "severity": "MINOR",
                    "component": "proj:src/util.py",
                    "type": "CODE_SMELL",
                    "message": "Rename this variable",
                    "status": "REOPENED"
                }
            ],
            "paging": {
                "total": 2,
                "pageIndex": 1,
                "pageSize": 100
            }
        }"#;

        let resp: IssuesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.issues.len(), 2);
        assert_eq!(resp.paging.total, 2);

        let issue1 = &resp.issues[0];
        assert_eq!(issue1.key, "AX1");
        assert_eq!(issue1.severity, "CRITICAL");
        assert_eq!(issue1.issue_type, "BUG");
        assert_eq!(issue1.component, "proj:src/main.py");
        let tr = issue1.text_range.as_ref().unwrap();
        assert_eq!(tr.start_line, 10);
        assert_eq!(tr.end_line, 12);
        assert_eq!(issue1.tags, vec!["cwe", "owasp"]);

        let issue2 = &resp.issues[1];
        assert_eq!(issue2.key, "AX2");
        assert!(issue2.text_range.is_none());
        assert_eq!(issue2.status, "REOPENED");
        assert!(issue2.tags.is_empty());
    }

    // -- Coverage tests (US-004) --

    #[test]
    fn test_build_coverage_from_file_level_full() {
        let result = build_coverage_result_from_file_level(Some(100.0), 10, 15);
        assert!(result.fully_covered);
        assert_eq!(result.coverage_pct, 100.0);
        assert_eq!(result.covered_lines, vec![10, 11, 12, 13, 14, 15]);
        assert!(result.uncovered_lines.is_empty());
    }

    #[test]
    fn test_build_coverage_from_file_level_partial() {
        let result = build_coverage_result_from_file_level(Some(75.5), 5, 8);
        assert!(!result.fully_covered);
        assert_eq!(result.coverage_pct, 75.5);
        // When we can't determine specific lines, all are marked uncovered
        assert_eq!(result.uncovered_lines, vec![5, 6, 7, 8]);
        assert!(result.covered_lines.is_empty());
    }

    #[test]
    fn test_build_coverage_from_file_level_none() {
        let result = build_coverage_result_from_file_level(None, 1, 3);
        assert!(!result.fully_covered);
        assert_eq!(result.coverage_pct, 0.0);
        assert_eq!(result.uncovered_lines, vec![1, 2, 3]);
    }

    #[test]
    fn test_coverage_result_all_non_coverable() {
        // When all lines lack coverage data, treat as uncovered (not 100%)
        let result = CoverageResult {
            covered_lines: vec![],
            uncovered_lines: vec![1, 2, 3], // non-coverable lines promoted to uncovered
            non_coverable_lines: vec![1, 2, 3],
            coverage_pct: 0.0,
            fully_covered: false,
        };
        assert!(!result.fully_covered);
        assert_eq!(result.coverage_pct, 0.0);
    }

    #[test]
    fn test_coverage_result_mixed_lines() {
        let covered = vec![10, 11, 13];
        let uncovered = vec![12, 14];
        let total_coverable = covered.len() + uncovered.len();
        let pct = (covered.len() as f64 / total_coverable as f64) * 100.0;

        let result = CoverageResult {
            covered_lines: covered,
            uncovered_lines: uncovered.clone(),
            non_coverable_lines: vec![15], // comment line
            coverage_pct: pct,
            fully_covered: false,
        };

        assert!(!result.fully_covered);
        assert_eq!(result.uncovered_lines, vec![12, 14]);
        assert!((result.coverage_pct - 60.0).abs() < 0.1);
    }

    #[test]
    fn test_source_line_deserialization() {
        let json = r#"[
            {"line": 1, "lineHits": 3},
            {"line": 2, "lineHits": 0},
            {"line": 3},
            {"line": 4, "lineHits": 1, "conditions": 2, "coveredConditions": 1}
        ]"#;
        let lines: Vec<SourceLine> = serde_json::from_str(json).unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].line_hits, Some(3));  // covered
        assert_eq!(lines[1].line_hits, Some(0));  // uncovered
        assert_eq!(lines[2].line_hits, None);      // non-coverable
        assert_eq!(lines[3].conditions, Some(2));
        assert_eq!(lines[3].covered_conditions, Some(1));
    }
}
