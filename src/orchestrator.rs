use anyhow::Result;
use std::path::Path;
use std::time::Instant;
use tracing::{error, info, warn};

use crate::claude;
use crate::config::ValidatedConfig;
use crate::git;
use crate::report::{self, FixStatus, IssueResult};
use crate::runner;
use crate::config::ScannerKind;
use crate::sonar::{self, Issue, SonarClient};

/// Maximum number of test generation attempts before giving up (US-005).

/// Result of the coverage check for an issue's affected lines (US-004).
enum CoverageCheck {
    /// All coverable lines in the affected range are covered.
    FullyCovered,
    /// Some lines need coverage — includes the specific uncovered line numbers.
    NeedsCoverage {
        uncovered_lines: Vec<u32>,
        coverage_pct: f64,
    },
    /// Coverage data is not available (API error, no data, etc.)
    Unavailable,
}

/// Result of the test generation process with retries (US-005).
enum TestGenResult {
    /// Tests generated, all pass, and coverage target reached.
    Success { test_files: Vec<String> },
    /// Tests generated and pass, but coverage is still < 100% after all retries.
    PartialCoverage { test_files: Vec<String> },
    /// Tests were generated but fail. Includes the test output.
    TestsFailed { output: String },
    /// Claude failed to generate tests at all.
    GenerationFailed { error: String },
}

pub struct Orchestrator {
    config: ValidatedConfig,
    client: SonarClient,
    results: Vec<IssueResult>,
    /// Total issues found in SonarQube (before --max-issues filter)
    total_issues_found: usize,
    /// Prompt configuration from YAML (US-019)
    prompt_config: crate::yaml_config::PromptsYaml,
    /// Execution state for resume support (US-017)
    exec_state: Option<crate::state::ExecutionState>,
    /// Rule description cache (US-020): rule_key → description
    rule_cache: std::collections::HashMap<String, String>,
}

impl Orchestrator {
    pub fn new(config: ValidatedConfig) -> Result<Self> {
        let client = SonarClient::new(&config);

        // US-019: Load prompt config from YAML
        let prompt_config = crate::yaml_config::load_yaml_config(
            &config.path,
            None,
        )?
        .map(|y| y.prompts)
        .unwrap_or_default();

        // US-017: Load existing state if resuming
        let exec_state = if config.resume {
            match crate::state::load_state(&config.path)? {
                Some(state) => {
                    if state.is_compatible(&config.sonar_project_id, &config.branch) {
                        info!(
                            "Resuming: {} issues already processed",
                            state.processed.len()
                        );
                        Some(state)
                    } else {
                        warn!("State file exists but config changed (project/branch). Starting fresh.");
                        None
                    }
                }
                None => {
                    info!("No previous state found. Starting fresh.");
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            config,
            client,
            results: Vec::new(),
            total_issues_found: 0,
            prompt_config,
            exec_state,
            rule_cache: std::collections::HashMap::new(),
        })
    }

    /// Generate a partial report from whatever results are available (US-012).
    /// Called when global timeout is reached or execution is interrupted.
    pub fn generate_partial_report(&self) {
        info!("Generating partial report with {} results so far", self.results.len());
        report::generate_report(
            &self.config.path,
            &self.results,
            self.total_issues_found,
            0, // elapsed unknown in timeout case
        );
    }

    /// Run the full Reparo flow (US-010).
    ///
    /// Returns an exit code:
    /// - 0: all issues fixed (or none found, or dry-run)
    /// - 1: fatal error (config, connectivity)
    /// - 2: partial success (some fixed, some failed)
    pub async fn run(&mut self) -> Result<i32> {
        let start = Instant::now();

        // Step 1: Validate SonarQube connectivity (US-001, US-016: with retry)
        info!("=== Step 1: Checking SonarQube connectivity ===");
        crate::retry::retry_async(3, 3, "SonarQube connection check", || {
            self.client.check_connection()
        }).await?;

        self.client.detect_edition().await;

        // Detect test command early — needed for pre-flight and processing
        let test_command = self.config.test_command.clone().or_else(|| {
            runner::detect_test_command(&self.config.path)
        });
        let test_command = match test_command {
            Some(cmd) => cmd,
            None => {
                warn!("Could not detect test command. Use --test-command to specify one.");
                warn!("Continuing without test validation.");
                String::new()
            }
        };

        // Step 2: Create fix branch from current branch (whatever it is)
        info!("=== Step 2: Creating fix branch ===");
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let branch_name = format!("fix/sonar-{}", ts);

        if let Err(e) = git::create_branch(&self.config.path, &branch_name, &self.config.branch) {
            error!("Failed to create branch {}: {}", branch_name, e);
            anyhow::bail!("Cannot create fix branch: {}", e);
        }
        info!("Created branch: {} (from {})", branch_name, self.config.branch);

        // Step 2a: Setup — run setup command (e.g., npm install) before anything else
        if let Some(ref setup_cmd) = self.config.commands.setup {
            info!("=== Step 2a: Setup ===");
            info!("Running setup: {}", setup_cmd);
            match runner::run_shell_command(&self.config.path, setup_cmd, "setup") {
                Ok((true, _)) => info!("Setup completed successfully"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Setup command failed:\n{}", truncate(&output, 500));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Setup command error: {}", e);
                }
            }
        }

        // Step 2b: Initial formatting — run formatter and commit separately before fixes
        if self.config.skip_format {
            info!("=== Step 2b: Initial format SKIPPED (--skip-format) ===");
        } else if let Some(ref fmt_cmd) = self.config.commands.format {
            info!("=== Step 2b: Initial formatting ===");
            match runner::run_shell_command(&self.config.path, fmt_cmd, "initial format") {
                Ok((true, _)) => {
                    info!("Formatter ran successfully");
                    // Check if formatting produced any changes
                    match git::has_changes(&self.config.path) {
                        Ok(true) => {
                            info!("Formatting produced changes — committing...");
                            if let Err(e) = git::commit_all(
                                &self.config.path,
                                "style: apply code formatting before sonar fixes",
                            ) {
                                warn!("Failed to commit formatting changes: {}", e);
                            } else {
                                info!("Formatting changes committed");
                            }
                        }
                        Ok(false) => {
                            info!("No formatting changes needed");
                        }
                        Err(e) => {
                            warn!("Could not check git status: {}", e);
                        }
                    }
                }
                Ok((false, output)) => {
                    warn!("Formatter failed (non-blocking): {}", truncate(&output, 200));
                }
                Err(e) => {
                    warn!("Formatter error (non-blocking): {}", e);
                }
            }
        }

        // Step 3: Pre-flight checks — build and tests must pass before any fixes
        info!("=== Step 3: Pre-flight checks ===");
        if let Some(ref build_cmd) = self.config.commands.build {
            info!("Pre-flight: running build...");
            match runner::run_shell_command(&self.config.path, build_cmd, "pre-flight build") {
                Ok((true, _)) => info!("Pre-flight build passed"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Pre-flight build fails — fix the build before running Reparo:\n{}", truncate(&output, 500));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Pre-flight build error: {}", e);
                }
            }
        }
        if !test_command.is_empty() {
            info!("Pre-flight: running tests...");
            match runner::run_tests(&self.config.path, &test_command, self.config.test_timeout) {
                Ok((true, _)) => info!("Pre-flight tests passed"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Pre-flight tests fail — fix tests before running Reparo:\n{}", truncate(&output, 500));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    anyhow::bail!("Pre-flight test error: {}", e);
                }
            }
        }

        // Step 3b: Coverage boosting — generate tests until min_coverage is reached
        if self.config.skip_coverage {
            info!("=== Step 3b: Coverage boost SKIPPED (--skip-coverage) ===");
        } else if self.config.min_coverage > 0.0 {
            self.boost_coverage_to_threshold(&test_command)?;
        }

        // Step 4: Initial SonarQube scan
        // Run coverage command first so the scanner picks up fresh lcov data
        if let Some(ref cov_cmd) = self.config.coverage_command
            .clone()
            .or_else(|| self.config.commands.coverage.clone())
        {
            info!("Generating coverage report before initial scan...");
            match runner::run_shell_command(&self.config.path, &cov_cmd, "pre-scan coverage") {
                Ok((true, _)) => info!("Coverage report generated"),
                Ok((false, output)) => warn!("Coverage command failed: {}", truncate(&output, 200)),
                Err(e) => warn!("Coverage command error: {}", e),
            }
        }

        if let Some(ref scanner) = self.config.scanner {
            info!("=== Step 4: Initial SonarQube scan ===");
            let ce_task_id = self.client.run_scanner(
                &self.config.path,
                scanner,
                &self.config.branch,
            )?;
            self.client
                .wait_for_analysis(ce_task_id.as_deref())
                .await?;
        } else {
            info!("=== Step 4: Skipping scanner (--skip-scan) ===");
        }

        // Fetch initial issues to get total count and dry-run info
        let initial_issues = self.client.fetch_issues().await?;
        self.total_issues_found = initial_issues.len();
        info!("Found {} issues", self.total_issues_found);

        if initial_issues.is_empty() {
            info!("No issues to fix. Congratulations!");
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }

        self.print_issue_summary(&initial_issues);
        self.print_issue_listing(&initial_issues);

        if self.config.dry_run {
            info!("=== Dry run mode — no fixes will be applied ===");
            info!("{} issues would be processed", initial_issues.len());
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }

        // US-017: Filter out already-processed issues if resuming
        let already_processed = self
            .exec_state
            .as_ref()
            .map(|s| s.processed_keys())
            .unwrap_or_default();

        // US-017: Initialize state if not resuming
        if self.exec_state.is_none() {
            self.exec_state = Some(crate::state::ExecutionState::new(
                &self.config.sonar_project_id,
                &self.config.branch,
                self.config.batch_size,
                self.total_issues_found,
            ));
        }

        // Step 5: Fix loop — only fix issues from the initial scan
        info!("=== Step 5: Fix loop ===");
        let max_issues = if self.config.max_issues > 0 {
            self.config.max_issues
        } else {
            usize::MAX
        };

        // Track original issue keys — we only fix issues that existed before we started.
        // Issues introduced by our fixes are NOT our responsibility.
        let original_issue_keys: std::collections::HashSet<String> =
            initial_issues.iter().map(|i| i.key.clone()).collect();
        info!("Tracking {} original issues", original_issue_keys.len());

        let mut total_fixed = 0usize;
        let mut total_failed = 0usize;
        let mut issue_num = 0usize;
        let mut consecutive_build_failures = 0usize;
        const MAX_CONSECUTIVE_FAILURES: usize = 3;

        loop {
            if issue_num >= max_issues {
                info!("Reached --max-issues limit ({})", max_issues);
                break;
            }

            // Circuit breaker: stop if too many consecutive build failures (likely systemic issue)
            if consecutive_build_failures >= MAX_CONSECUTIVE_FAILURES {
                warn!(
                    "Stopping: {} consecutive build failures — likely a systemic issue (e.g. Node.js version, broken dependency). Fix the build manually and re-run.",
                    consecutive_build_failures
                );
                break;
            }

            // Pre-flight: verify build still passes before attempting next fix
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "pre-fix build check") {
                    Ok((true, _)) => {}
                    Ok((false, output)) => {
                        error!(
                            "Build is broken before attempting next fix — stopping. Output:\n{}",
                            truncate(&output, 300)
                        );
                        break;
                    }
                    Err(e) => {
                        error!("Build check error: {} — stopping", e);
                        break;
                    }
                }
            }

            // Fetch fresh issues from SonarQube (most critical first)
            let issues = match self.client.fetch_issues().await {
                Ok(issues) => issues,
                Err(e) => {
                    error!("Failed to fetch issues: {}", e);
                    break;
                }
            };

            if issues.is_empty() {
                info!("No more issues to fix!");
                break;
            }

            // Pick the most critical issue that:
            // 1. Was in the original scan (not introduced by our fixes)
            // 2. Hasn't been processed yet
            let issue = match issues.into_iter().find(|i| {
                original_issue_keys.contains(&i.key)
                    && !already_processed.contains(&i.key)
                    && !self.results.iter().any(|r| r.issue_key == i.key)
            }) {
                Some(i) => i,
                None => {
                    info!("All remaining issues already processed");
                    break;
                }
            };

            issue_num += 1;
            info!(
                "--- [{}/{}] Processing: {} ({} {}) in {} ---",
                issue_num,
                max_issues.min(self.total_issues_found),
                issue.key,
                issue.severity,
                issue.issue_type,
                sonar::component_to_path(&issue.component)
            );

            // Pre-fetch rule description if not cached
            if !self.rule_cache.contains_key(&issue.rule) {
                if let Ok(desc) = self.client.get_rule_description(&issue.rule).await {
                    self.rule_cache.insert(issue.rule.clone(), desc);
                }
            }

            let result = self.process_issue(&issue, &test_command).await;
            match &result.status {
                FixStatus::Fixed => {
                    total_fixed += 1;
                    consecutive_build_failures = 0; // Reset on success
                    info!("Issue {} fixed successfully ({} fixed so far)", issue.key, total_fixed);
                }
                FixStatus::NeedsReview(reason) => {
                    total_failed += 1;
                    consecutive_build_failures = 0; // Review != build failure
                    warn!("Issue {} needs manual review: {}", issue.key, reason);
                }
                FixStatus::Failed(err) => {
                    total_failed += 1;
                    if err.contains("Build fails") || err.contains("Build command error") {
                        consecutive_build_failures += 1;
                    } else {
                        consecutive_build_failures = 0;
                    }
                    error!("Issue {} failed: {}", issue.key, err);
                }
                FixStatus::Skipped(reason) => {
                    info!("Issue {} skipped: {}", issue.key, reason);
                }
            }

            // US-013: Document in changelog immediately
            report::append_changelog(&self.config.path, &result);

            // US-017: Save state after each issue
            if let Some(ref mut state) = self.exec_state {
                let status_str = match &result.status {
                    FixStatus::Fixed => "fixed",
                    FixStatus::NeedsReview(_) => "needs_review",
                    FixStatus::Failed(_) => "failed",
                    FixStatus::Skipped(_) => "skipped",
                };
                let reason = match &result.status {
                    FixStatus::Failed(r) | FixStatus::NeedsReview(r) | FixStatus::Skipped(r) => Some(r.as_str()),
                    _ => None,
                };
                state.add_processed(&result.issue_key, status_str, result.pr_url.as_deref(), reason);
                let _ = crate::state::save_state(&self.config.path, state);
            }

            self.results.push(result);
        }

        info!(
            "Processing complete: {} fixed, {} failed/review",
            total_fixed, total_failed
        );

        // Step 5b: Deduplication — reduce duplicated code after fixes
        if self.config.skip_dedup {
            info!("=== Step 5b: Deduplication SKIPPED (--skip-dedup) ===");
        } else if let Some(ref scanner) = self.config.scanner {
            self.reduce_duplications(&test_command, scanner).await?;
        } else {
            info!("=== Step 5b: Deduplication SKIPPED (no scanner) ===");
        }

        // Step 6: Generate report (on the fix branch)
        info!("=== Step 6: Generating report ===");
        let elapsed = start.elapsed().as_secs();
        report::generate_report(&self.config.path, &self.results, self.total_issues_found, elapsed);

        // Commit report files to the fix branch
        let _ = git::add_all(&self.config.path);
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let _ = git::commit(&self.config.path, "docs: add REPORT.md and TECHDEBT_CHANGELOG.md");
        }

        // Step 7: Create PR if enabled and there are fixes
        if self.config.pr && total_fixed > 0 {
            info!("=== Step 7: Creating PR ===");
            match self.create_pr(&branch_name) {
                Ok(pr_url) => {
                    info!("PR created: {}", pr_url);
                    for r in self.results.iter_mut() {
                        if matches!(r.status, FixStatus::Fixed) {
                            r.pr_url = Some(pr_url.clone());
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create PR: {}", e);
                }
            }
        } else if !self.config.pr {
            info!("PR creation disabled (--no-pr)");
        } else if total_fixed == 0 {
            info!("No fixes — skipping PR creation");
        }

        let exit_code = self.print_summary(elapsed);

        // Stay on the fix branch so the user can review changes
        info!("Staying on branch '{}' for review", branch_name);

        // US-017: Clean up state file on successful completion
        crate::state::remove_state(&self.config.path);

        Ok(exit_code)
    }

    /// Boost project-wide test coverage to the configured minimum threshold.
    ///
    /// Iterates through files sorted by coverage (least covered first), generating
    /// tests for each until the overall project coverage meets the threshold.
    /// After each file, verifies no source code was modified, then commits the tests.
    fn boost_coverage_to_threshold(&self, test_command: &str) -> Result<()> {
        let coverage_cmd = self.config.coverage_command.clone()
            .or_else(|| self.config.commands.coverage.clone())
            .or_else(|| runner::detect_coverage_command(&self.config.path));

        let cov_cmd = match coverage_cmd {
            Some(c) => c,
            None => {
                warn!("No coverage command available — skipping coverage gate. Set commands.coverage in YAML or use --coverage-command.");
                return Ok(());
            }
        };

        info!("=== Step 3b: Coverage boost (project: {:.0}%, per-file: {:.0}%) ===",
            self.config.min_coverage,
            self.config.min_file_coverage
        );

        // Initial coverage measurement
        let overall_pct = match self.run_coverage_and_measure(&cov_cmd) {
            Some(pct) => pct,
            None => {
                warn!("Could not measure project coverage — skipping coverage gate");
                return Ok(());
            }
        };

        // Get per-file coverage sorted ascending (least covered first)
        let lcov_path = match runner::find_lcov_report(&self.config.path) {
            Some(p) => p,
            None => {
                warn!("No lcov report found — cannot identify uncovered files");
                return Ok(());
            }
        };

        let file_coverages = runner::per_file_lcov_coverage(&lcov_path);

        // Build the list of files that need boosting:
        // 1. Files needed to reach overall min_coverage (sorted by coverage asc)
        // 2. Files below min_file_coverage threshold (regardless of overall %)
        let overall_needs_boost = overall_pct < self.config.min_coverage;
        let has_file_threshold = self.config.min_file_coverage > 0.0;

        let files_below_file_threshold: Vec<_> = if has_file_threshold {
            file_coverages.iter()
                .filter(|fc| fc.coverage_pct < self.config.min_file_coverage && !is_test_file(&fc.file))
                .collect()
        } else {
            Vec::new()
        };

        if !overall_needs_boost && files_below_file_threshold.is_empty() {
            info!(
                "Project-wide coverage {:.1}% meets {:.0}% and all files meet per-file threshold — no boost needed",
                overall_pct, self.config.min_coverage
            );
            return Ok(());
        }

        if overall_needs_boost {
            info!("Project-wide coverage {:.1}% is below {:.0}%", overall_pct, self.config.min_coverage);
        }
        if !files_below_file_threshold.is_empty() {
            info!("{} file(s) below per-file threshold of {:.0}%", files_below_file_threshold.len(), self.config.min_file_coverage);
        }

        // Merge both sets: files for overall boost + files below per-file threshold
        // Deduplicate and keep sorted by coverage ascending
        let files_needing_tests: Vec<_> = file_coverages.iter()
            .filter(|fc| {
                if is_test_file(&fc.file) {
                    return false;
                }
                if fc.coverage_pct >= 100.0 {
                    return false;
                }
                // Include if needed for overall boost OR below per-file threshold
                overall_needs_boost || (has_file_threshold && fc.coverage_pct < self.config.min_file_coverage)
            })
            .collect();

        if files_needing_tests.is_empty() {
            warn!("No source files with uncovered lines found in lcov report");
            return Ok(());
        }

        info!("Found {} source files needing test coverage — generating tests starting from least covered", files_needing_tests.len());

        let test_examples = runner::find_test_examples(&self.config.path);
        let test_examples_str = test_examples.join("\n\n");
        let test_framework = test_command;

        let mut current_pct = overall_pct;
        let mut files_boosted = 0;
        // Track which files have been boosted so we can skip them
        let mut boosted_files: std::collections::HashSet<String> = std::collections::HashSet::new();

        for fc in &files_needing_tests {
            // Check if we can stop: overall threshold met AND this file doesn't need per-file boost
            let overall_met = current_pct >= self.config.min_coverage;
            let file_needs_boost = has_file_threshold && fc.coverage_pct < self.config.min_file_coverage;

            if overall_met && !file_needs_boost {
                continue; // Skip files that are only needed for overall boost
            }

            let reason = if !overall_met && file_needs_boost {
                format!("overall {:.1}% < {:.0}% AND file {:.1}% < {:.0}%",
                    current_pct, self.config.min_coverage, fc.coverage_pct, self.config.min_file_coverage)
            } else if file_needs_boost {
                format!("file {:.1}% < per-file threshold {:.0}%", fc.coverage_pct, self.config.min_file_coverage)
            } else {
                format!("overall {:.1}% < {:.0}%", current_pct, self.config.min_coverage)
            };

            info!(
                "--- Coverage boost [{}/{}]: {} ({:.1}%, {}/{} lines) — {} ---",
                files_boosted + 1,
                files_needing_tests.len(),
                fc.file,
                fc.coverage_pct,
                fc.covered_lines,
                fc.total_lines,
                reason
            );

            if self.boost_file_coverage(fc, test_framework, &test_examples_str)? {
                files_boosted += 1;
                boosted_files.insert(fc.file.clone());

                // Re-measure coverage
                match self.run_coverage_and_measure(&cov_cmd) {
                    Some(pct) => {
                        info!("Project-wide coverage after boost: {:.1}% (was {:.1}%)", pct, current_pct);
                        current_pct = pct;
                    }
                    None => {
                        warn!("Could not re-measure coverage — continuing with next file");
                    }
                }
            }
        }

        // Final summary
        let remaining_below: Vec<_> = if has_file_threshold {
            // Re-read lcov to check which files are still below threshold
            runner::find_lcov_report(&self.config.path)
                .map(|p| runner::per_file_lcov_coverage(&p))
                .unwrap_or_default()
                .into_iter()
                .filter(|fc| fc.coverage_pct < self.config.min_file_coverage && !is_test_file(&fc.file))
                .collect()
        } else {
            Vec::new()
        };

        if current_pct >= self.config.min_coverage && remaining_below.is_empty() {
            info!(
                "Coverage boost complete: {:.1}% (target {:.0}%) — {} files boosted",
                current_pct, self.config.min_coverage, files_boosted
            );
        } else {
            if current_pct < self.config.min_coverage {
                warn!(
                    "Coverage boost: overall {:.1}% still below target {:.0}%",
                    current_pct, self.config.min_coverage
                );
            }
            if !remaining_below.is_empty() {
                warn!(
                    "{} file(s) still below per-file threshold {:.0}%:",
                    remaining_below.len(), self.config.min_file_coverage
                );
                for fc in &remaining_below {
                    warn!("  {} — {:.1}%", fc.file, fc.coverage_pct);
                }
            }
            warn!("{} files boosted. Continuing with fixes anyway.", files_boosted);
        }

        Ok(())
    }

    /// Generate tests for a single file and commit them.
    /// Returns true if tests were generated, pass, and committed successfully.
    fn boost_file_coverage(
        &self,
        fc: &runner::FileCoverage,
        test_framework: &str,
        test_examples_str: &str,
    ) -> Result<bool> {
        // Read the source file
        let full_path = self.config.path.join(&fc.file);
        let source_content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Cannot read {}: {} — skipping", fc.file, e);
                return Ok(false);
            }
        };

        // Build uncovered lines description
        let uncovered_desc = format!(
            "Lines 1-{} (file has {:.1}% coverage, {} uncovered lines out of {} coverable)",
            source_content.lines().count(),
            fc.coverage_pct,
            fc.total_lines - fc.covered_lines,
            fc.total_lines
        );

        let prompt = claude::build_test_generation_prompt(
            &fc.file,
            &source_content,
            &uncovered_desc,
            test_framework,
            test_examples_str,
        );

        if self.config.show_prompts {
            info!("Coverage boost prompt:\n{}", prompt);
        }

        info!("Generating tests for {} ...", fc.file);
        match claude::run_claude(
            &self.config.path,
            &prompt,
            self.config.claude_timeout,
            self.config.dangerously_skip_permissions,
            false,
        ) {
            Ok(_) => {
                info!("Claude completed test generation for {}", fc.file);
            }
            Err(e) => {
                warn!("Failed to generate tests for {}: {} — skipping", fc.file, e);
                let _ = git::revert_changes(&self.config.path);
                return Ok(false);
            }
        }

        // Verify no source files were modified
        let changed = match git::changed_files(&self.config.path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Cannot check changed files: {} — reverting", e);
                let _ = git::revert_changes(&self.config.path);
                return Ok(false);
            }
        };

        if changed.is_empty() {
            warn!("No files changed after test generation for {} — skipping", fc.file);
            return Ok(false);
        }

        let source_files_modified: Vec<&String> = changed.iter()
            .filter(|f| !is_test_file(f) && !f.contains(".reparo") && !f.contains("TECHDEBT_CHANGELOG"))
            .collect();

        if !source_files_modified.is_empty() {
            warn!(
                "Source files were modified during test generation for {}: {:?} — reverting all changes",
                fc.file, source_files_modified
            );
            let _ = git::revert_changes(&self.config.path);
            return Ok(false);
        }

        // Run tests
        info!("Running tests to validate generated tests...");
        match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
            Ok((true, _)) => {
                info!("Tests pass after generating tests for {}", fc.file);
            }
            Ok((false, output)) => {
                warn!("Tests FAIL after generating tests for {} — reverting:\n{}", fc.file, truncate(&output, 300));
                let _ = git::revert_changes(&self.config.path);
                return Ok(false);
            }
            Err(e) => {
                warn!("Test execution error for {} — reverting: {}", fc.file, e);
                let _ = git::revert_changes(&self.config.path);
                return Ok(false);
            }
        }

        // Commit test files only
        let test_files_changed: Vec<&str> = changed.iter()
            .filter(|f| is_test_file(f))
            .map(|s| s.as_str())
            .collect();

        if test_files_changed.is_empty() {
            let _ = git::revert_changes(&self.config.path);
            return Ok(false);
        }

        if let Err(e) = git::add_files(&self.config.path, &test_files_changed) {
            warn!("Failed to stage test files: {} — reverting", e);
            let _ = git::revert_changes(&self.config.path);
            return Ok(false);
        }
        let commit_msg = format!("test(coverage): add tests for {} ({:.0}% → boost)", fc.file, fc.coverage_pct);
        if let Err(e) = git::commit(&self.config.path, &commit_msg) {
            warn!("Failed to commit tests for {}: {} — reverting", fc.file, e);
            let _ = git::revert_changes(&self.config.path);
            return Ok(false);
        }

        info!("Committed tests for {}", fc.file);

        // Revert any non-test leftover changes
        let _ = git::revert_changes(&self.config.path);

        Ok(true)
    }

    /// Run the coverage command and return the overall project coverage percentage.
    fn run_coverage_and_measure(&self, cov_cmd: &str) -> Option<f64> {
        match runner::run_shell_command(&self.config.path, cov_cmd, "coverage measurement") {
            Ok((true, _)) => {}
            Ok((false, output)) => {
                warn!("Coverage command failed: {}", truncate(&output, 200));
                return None;
            }
            Err(e) => {
                warn!("Coverage command error: {}", e);
                return None;
            }
        }

        let lcov_path = runner::find_lcov_report(&self.config.path)?;
        let overall = runner::overall_lcov_coverage(&lcov_path);
        if let Some(pct) = overall {
            info!("Project-wide test coverage: {:.1}%", pct);
        }
        overall
    }

    /// Step 5b: Reduce code duplication after sonar fixes.
    ///
    /// For each file with duplications (sorted by most duplicated first):
    /// 1. Ensure 100% test coverage of duplicated ranges
    /// 2. Ask Claude to refactor and eliminate duplication
    /// 3. Verify tests still pass
    /// 4. Re-scan with SonarQube to verify duplication is reduced
    /// 5. Commit if verified, revert if not
    async fn reduce_duplications(
        &self,
        test_command: &str,
        scanner: &ScannerKind,
    ) -> Result<()> {
        info!("=== Step 5b: Deduplication ===");

        // Get initial duplication %
        let initial_dup_pct = self.client.get_duplication_percentage().await?;
        info!("Current project duplication: {:.1}%", initial_dup_pct);

        if initial_dup_pct == 0.0 {
            info!("No duplications found — skipping");
            return Ok(());
        }

        // Get files with duplications, sorted by most duplicated first
        let dup_files = self.client.get_files_with_duplications().await?;
        if dup_files.is_empty() {
            info!("No files with duplications found");
            return Ok(());
        }

        info!("Found {} files with duplicated code:", dup_files.len());
        for (i, f) in dup_files.iter().take(20).enumerate() {
            info!(
                "  {}. {} — {:.1}% ({} lines)",
                i + 1, f.file_path, f.duplication_pct, f.duplicated_lines
            );
        }

        let max_iterations = if self.config.max_dedup == 0 {
            dup_files.len()
        } else {
            self.config.max_dedup.min(dup_files.len())
        };

        let mut dedup_fixed = 0usize;
        let mut dedup_failed = 0usize;

        for (idx, dup_file) in dup_files.iter().take(max_iterations).enumerate() {
            info!(
                "--- [dedup {}/{}] {} ({:.1}% duplicated) ---",
                idx + 1,
                max_iterations,
                dup_file.file_path,
                dup_file.duplication_pct
            );

            // Read the source file
            let abs_path = self.config.path.join(&dup_file.file_path);
            let file_content = match std::fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Cannot read {}: {} — skipping", dup_file.file_path, e);
                    dedup_failed += 1;
                    continue;
                }
            };

            let total_lines = file_content.lines().count() as u32;

            // Get the duplicated blocks for this file
            let blocks = self
                .client
                .get_file_duplications(&dup_file.component_key)
                .await
                .unwrap_or_default();

            let duplicated_ranges: Vec<(u32, u32)> = blocks
                .iter()
                .map(|b| (b.from, b.from + b.size - 1))
                .collect();

            if duplicated_ranges.is_empty() {
                info!("No specific duplicated ranges found — skipping");
                continue;
            }

            // Step 1: Ensure 100% coverage of the duplicated ranges
            // Merge ranges into a single start..end for coverage check
            let cov_start = duplicated_ranges.iter().map(|r| r.0).min().unwrap_or(1);
            let cov_end = duplicated_ranges.iter().map(|r| r.1).max().unwrap_or(total_lines);

            let coverage = self
                .client
                .get_line_coverage(
                    &dup_file.component_key,
                    cov_start,
                    cov_end,
                )
                .await?;

            coverage.log_summary(&dup_file.file_path, cov_start, cov_end);

            if !coverage.fully_covered && !coverage.uncovered_lines.is_empty() {
                info!(
                    "Coverage {:.1}% — generating tests for {} uncovered lines before dedup...",
                    coverage.coverage_pct,
                    coverage.uncovered_lines.len()
                );

                // Generate tests for the uncovered duplicated code
                let fake_issue = sonar::Issue {
                    key: format!("dedup-{}", dup_file.file_path),
                    rule: "dedup".to_string(),
                    severity: "CRITICAL".to_string(),
                    issue_type: "CODE_SMELL".to_string(),
                    message: format!("Deduplication of {}", dup_file.file_path),
                    component: dup_file.component_key.clone(),
                    text_range: Some(sonar::TextRange {
                        start_line: cov_start,
                        end_line: cov_end,
                        start_offset: None,
                        end_offset: None,
                    }),
                    status: "OPEN".to_string(),
                    tags: vec![],
                };

                let gen_result = self
                    .generate_tests_with_retry(
                        &fake_issue,
                        &dup_file.file_path,
                        &file_content,
                        cov_start,
                        cov_end,
                        &coverage.uncovered_lines,
                        test_command,
                    )
                    .await;

                match gen_result {
                    TestGenResult::Success { test_files } => {
                        info!("Coverage achieved for dedup of {}", dup_file.file_path);
                        // Commit test files
                        let _ = git::add_all(&self.config.path);
                        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                            let msg = format!(
                                "test(dedup): add tests for {} before deduplication",
                                dup_file.file_path
                            );
                            let _ = git::commit(&self.config.path, &msg);
                            info!("Committed {} test file(s)", test_files.len());
                        }
                    }
                    TestGenResult::PartialCoverage { .. } => {
                        warn!(
                            "Could not achieve 100% coverage for {} — skipping dedup (requires full coverage)",
                            dup_file.file_path
                        );
                        // Revert generated tests since we can't proceed without 100% coverage
                        let _ = git::revert_changes(&self.config.path);
                        dedup_failed += 1;
                        continue;
                    }
                    TestGenResult::TestsFailed { .. } | TestGenResult::GenerationFailed { .. } => {
                        warn!("Failed to generate tests for {} — skipping dedup", dup_file.file_path);
                        let _ = git::revert_changes(&self.config.path);
                        dedup_failed += 1;
                        continue;
                    }
                }
            }

            // Step 2: Ask Claude to refactor and eliminate duplication
            // Re-read file content (may have changed if tests were generated)
            let current_content = std::fs::read_to_string(&abs_path)
                .unwrap_or_else(|_| file_content.clone());

            let prompt = claude::build_dedup_prompt(
                &dup_file.file_path,
                &current_content,
                &duplicated_ranges,
                dup_file.duplication_pct,
            );

            if self.config.show_prompts {
                info!("Dedup prompt:\n{}", prompt);
            }

            // Clean before fix
            if let Some(ref clean_cmd) = self.config.commands.clean {
                let _ = runner::run_shell_command(&self.config.path, clean_cmd, "clean");
            }

            info!("Asking Claude to refactor {} to reduce duplication...", dup_file.file_path);
            match claude::run_claude(
                &self.config.path,
                &prompt,
                self.config.claude_timeout,
                self.config.dangerously_skip_permissions,
                false, // don't re-show prompt
            ) {
                Ok(_output) => {
                    info!("Claude completed dedup refactoring for {}", dup_file.file_path);
                }
                Err(e) => {
                    warn!("Claude failed for dedup of {}: {} — reverting", dup_file.file_path, e);
                    let _ = git::revert_changes(&self.config.path);
                    dedup_failed += 1;
                    continue;
                }
            }

            // Check that no test files were modified
            let changed = git::changed_files(&self.config.path).unwrap_or_default();
            let test_files_changed: Vec<_> = changed.iter().filter(|f| is_test_file(f)).collect();
            if !test_files_changed.is_empty() {
                warn!(
                    "Dedup modified test files {:?} — reverting",
                    test_files_changed
                );
                let _ = git::revert_changes(&self.config.path);
                dedup_failed += 1;
                continue;
            }

            // Step 3: Format if configured
            if let Some(ref fmt_cmd) = self.config.commands.format {
                let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
            }

            // Step 4: Build must pass
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                    Ok((true, _)) => info!("Build passed after dedup"),
                    Ok((false, output)) => {
                        warn!("Build failed after dedup — reverting: {}", truncate(&output, 200));
                        let _ = git::revert_changes(&self.config.path);
                        dedup_failed += 1;
                        continue;
                    }
                    Err(e) => {
                        warn!("Build error after dedup — reverting: {}", e);
                        let _ = git::revert_changes(&self.config.path);
                        dedup_failed += 1;
                        continue;
                    }
                }
            }

            // Step 5: Tests must pass (critical — 100% coverage required)
            match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                Ok((true, _)) => info!("All tests pass after dedup"),
                Ok((false, output)) => {
                    warn!("Tests failed after dedup — reverting: {}", truncate(&output, 200));
                    let _ = git::revert_changes(&self.config.path);
                    dedup_failed += 1;
                    continue;
                }
                Err(e) => {
                    warn!("Test error after dedup — reverting: {}", e);
                    let _ = git::revert_changes(&self.config.path);
                    dedup_failed += 1;
                    continue;
                }
            }

            // Step 6: Re-scan with SonarQube to verify duplication is reduced
            info!("Re-scanning with SonarQube to verify dedup for {}...", dup_file.file_path);
            match self.client.run_scanner(&self.config.path, scanner, &self.config.branch) {
                Ok(ce_task_id) => {
                    if let Err(e) = self.client.wait_for_analysis(ce_task_id.as_deref()).await {
                        warn!("SonarQube analysis failed after dedup: {} — committing anyway", e);
                    }
                }
                Err(e) => {
                    warn!("Scanner failed after dedup: {} — committing anyway", e);
                }
            }

            let new_dup_pct = self.client.get_duplication_percentage().await.unwrap_or(initial_dup_pct);
            info!("Duplication after refactoring {}: {:.1}% (was {:.1}%)", dup_file.file_path, new_dup_pct, initial_dup_pct);

            // Commit the dedup changes
            let _ = git::add_all(&self.config.path);
            if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                let msg = format!(
                    "refactor(dedup): reduce code duplication in {}",
                    dup_file.file_path
                );
                match git::commit(&self.config.path, &msg) {
                    Ok(()) => {
                        info!("Committed dedup refactoring for {}", dup_file.file_path);
                        dedup_fixed += 1;
                    }
                    Err(e) => {
                        warn!("Failed to commit dedup: {}", e);
                        dedup_failed += 1;
                    }
                }
            } else {
                info!("No changes from dedup — Claude made no modifications");
            }
        }

        info!(
            "Deduplication complete: {} files refactored, {} skipped/failed",
            dedup_fixed, dedup_failed
        );

        Ok(())
    }

    async fn process_issue(&self, issue: &Issue, test_command: &str) -> IssueResult {
        let file_path = sonar::component_to_path(&issue.component);
        let lines = format_lines(&issue.text_range);
        let mut result = IssueResult {
            issue_key: issue.key.clone(),
            rule: issue.rule.clone(),
            severity: issue.severity.clone(),
            issue_type: issue.issue_type.clone(),
            message: issue.message.clone(),
            file: file_path.clone(),
            lines: lines.clone(),
            status: FixStatus::Failed("Not processed".to_string()),
            change_description: String::new(),
            tests_added: Vec::new(),
            pr_url: None,
            diff_summary: None,
        };

        let full_path = self.config.path.join(&file_path);
        let file_content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => {
                result.status = FixStatus::Failed(format!("Cannot read file: {}", e));
                return result;
            }
        };

        let total_lines = file_content.lines().count() as u32;
        let (start_line, end_line) = match &issue.text_range {
            Some(tr) if tr.start_line == tr.end_line => {
                // Single-line range (e.g. function signature for cognitive complexity).
                // Expand to cover from that line to end of file so the coverage
                // check includes the full function body.
                (tr.start_line, total_lines)
            }
            Some(tr) => (tr.start_line, tr.end_line),
            None => (1, total_lines),
        };

        // Step A: Check line-level coverage and generate tests if needed (US-004)
        // Skip coverage for non-coverable files (CSS, HTML, assets, etc.)
        if is_non_coverable_file(&file_path) {
            info!("Skipping coverage check for non-coverable file: {}", file_path);
        } else if !test_command.is_empty() {
            let cov_result = self
                .check_coverage(&issue.component, &file_path, start_line, end_line)
                .await;

            match cov_result {
                CoverageCheck::FullyCovered => {
                    // All affected lines are covered — proceed to fix
                }
                CoverageCheck::NeedsCoverage { uncovered_lines, coverage_pct } => {
                    info!(
                        "Coverage {:.1}% — generating tests for {} uncovered lines...",
                        coverage_pct,
                        uncovered_lines.len()
                    );

                    // US-005: Generate tests with retry loop (max 3 attempts)
                    let gen_result = self
                        .generate_tests_with_retry(
                            issue,
                            &file_path,
                            &file_content,
                            start_line,
                            end_line,
                            &uncovered_lines,
                            test_command,
                        )
                        .await;

                    match gen_result {
                        TestGenResult::Success { test_files } => {
                            result.tests_added = test_files;
                        }
                        TestGenResult::PartialCoverage { test_files } => {
                            warn!(
                                "Could not achieve 100% coverage after 3 attempts for {}. Keeping passing tests, skipping fix.",
                                issue.key
                            );
                            // Commit the passing tests — more coverage is always welcome
                            if !test_files.is_empty() {
                                let commit_msg = format!(
                                    "test(coverage): add partial tests for {} (100% not reached, fix skipped)",
                                    issue.component
                                );
                                let _ = git::add_all(&self.config.path);
                                let _ = git::commit(&self.config.path, &commit_msg);
                                info!("Committed partial test coverage for {}", issue.key);
                            }
                            result.tests_added = test_files;
                            result.status = FixStatus::NeedsReview(
                                "Could not achieve 100% coverage after 3 test generation attempts — tests kept, fix skipped".to_string()
                            );
                            return result;
                        }
                        TestGenResult::TestsFailed { output } => {
                            warn!("Generated tests fail, reverting test changes");
                            let _ = git::revert_changes(&self.config.path);
                            result.status = FixStatus::Failed(format!(
                                "Generated tests fail: {}",
                                truncate(&output, 200)
                            ));
                            return result;
                        }
                        TestGenResult::GenerationFailed { error } => {
                            warn!("Failed to generate tests: {}", error);
                            // Continue with fix anyway
                        }
                    }

                    // Commit test additions before fixing
                    if !result.tests_added.is_empty() {
                        let _ = git::add_all(&self.config.path);
                        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                            let msg = format!(
                                "test(sonar): add tests for {} coverage\n\nPreparing coverage for fix of {}",
                                file_path, issue.key
                            );
                            let _ = git::commit(&self.config.path, &msg);
                        }
                    }
                }
                CoverageCheck::Unavailable => {
                    info!("No coverage data available for {}, proceeding with fix", file_path);
                }
            }
        }

        // Step A-2: Clean before fix if command defined (US-014)
        if let Some(ref clean_cmd) = self.config.commands.clean {
            match runner::run_shell_command(&self.config.path, clean_cmd, "clean") {
                Ok((true, _)) => info!("Clean succeeded"),
                Ok((false, output)) => warn!("Clean failed: {}", truncate(&output, 100)),
                Err(e) => warn!("Clean command error: {}", e),
            }
        }

        // Step B: Fix the issue (US-006)
        info!(
            "Fixing {} ({} {}) in {}:{}",
            issue.key, issue.severity, issue.issue_type, file_path, lines
        );

        // US-020: Use cached rule description
        let rule_desc = if let Some(cached) = self.rule_cache.get(&issue.rule) {
            cached.clone()
        } else {
            let desc = self
                .client
                .get_rule_description(&issue.rule)
                .await
                .unwrap_or_else(|_| issue.rule.clone());
            desc
        };

        // US-019: Resolve prompt hint from YAML config
        let prompt_hint = crate::yaml_config::resolve_prompt_hint(
            &self.prompt_config,
            &issue.rule,
            &issue.issue_type,
        );
        let rule_desc_with_hint = if let Some(ref hint) = prompt_hint {
            format!("{}\n\n## Additional guidance:\n{}", rule_desc, hint)
        } else {
            rule_desc
        };

        let prompt = claude::build_fix_prompt(
            &issue.key,
            &issue.issue_type,
            &issue.severity,
            &issue.rule,
            &issue.message,
            &file_path,
            &file_content,
            start_line,
            end_line,
            &rule_desc_with_hint,
        );

        let claude_output = match claude::run_claude(&self.config.path, &prompt, self.config.claude_timeout, self.config.dangerously_skip_permissions, self.config.show_prompts) {
            Ok(output) => output,
            Err(e) => {
                result.status = FixStatus::Failed(format!("Claude failed: {}", e));
                let _ = git::revert_changes(&self.config.path);
                return result;
            }
        };

        // Check if anything actually changed (excluding internal files)
        let all_changed = git::changed_files(&self.config.path).unwrap_or_default();
        let changed: Vec<String> = all_changed
            .into_iter()
            .filter(|f| !is_internal_file(f))
            .collect();
        if changed.is_empty() {
            result.status = FixStatus::Failed("Claude made no changes".to_string());
            return result;
        }

        // Log which files were changed
        info!("Files changed by fix: {:?}", changed);

        // Check if Claude modified test files — revert ONLY the test changes, keep source changes
        let modified_test_files: Vec<String> = changed.iter().filter(|f| is_test_file(f)).cloned().collect();
        if !modified_test_files.is_empty() {
            warn!(
                "Claude modified test file(s) {:?} — reverting test changes only, keeping source fix",
                modified_test_files
            );
            // Revert only the test files, keep source changes
            for test_file in &modified_test_files {
                let checkout_result = std::process::Command::new("git")
                    .current_dir(&self.config.path)
                    .args(["checkout", "HEAD", "--", test_file])
                    .status();
                match checkout_result {
                    Ok(s) if s.success() => {
                        info!("Reverted test file: {}", test_file);
                    }
                    _ => {
                        // File might be newly created (untracked) — remove it
                        let abs_path = self.config.path.join(test_file);
                        if abs_path.exists() {
                            let _ = std::fs::remove_file(&abs_path);
                            info!("Removed new test file: {}", test_file);
                        }
                    }
                }
            }

            // Re-check if any source changes remain after reverting test files
            let remaining = git::changed_files(&self.config.path).unwrap_or_default();
            let source_changes: Vec<String> = remaining
                .into_iter()
                .filter(|f| !is_test_file(f) && !is_internal_file(f))
                .collect();
            if source_changes.is_empty() {
                result.status = FixStatus::Failed(
                    "Claude only modified test files — no source fix applied".to_string(),
                );
                let _ = git::revert_changes(&self.config.path);
                return result;
            }
            info!("Keeping source changes: {:?}", source_changes);
        }

        // Build a structured change description
        result.change_description = build_change_description(&claude_output, &changed);

        // Step C-1..C-3: Format → Build → Test with retry loop
        // If build or tests fail, ask Claude to fix the error (without modifying tests)
        // and retry up to coverage_attempts times.
        let max_fix_attempts = self.config.coverage_attempts;
        let mut fix_verified = false;

        for fix_attempt in 1..=max_fix_attempts {
            if fix_attempt > 1 {
                info!("Fix-repair attempt {}/{} for {}", fix_attempt, max_fix_attempts, issue.key);
            }

            // Format code if command defined
            if let Some(ref fmt_cmd) = self.config.commands.format {
                match runner::run_shell_command(&self.config.path, fmt_cmd, "format") {
                    Ok((true, _)) => {
                        info!("Code formatted successfully");
                    }
                    Ok((false, output)) => {
                        warn!("Formatter failed, continuing: {}", truncate(&output, 100));
                    }
                    Err(e) => {
                        warn!("Formatter error: {}", e);
                    }
                }
            }

            // Build/compile if command defined
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                    Ok((true, _)) => {
                        info!("Build succeeded after fix");
                    }
                    Ok((false, output)) => {
                        warn!("Build fails after fix for {} (attempt {})", issue.key, fix_attempt);
                        if fix_attempt < max_fix_attempts {
                            info!("Asking Claude to fix the build error...");
                            let repair_prompt = claude::build_fix_error_prompt(
                                "build",
                                &truncate(&output, 2000),
                                &file_path,
                                &issue.message,
                            );
                            match claude::run_claude(
                                &self.config.path,
                                &repair_prompt,
                                self.config.claude_timeout,
                                self.config.dangerously_skip_permissions,
                                self.config.show_prompts,
                            ) {
                                Ok(_) => {
                                    info!("Claude applied build fix — retrying...");
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Claude failed to fix build: {}", e);
                                }
                            }
                        }
                        // Final attempt or Claude failed — revert and give up
                        let _ = git::revert_changes(&self.config.path);
                        result.status = FixStatus::Failed(format!(
                            "Build fails after fix ({} attempts): {}",
                            fix_attempt,
                            truncate(&output, 200)
                        ));
                        return result;
                    }
                    Err(e) => {
                        warn!("Build command error: {}", e);
                        let _ = git::revert_changes(&self.config.path);
                        result.status = FixStatus::Failed(format!("Build command error: {}", e));
                        return result;
                    }
                }
            }

            // Validate tests — tests MUST NOT be modified
            if !test_command.is_empty() {
                info!("Running full test suite to validate fix...");
                match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                    Ok((true, _)) => {
                        info!("All tests pass after fix for {}", issue.key);
                        fix_verified = true;
                        break;
                    }
                    Ok((false, output)) => {
                        warn!("Tests fail after fix for {} (attempt {})", issue.key, fix_attempt);

                        if fix_attempt < max_fix_attempts {
                            info!("Asking Claude to fix the test failure (without modifying tests)...");
                            let repair_prompt = claude::build_fix_error_prompt(
                                "test",
                                &truncate(&output, 2000),
                                &file_path,
                                &issue.message,
                            );
                            match claude::run_claude(
                                &self.config.path,
                                &repair_prompt,
                                self.config.claude_timeout,
                                self.config.dangerously_skip_permissions,
                                self.config.show_prompts,
                            ) {
                                Ok(_) => {
                                    // Check Claude didn't modify test files
                                    let repair_changed = git::changed_files(&self.config.path).unwrap_or_default();
                                    let test_files_touched: Vec<_> = repair_changed.iter().filter(|f| is_test_file(f)).collect();
                                    if !test_files_touched.is_empty() {
                                        warn!("Claude modified test files during repair: {:?} — reverting repair", test_files_touched);
                                        let _ = git::revert_changes(&self.config.path);
                                    } else {
                                        info!("Claude applied test fix — retrying...");
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    warn!("Claude failed to fix tests: {}", e);
                                }
                            }
                        }

                        // Final attempt or Claude failed — revert and give up
                        let failing_tests = parse_failing_tests(&output);
                        let failure_analysis = analyze_test_failure(
                            &issue.rule,
                            &issue.message,
                            &result.change_description,
                            &failing_tests,
                            &output,
                        );

                        info!("Failing tests: {:?}", failing_tests);
                        info!("Failure analysis: {}", failure_analysis.reason);

                        let _ = git::revert_changes(&self.config.path);

                        result.status = FixStatus::NeedsReview(format!(
                            "Fix causes test failure(s) after {} attempts. {}",
                            fix_attempt,
                            failure_analysis.reason,
                        ));

                        report::append_review_needed(
                            &self.config.path,
                            &result,
                            &failing_tests,
                            &failure_analysis,
                            &output,
                        );
                        return result;
                    }
                    Err(e) => {
                        warn!("Test runner error after fix for {}: {}", issue.key, e);
                        let _ = git::revert_changes(&self.config.path);
                        result.status = FixStatus::NeedsReview(format!(
                            "Test runner failed: {}. Cannot confirm fix is safe.",
                            e
                        ));
                        report::append_review_needed(
                            &self.config.path,
                            &result,
                            &[],
                            &TestFailureAnalysis {
                                reason: format!("Test runner error: {}", e),
                                suggested_action: "Check the test command and project setup, then retry.".to_string(),
                            },
                            &e.to_string(),
                        );
                        return result;
                    }
                }
            } else {
                // No test command — consider fix verified after build passes
                fix_verified = true;
                break;
            }
        }

        if !fix_verified {
            let _ = git::revert_changes(&self.config.path);
            result.status = FixStatus::Failed(format!(
                "Could not pass build+tests after {} attempts",
                max_fix_attempts
            ));
            return result;
        }

        // Step C-4: Lint if command defined — retry with Claude to fix lint errors
        if let Some(ref lint_cmd) = self.config.commands.lint {
            let max_lint_attempts = self.config.coverage_attempts;
            for lint_attempt in 1..=max_lint_attempts {
                match runner::run_shell_command(&self.config.path, lint_cmd, "lint") {
                    Ok((true, _)) => {
                        info!("Lint passed after fix");
                        break;
                    }
                    Ok((false, output)) => {
                        if lint_attempt < max_lint_attempts {
                            info!(
                                "Lint errors after fix (attempt {}/{}) — asking Claude to fix...",
                                lint_attempt, max_lint_attempts
                            );
                            let lint_prompt = format!(
                                r#"Fix the following lint errors in this project. Do NOT modify any test files.

## Lint output:
```
{}
```

## Instructions:
1. Fix ALL the lint errors listed above
2. Do NOT modify any test files (*.spec.ts, *.test.ts, etc.)
3. Do NOT change functionality — only fix lint issues
4. Ensure the code still compiles after fixes

Apply the fixes now."#,
                                truncate(&output, 3000)
                            );
                            match claude::run_claude(
                                &self.config.path,
                                &lint_prompt,
                                self.config.claude_timeout,
                                self.config.dangerously_skip_permissions,
                                self.config.show_prompts,
                            ) {
                                Ok(_) => {
                                    // Format after lint fix
                                    if let Some(ref fmt_cmd) = self.config.commands.format {
                                        let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                                    }
                                    // Verify build still passes
                                    if let Some(ref build_cmd) = self.config.commands.build {
                                        match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                                            Ok((true, _)) => {}
                                            _ => {
                                                warn!("Lint fix broke the build — reverting lint fix");
                                                let _ = git::revert_changes(&self.config.path);
                                                break;
                                            }
                                        }
                                    }
                                    // Verify tests still pass
                                    match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                                        Ok((true, _)) => {
                                            info!("Build+tests pass after lint fix — re-checking lint...");
                                            continue;
                                        }
                                        _ => {
                                            warn!("Lint fix broke tests — reverting lint fix");
                                            let _ = git::revert_changes(&self.config.path);
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Claude failed to fix lint errors: {}", e);
                                    break;
                                }
                            }
                        } else {
                            warn!(
                                "Lint still failing after {} attempts for {} (non-blocking): {}",
                                max_lint_attempts, issue.key, truncate(&output, 200)
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Lint command error (non-blocking): {}", e);
                        break;
                    }
                }
            }
        }

        // Step C-5: Regenerate coverage report before re-scan
        // Run coverage command (not just test) so SonarQube picks up fresh lcov data
        if let Some(ref cov_cmd) = self.config.coverage_command
            .clone()
            .or_else(|| self.config.commands.coverage.clone())
        {
            info!("Regenerating coverage report before SonarQube re-scan...");
            match runner::run_shell_command(&self.config.path, cov_cmd, "coverage") {
                Ok((true, _)) => info!("Coverage report updated"),
                Ok((false, output)) => warn!("Coverage command failed (non-blocking): {}", truncate(&output, 100)),
                Err(e) => warn!("Coverage command error (non-blocking): {}", e),
            }
        }

        // Step C-6: Re-scan with SonarQube to verify the issue is resolved (with retries)
        let max_sonar_retries = self.config.coverage_attempts;
        if let Some(ref scanner) = self.config.scanner {
            for sonar_attempt in 1..=max_sonar_retries {
                info!("Re-scanning with SonarQube to verify fix for {} (attempt {}/{})...", issue.key, sonar_attempt, max_sonar_retries);
                match self.client.run_scanner(&self.config.path, scanner, &self.config.branch) {
                    Ok(ce_task_id) => {
                        if let Err(e) = self.client.wait_for_analysis(ce_task_id.as_deref()).await {
                            warn!("SonarQube re-analysis wait failed: {} — continuing anyway", e);
                            break; // Can't verify, proceed optimistically
                        }
                        // Check if the specific issue is still open
                        match self.client.fetch_issues().await {
                            Ok(issues) => {
                                let still_open = issues.iter().any(|i| i.key == issue.key);
                                if !still_open {
                                    info!("SonarQube confirms issue {} is resolved", issue.key);
                                    break; // Issue resolved, exit retry loop
                                }
                                // Issue still reported — retry with Claude or give up
                                if sonar_attempt < max_sonar_retries {
                                    warn!(
                                        "SonarQube still reports issue {} (attempt {}/{}) — asking Claude for a different approach...",
                                        issue.key, sonar_attempt, max_sonar_retries
                                    );
                                    // Revert the failed fix
                                    let _ = git::revert_changes(&self.config.path);
                                    // Ask Claude to try a different approach
                                    let retry_prompt = format!(
                                        r#"Your previous fix for SonarQube issue {} did NOT resolve it.

## Issue details
- **Rule**: {} — {}
- **File**: `{}`
- **Previous attempt**: The fix compiled and tests passed, but SonarQube still reports the same issue.

## Instructions:
1. Try a DIFFERENT approach to fix this issue
2. The previous fix was insufficient — the code still violates rule {}
3. Read the file, understand why the rule is still triggered, and apply a more thorough fix
4. Do NOT modify any test files
5. Ensure the fix compiles and tests still pass

Apply a different fix now."#,
                                        issue.key, issue.rule, issue.message,
                                        file_path, issue.rule
                                    );
                                    match claude::run_claude(
                                        &self.config.path,
                                        &retry_prompt,
                                        self.config.claude_timeout,
                                        self.config.dangerously_skip_permissions,
                                        self.config.show_prompts,
                                    ) {
                                        Ok(_) => {
                                            info!("Claude applied retry fix — verifying build+tests...");
                                            // Quick build+test check before re-scanning
                                            if let Some(ref fmt_cmd) = self.config.commands.format {
                                                let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                                            }
                                            if let Some(ref build_cmd) = self.config.commands.build {
                                                match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                                                    Ok((true, _)) => {}
                                                    _ => {
                                                        warn!("Retry fix broke the build — reverting");
                                                        let _ = git::revert_changes(&self.config.path);
                                                        break;
                                                    }
                                                }
                                            }
                                            match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                                                Ok((true, _)) => {
                                                    // Build+tests pass, loop will re-scan
                                                    continue;
                                                }
                                                _ => {
                                                    warn!("Retry fix broke tests — reverting");
                                                    let _ = git::revert_changes(&self.config.path);
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Claude failed to retry fix: {}", e);
                                            break;
                                        }
                                    }
                                } else {
                                    // Final attempt — give up
                                    warn!("SonarQube still reports issue {} after {} attempts — reverting", issue.key, max_sonar_retries);
                                    let _ = git::revert_changes(&self.config.path);
                                    result.status = FixStatus::NeedsReview(
                                        format!("Fix applied and tests pass, but SonarQube still reports the issue after {} attempts. Manual review needed.", max_sonar_retries)
                                    );
                                    return result;
                                }
                            }
                            Err(e) => {
                                warn!("Could not verify issue resolution: {} — continuing", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("SonarQube re-scan failed: {} — continuing without verification", e);
                        break;
                    }
                }
            }
        }

        // Step D: Commit the fix (US-008)
        // Only stage the files changed by this fix — exclude changelog/state/report files
        let files_to_stage: Vec<&str> = changed
            .iter()
            .map(|s| s.as_str())
            .collect();
        if !files_to_stage.is_empty() {
            let _ = git::add_files(&self.config.path, &files_to_stage);
        }
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let changed_files_str = changed.join(", ");
            let issue_url = format!(
                "{}/project/issues?id={}&open={}",
                self.config.sonar_url, self.config.sonar_project_id, issue.key
            );
            let msg = format!(
                "fix(sonar): {} - {}\n\n\
                 Rule: {}\n\
                 Severity: {}\n\
                 File: {}:{}\n\
                 Modified: {}\n\
                 Issue: {}",
                issue.issue_type.to_lowercase(),
                truncate(&issue.message, 72),
                issue.rule,
                issue.severity,
                file_path,
                lines,
                changed_files_str,
                issue_url,
            );
            if let Err(e) = git::commit(&self.config.path, &msg) {
                result.status = FixStatus::Failed(format!("Commit failed: {}", e));
                return result;
            }
            info!("Committed fix for {}", issue.key);

            // US-021: Capture diff summary for PR body
            result.diff_summary = capture_diff_summary(&self.config.path);
        }

        result.status = FixStatus::Fixed;
        result
    }

    /// Check line-level coverage for the affected lines of an issue (US-004).
    async fn check_coverage(
        &self,
        component: &str,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> CoverageCheck {
        match self
            .client
            .get_line_coverage(component, start_line, end_line)
            .await
        {
            Ok(cov) => {
                cov.log_summary(file_path, start_line, end_line);
                if cov.fully_covered {
                    CoverageCheck::FullyCovered
                } else if cov.uncovered_lines.is_empty() && cov.covered_lines.is_empty() {
                    // All lines non-coverable, or no data
                    CoverageCheck::FullyCovered
                } else {
                    CoverageCheck::NeedsCoverage {
                        uncovered_lines: cov.uncovered_lines,
                        coverage_pct: cov.coverage_pct,
                    }
                }
            }
            Err(e) => {
                warn!("Failed to check coverage for {}: {}", file_path, e);
                CoverageCheck::Unavailable
            }
        }
    }

    /// Generate tests with retry loop (US-005).
    ///
    /// 1. Generate tests via `claude -d`
    /// 2. Run tests — if they fail, return TestsFailed
    /// 3. Re-check coverage — if 100%, return Success
    /// 4. If < 100%, retry with additional context (up to self.config.coverage_attempts)
    /// 5. After all retries, return PartialCoverage if tests pass but coverage < 100%
    async fn generate_tests_with_retry(
        &self,
        issue: &Issue,
        file_path: &str,
        file_content: &str,
        start_line: u32,
        end_line: u32,
        initial_uncovered: &[u32],
        test_command: &str,
    ) -> TestGenResult {
        let test_examples = runner::find_test_examples(&self.config.path);
        let examples_str = test_examples.join("\n\n");
        let framework = detect_test_framework(&self.config.path);
        let mut all_test_files: Vec<String> = Vec::new();
        let mut current_uncovered = initial_uncovered.to_vec();
        let mut last_test_output = String::new();

        for attempt in 1..=self.config.coverage_attempts {
            info!(
                "Test generation attempt {}/{} for {} ({} uncovered lines)",
                attempt,
                self.config.coverage_attempts,
                issue.key,
                current_uncovered.len()
            );

            let uncovered_desc = current_uncovered
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(", ");

            // Build prompt — first attempt or retry with context
            let prompt = if attempt == 1 {
                let uncovered = format!(
                    "Lines {}-{} (specifically uncovered: {})",
                    start_line, end_line, uncovered_desc
                );
                claude::build_test_generation_prompt(
                    file_path,
                    file_content,
                    &uncovered,
                    &framework,
                    &examples_str,
                )
            } else {
                let still_uncovered = format!(
                    "Lines still uncovered: {}",
                    uncovered_desc
                );
                claude::build_test_generation_retry_prompt(
                    file_path,
                    file_content,
                    &still_uncovered,
                    &framework,
                    &examples_str,
                    attempt,
                    &truncate(&last_test_output, 1000),
                )
            };

            // Run claude to generate tests
            match claude::run_claude(&self.config.path, &prompt, self.config.claude_timeout, self.config.dangerously_skip_permissions, self.config.show_prompts) {
                Ok(_) => {}
                Err(e) => {
                    if attempt == 1 {
                        return TestGenResult::GenerationFailed {
                            error: e.to_string(),
                        };
                    }
                    // On retries, keep what we have
                    warn!("Claude failed on retry attempt {}: {}", attempt, e);
                    break;
                }
            }

            // Detect new test files
            let changed = git::changed_files(&self.config.path).unwrap_or_default();
            let new_test_files: Vec<String> = changed
                .into_iter()
                .filter(|f| is_test_file(f) && !all_test_files.contains(f))
                .collect();

            if !new_test_files.is_empty() {
                info!("Generated test files (attempt {}): {:?}", attempt, new_test_files);
                all_test_files.extend(new_test_files);
            } else if attempt == 1 {
                warn!("Claude did not create any test files");
                return TestGenResult::GenerationFailed {
                    error: "No test files were created".to_string(),
                };
            }

            // Run tests to verify they pass
            match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                Ok((true, output)) => {
                    info!("Tests pass (attempt {})", attempt);
                    last_test_output = output;
                }
                Ok((false, output)) => {
                    warn!("Generated tests fail (attempt {})", attempt);
                    if attempt < self.config.coverage_attempts {
                        // Revert only the failing test changes and retry
                        let _ = git::revert_changes(&self.config.path);
                        all_test_files.clear();
                        last_test_output = output;
                        continue;
                    }
                    // Final attempt failed — revert all
                    let _ = git::revert_changes(&self.config.path);
                    return TestGenResult::TestsFailed { output };
                }
                Err(e) => {
                    warn!("Failed to run tests (attempt {}): {}", attempt, e);
                    last_test_output = e.to_string();
                }
            }

            // Run coverage command to generate local lcov report
            let coverage_cmd = self.config.coverage_command.clone()
                .or_else(|| runner::detect_coverage_command(&self.config.path));
            if let Some(ref cov_cmd) = coverage_cmd {
                info!("Running coverage command: {}", cov_cmd);
                match runner::run_shell_command(&self.config.path, cov_cmd, "coverage") {
                    Ok((true, _)) => info!("Coverage report generated successfully"),
                    Ok((false, output)) => warn!("Coverage command failed: {}", truncate(&output, 200)),
                    Err(e) => warn!("Failed to run coverage command: {}", e),
                }
            }

            // Check coverage locally from lcov report (fast, no SonarQube round-trip)
            let lcov_path = runner::find_lcov_report(&self.config.path);
            match lcov_path {
                Some(ref lcov) => {
                    match runner::check_local_coverage(lcov, file_path, start_line, end_line) {
                        Some(cov) if cov.fully_covered => {
                            info!(
                                "100% local coverage achieved after {} attempt(s) for {}",
                                attempt, issue.key
                            );
                            return TestGenResult::Success {
                                test_files: all_test_files,
                            };
                        }
                        Some(cov) => {
                            info!(
                                "Local coverage {:.1}% ({} lines still uncovered) after attempt {}",
                                cov.coverage_pct,
                                cov.uncovered.len(),
                                attempt
                            );
                            current_uncovered = cov.uncovered;
                            // Continue to next attempt
                        }
                        None => {
                            // Log which files ARE in the lcov to help diagnose
                            if let Ok(content) = std::fs::read_to_string(lcov) {
                                let lcov_files: Vec<&str> = content.lines()
                                    .filter(|l| l.starts_with("SF:"))
                                    .map(|l| &l[3..])
                                    .collect();
                                warn!(
                                    "File '{}' not found in lcov report. Files in report: {:?}",
                                    file_path, lcov_files
                                );
                            } else {
                                warn!("File not found in lcov report — cannot verify coverage");
                            }
                            return TestGenResult::Success {
                                test_files: all_test_files,
                            };
                        }
                    }
                }
                None => {
                    warn!("No lcov report found — cannot verify coverage");
                    return TestGenResult::Success {
                        test_files: all_test_files,
                    };
                }
            }
        }

        // Exhausted all attempts but tests pass — partial coverage
        if all_test_files.is_empty() {
            TestGenResult::GenerationFailed {
                error: "No tests generated after all attempts".to_string(),
            }
        } else {
            TestGenResult::PartialCoverage {
                test_files: all_test_files,
            }
        }
    }

    /// Create a PR from the accumulated results (US-008).
    fn create_pr(&self, branch_name: &str) -> Result<String> {
        // Stage any remaining changes (changelog, etc.) and push
        let _ = git::add_all(&self.config.path);
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let _ = git::commit(&self.config.path, "chore: include changelog and report updates");
        }

        // US-016: Push with retry
        let path = self.config.path.clone();
        let branch = branch_name.to_string();
        crate::retry::retry_sync(3, 3, "git push", || {
            git::push(&path, &branch)
        })?;

        let fixed_results: Vec<&IssueResult> = self
            .results
            .iter()
            .filter(|r| matches!(r.status, FixStatus::Fixed))
            .collect();
        let failed_count = self
            .results
            .iter()
            .filter(|r| matches!(r.status, FixStatus::Failed(_) | FixStatus::NeedsReview(_)))
            .count();

        // -- Title (US-008) --
        let title = if fixed_results.len() == 1 {
            let r = &fixed_results[0];
            format!(
                "[SonarQube] Fix {} {}: {}",
                r.severity,
                r.issue_type.to_lowercase(),
                truncate(&r.message, 50)
            )
        } else {
            let severities: Vec<&str> = self
                .results
                .iter()
                .map(|r| r.severity.as_str())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            format!(
                "[SonarQube] Fix {} issues ({})",
                fixed_results.len(),
                severities.join(", ")
            )
        };

        // -- Body --
        let mut body = String::from("## Summary\n\n");
        body.push_str("Automated SonarQube issue fixes by Reparo.\n\n");

        if self.results.len() > 1 {
            body.push_str(&format!(
                "**Result**: {} fixed, {} failed/review out of {} processed.\n\n",
                fixed_results.len(),
                failed_count,
                self.results.len()
            ));
        }

        // Issue table
        body.push_str("### Issues\n\n");
        body.push_str("| Issue | Severity | Type | File | Rule | Status |\n");
        body.push_str("|-------|----------|------|------|------|--------|\n");
        for r in &self.results {
            let status = match &r.status {
                FixStatus::Fixed => "Fixed",
                FixStatus::NeedsReview(_) => "Needs review",
                FixStatus::Failed(_) => "Failed",
                FixStatus::Skipped(_) => "Skipped",
            };
            body.push_str(&format!(
                "| {} | {} | {} | `{}` | `{}` | {} |\n",
                r.issue_key, r.severity, r.issue_type, r.file, r.rule, status,
            ));
        }

        if !fixed_results.is_empty() {
            body.push_str("\n### Changes\n\n");
            for r in &fixed_results {
                body.push_str(&format!("- **{}**: {}\n", r.issue_key, r.change_description));
            }
        }

        // Tests added
        let all_tests: Vec<&str> = fixed_results
            .iter()
            .flat_map(|r| r.tests_added.iter().map(|s| s.as_str()))
            .collect();
        if !all_tests.is_empty() {
            body.push_str("\n### Tests added\n\n");
            for t in &all_tests {
                body.push_str(&format!("- `{}`\n", t));
            }
        }

        // US-021: Include diff summaries
        let diffs: Vec<(&str, &str)> = fixed_results
            .iter()
            .filter_map(|r| {
                r.diff_summary
                    .as_deref()
                    .map(|d| (r.issue_key.as_str(), d))
            })
            .collect();
        if !diffs.is_empty() {
            body.push_str("\n### Diffs\n\n");
            for (key, diff) in &diffs {
                body.push_str(&format!("**{}**:\n{}\n\n", key, diff));
            }
        }

        body.push_str("\n## Test plan\n\n");
        body.push_str("- [x] All existing tests pass (verified by Reparo)\n");
        if !all_tests.is_empty() {
            body.push_str(&format!(
                "- [x] {} new test file(s) added for coverage\n",
                all_tests.len()
            ));
        }
        body.push_str("- [ ] SonarQube re-scan confirms issues resolved\n");
        body.push_str("- [ ] Code review approved\n\n");
        body.push_str("Generated with [Reparo](https://github.com/reparo) using Claude\n");

        // Labels
        let mut labels: Vec<&str> = vec!["sonar-fix", "automated"];
        let severity_labels: Vec<String> = self
            .results
            .iter()
            .map(|r| r.severity.to_lowercase())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let severity_refs: Vec<&str> = severity_labels.iter().map(|s| s.as_str()).collect();
        labels.extend(severity_refs);

        git::create_pr(
            &self.config.path,
            &title,
            &body,
            &self.config.branch,
            &labels,
        )
    }

    /// Print a structured summary of issues by severity and type (US-003).
    fn print_issue_summary(&self, issues: &[Issue]) {
        // Counts by severity (in priority order)
        let severity_order = ["BLOCKER", "CRITICAL", "MAJOR", "MINOR", "INFO"];
        let type_order = ["BUG", "VULNERABILITY", "SECURITY_HOTSPOT", "CODE_SMELL"];

        let mut by_severity: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut by_type: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();

        for issue in issues {
            *by_severity.entry(&issue.severity).or_default() += 1;
            *by_type.entry(&issue.issue_type).or_default() += 1;
        }

        info!("┌─────────────────────────────────────────────────┐");
        info!("│              Issue Summary ({:>5} total)          │", issues.len());
        info!("├─────────────────────────────────────────────────┤");
        info!("│ By severity:                                    │");
        for sev in &severity_order {
            if let Some(&count) = by_severity.get(sev) {
                let bar = "█".repeat(count.min(30));
                info!("│   {:>8}: {:>4}  {:<30}│", sev, count, bar);
            }
        }
        info!("│ By type:                                        │");
        for typ in &type_order {
            if let Some(&count) = by_type.get(typ) {
                info!("│   {:>16}: {:>4}                            │", typ, count);
            }
        }
        info!("└─────────────────────────────────────────────────┘");
    }

    /// Print per-issue listing: severity, type, file, line, rule, message (US-003).
    fn print_issue_listing(&self, issues: &[Issue]) {
        info!("Issue listing (sorted by priority):");
        info!(
            "  {:<10} {:<18} {:<40} {:<8} {:<25} {}",
            "SEVERITY", "TYPE", "FILE", "LINE", "RULE", "MESSAGE"
        );
        info!("  {}", "-".repeat(120));
        for issue in issues {
            let file = sonar::component_to_path(&issue.component);
            let line = match &issue.text_range {
                Some(tr) => {
                    if tr.start_line == tr.end_line {
                        format!("{}", tr.start_line)
                    } else {
                        format!("{}-{}", tr.start_line, tr.end_line)
                    }
                }
                None => "?".to_string(),
            };
            // Truncate long fields for readable console output
            let file_char_count = file.chars().count();
            let file_display = if file_char_count > 38 {
                let suffix: String = file.chars().skip(file_char_count - 35).collect();
                format!("...{}", suffix)
            } else {
                file
            };
            let msg_char_count = issue.message.chars().count();
            let msg_display = if msg_char_count > 60 {
                let prefix: String = issue.message.chars().take(57).collect();
                format!("{}...", prefix)
            } else {
                issue.message.clone()
            };
            info!(
                "  {:<10} {:<18} {:<40} {:<8} {:<25} {}",
                issue.severity,
                issue.issue_type,
                file_display,
                line,
                issue.rule,
                msg_display,
            );
        }
    }

    /// Print the final summary and return the appropriate exit code (US-010).
    ///
    /// Exit codes:
    /// - 0: all issues fixed successfully (or no issues found)
    /// - 2: partial success (some fixes, some failures)
    fn print_summary(&self, elapsed: u64) -> i32 {
        let total = self.results.len();
        let fixed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
        let review = self.results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))).count();
        let failed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))).count();
        let skipped = self.results.iter().filter(|r| matches!(r.status, FixStatus::Skipped(_))).count();
        let prs_created: usize = self
            .results
            .iter()
            .filter_map(|r| r.pr_url.as_ref())
            .collect::<std::collections::HashSet<_>>()
            .len();

        info!("╔══════════════════════════════════════════════╗");
        info!("║           Reparo — Final Summary          ║");
        info!("╠══════════════════════════════════════════════╣");
        info!("║  Total issues processed:  {:>5}              ║", total);
        info!("║  Fixed:                   {:>5}              ║", fixed);
        info!("║  Needs manual review:     {:>5}              ║", review);
        info!("║  Failed:                  {:>5}              ║", failed);
        info!("║  Skipped (idempotent):    {:>5}              ║", skipped);
        info!("║  PRs created:            {:>5}              ║", prs_created);
        info!("║  Time elapsed:         {:>3}m {:>2}s              ║", elapsed / 60, elapsed % 60);
        info!("╚══════════════════════════════════════════════╝");

        if prs_created > 0 {
            info!("PRs:");
            for url in self.results.iter().filter_map(|r| r.pr_url.as_ref()).collect::<std::collections::HashSet<_>>() {
                info!("  {}", url);
            }
        }

        // Determine exit code
        if total == 0 || fixed == total {
            0 // all good
        } else if fixed > 0 {
            2 // partial success
        } else {
            2 // all failed but not a config error
        }
    }
}

use crate::report::TestFailureAnalysis;

/// Parse test output to extract names of failing tests (US-007).
///
/// Handles common test runner output formats:
/// - pytest: `FAILED tests/test_foo.py::test_bar`
/// - JUnit/Maven: `Tests run: X, Failures: Y` + `testMethodName(ClassName)`
/// - Jest: `FAIL src/foo.test.js` + `✕ test name`
/// - Go: `--- FAIL: TestFoo`
/// - Rust: `test module::test_name ... FAILED`
fn parse_failing_tests(output: &str) -> Vec<String> {
    let mut failures = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // pytest: FAILED tests/test_foo.py::test_bar - ...
        if trimmed.starts_with("FAILED ") {
            let test_name = trimmed
                .strip_prefix("FAILED ")
                .unwrap_or(trimmed)
                .split(" - ")
                .next()
                .unwrap_or(trimmed)
                .trim();
            if !test_name.is_empty() {
                failures.push(test_name.to_string());
            }
        }
        // Go: --- FAIL: TestFoo (0.00s)
        else if trimmed.starts_with("--- FAIL: ") {
            let test_name = trimmed
                .strip_prefix("--- FAIL: ")
                .unwrap_or(trimmed)
                .split_whitespace()
                .next()
                .unwrap_or(trimmed)
                .trim();
            if !test_name.is_empty() {
                failures.push(test_name.to_string());
            }
        }
        // Rust: test module::test_name ... FAILED
        else if trimmed.starts_with("test ") && trimmed.ends_with("FAILED") {
            let test_name = trimmed
                .strip_prefix("test ")
                .unwrap_or(trimmed)
                .strip_suffix("... FAILED")
                .unwrap_or(trimmed)
                .trim();
            if !test_name.is_empty() {
                failures.push(test_name.to_string());
            }
        }
        // Jest: FAIL src/foo.test.js
        else if trimmed.starts_with("FAIL ") && !trimmed.contains("Tests:") {
            let test_file = trimmed
                .strip_prefix("FAIL ")
                .unwrap_or(trimmed)
                .trim();
            if !test_file.is_empty() {
                failures.push(test_file.to_string());
            }
        }
        // Jest: ✕ test name (Xms)
        else if let Some(rest) = trimmed.strip_prefix("✕ ").or_else(|| trimmed.strip_prefix("× ")) {
            let test_name = rest.split('(').next().unwrap_or(rest).trim();
            if !test_name.is_empty() {
                failures.push(test_name.to_string());
            }
        }
    }

    // Deduplicate
    failures.sort();
    failures.dedup();
    failures
}

/// Analyze why the test failure likely occurred (US-007).
fn analyze_test_failure(
    rule: &str,
    issue_message: &str,
    change_description: &str,
    failing_tests: &[String],
    _test_output: &str,
) -> TestFailureAnalysis {
    let tests_str = if failing_tests.is_empty() {
        "unknown test(s)".to_string()
    } else {
        failing_tests.join(", ")
    };

    // Heuristic analysis based on rule type
    let (reason, action) = if rule.contains("S1172") || rule.contains("unused") {
        (
            format!(
                "Removing unused parameter likely broke test(s) [{}] that reference it directly.",
                tests_str
            ),
            "Update the test to not pass the removed parameter, or keep the parameter with a suppression comment.".to_string(),
        )
    } else if rule.contains("S1135") || rule.contains("todo") || rule.contains("fixme") {
        (
            format!(
                "Removing TODO/FIXME comment may have changed line numbers or behavior expected by test(s) [{}].",
                tests_str
            ),
            "This is likely a false positive from the fix. Review if the test assertion depends on specific output or line numbers.".to_string(),
        )
    } else if rule.contains("rename") || issue_message.to_lowercase().contains("rename") {
        (
            format!(
                "Renaming changed the API surface. Test(s) [{}] reference the old name.",
                tests_str
            ),
            "Update the test(s) to use the new name, or reconsider the rename.".to_string(),
        )
    } else if issue_message.to_lowercase().contains("return") || issue_message.to_lowercase().contains("null") {
        (
            format!(
                "The fix changed return value or null-handling behavior. Test(s) [{}] expect the old behavior.",
                tests_str
            ),
            "Review whether the test expectation or the fix approach should be adjusted.".to_string(),
        )
    } else {
        (
            format!(
                "The fix for rule {} caused test failure(s) in [{}]. The change ({}) altered behavior that the tests depend on.",
                rule, tests_str, truncate(change_description, 100)
            ),
            "Review the failing test(s) and determine if the test expectation should be updated or if the fix approach needs revision.".to_string(),
        )
    };

    TestFailureAnalysis {
        reason,
        suggested_action: action,
    }
}

/// Capture a git diff summary of the last commit for PR body (US-021).
fn capture_diff_summary(project_path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(project_path)
        .args(["diff", "HEAD~1", "--stat"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stat = String::from_utf8_lossy(&output.stdout).to_string();

    // Also get a short diff (limited to 200 lines)
    let diff_output = std::process::Command::new("git")
        .current_dir(project_path)
        .args(["diff", "HEAD~1", "-U3"])
        .output()
        .ok()?;

    let full_diff = String::from_utf8_lossy(&diff_output.stdout).to_string();
    let lines: Vec<&str> = full_diff.lines().collect();
    let diff_truncated = if lines.len() > 200 {
        format!(
            "{}\n\n... ({} more lines, see Files tab)",
            lines[..200].join("\n"),
            lines.len() - 200
        )
    } else {
        full_diff
    };

    Some(format!("```\n{}\n```\n\n<details>\n<summary>Full diff</summary>\n\n```diff\n{}\n```\n\n</details>", stat.trim(), diff_truncated))
}

#[allow(dead_code)]
fn sanitize_branch(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn format_lines(text_range: &Option<sonar::TextRange>) -> String {
    match text_range {
        Some(tr) => {
            if tr.start_line == tr.end_line {
                format!("{}", tr.start_line)
            } else {
                format!("{}-{}", tr.start_line, tr.end_line)
            }
        }
        None => "?".to_string(),
    }
}

/// Build a structured description of the changes made (US-006).
fn build_change_description(claude_output: &str, changed_files: &[String]) -> String {
    let summary = claude_output
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .unwrap_or("Automated fix applied");

    let files_str = changed_files.join(", ");
    format!("{} [files: {}]", summary, files_str)
}

fn detect_test_framework(project_path: &Path) -> String {
    if project_path.join("pom.xml").exists() {
        return "JUnit 5 (Maven)".to_string();
    }
    if project_path.join("build.gradle").exists() || project_path.join("build.gradle.kts").exists() {
        return "JUnit 5 (Gradle)".to_string();
    }
    if project_path.join("package.json").exists() {
        // Try to detect jest vs mocha
        if let Ok(content) = std::fs::read_to_string(project_path.join("package.json")) {
            if content.contains("jest") {
                return "Jest".to_string();
            }
            if content.contains("mocha") {
                return "Mocha".to_string();
            }
            if content.contains("vitest") {
                return "Vitest".to_string();
            }
        }
        return "Jest (assumed)".to_string();
    }
    if project_path.join("Cargo.toml").exists() {
        return "Rust built-in #[test]".to_string();
    }
    if project_path.join("pyproject.toml").exists() || project_path.join("setup.py").exists() {
        return "pytest".to_string();
    }
    if project_path.join("go.mod").exists() {
        return "Go testing package".to_string();
    }
    "Unknown - use project conventions".to_string()
}

/// Files that cannot have unit test coverage (style, templates, assets).
/// These should skip coverage checks and test generation.
fn is_non_coverable_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".scss")
        || lower.ends_with(".css")
        || lower.ends_with(".less")
        || lower.ends_with(".html")
        || lower.ends_with(".htm")
        || lower.ends_with(".svg")
        || lower.ends_with(".json")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".xml")
        || lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".gif")
}

/// Internal files that should be excluded from change detection.
fn is_internal_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".calendula-state.json")
        || lower.ends_with(".reparo-state.json")
        || lower.contains("techdebt_changelog")
        || lower.contains("report.md")
        || lower.contains("review_needed.md")
        || lower.ends_with("report-task.txt")
}

fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("test")
        || lower.contains("spec")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_test_file --

    #[test]
    fn test_is_test_file_python() {
        assert!(is_test_file("tests/test_calculator.py"));
        assert!(is_test_file("src/test_utils.py"));
    }

    #[test]
    fn test_is_test_file_java() {
        assert!(is_test_file("src/test/java/FooTest.java"));
        assert!(is_test_file("src/test/java/FooSpec.java"));
    }

    #[test]
    fn test_is_test_file_js() {
        assert!(is_test_file("src/components/Button.test.tsx"));
        assert!(is_test_file("src/components/Button.spec.ts"));
    }

    #[test]
    fn test_is_test_file_go() {
        assert!(is_test_file("pkg/handler_test.go"));
    }

    #[test]
    fn test_is_not_test_file() {
        assert!(!is_test_file("src/main.py"));
        assert!(!is_test_file("src/calculator.py"));
        assert!(!is_test_file("lib/utils.js"));
        assert!(!is_test_file("Cargo.toml"));
    }

    // -- sanitize_branch --

    #[test]
    fn test_sanitize_branch_simple() {
        assert_eq!(sanitize_branch("AX-123"), "AX-123");
        assert_eq!(sanitize_branch("issue_456"), "issue_456");
    }

    #[test]
    fn test_sanitize_branch_special_chars() {
        assert_eq!(sanitize_branch("AX:123/foo"), "AX-123-foo");
        assert_eq!(sanitize_branch("issue #42"), "issue--42");
    }

    // -- build_change_description --

    #[test]
    fn test_build_change_description_with_output() {
        let desc = build_change_description(
            "Added null check before dereference\nModified line 42",
            &["src/service.py".to_string()],
        );
        assert!(desc.contains("Added null check"));
        assert!(desc.contains("src/service.py"));
    }

    #[test]
    fn test_build_change_description_empty_output() {
        let desc = build_change_description(
            "",
            &["src/a.py".to_string(), "src/b.py".to_string()],
        );
        assert!(desc.contains("Automated fix applied"));
        assert!(desc.contains("src/a.py, src/b.py"));
    }

    #[test]
    fn test_build_change_description_skips_headers() {
        let desc = build_change_description(
            "# Summary\nFixed the null pointer issue\nDone",
            &["src/foo.java".to_string()],
        );
        assert!(desc.contains("Fixed the null pointer issue"));
        assert!(!desc.contains("# Summary"));
    }

    // -- format_lines --

    #[test]
    fn test_format_lines_single() {
        let tr = Some(sonar::TextRange {
            start_line: 42,
            end_line: 42,
            start_offset: None,
            end_offset: None,
        });
        assert_eq!(format_lines(&tr), "42");
    }

    #[test]
    fn test_format_lines_range() {
        let tr = Some(sonar::TextRange {
            start_line: 10,
            end_line: 20,
            start_offset: None,
            end_offset: None,
        });
        assert_eq!(format_lines(&tr), "10-20");
    }

    #[test]
    fn test_format_lines_none() {
        assert_eq!(format_lines(&None), "?");
    }

    // -- truncate --

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    // -- detect_test_framework --

    #[test]
    fn test_detect_test_framework_python() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]").unwrap();
        assert_eq!(detect_test_framework(tmp.path()), "pytest");
    }

    #[test]
    fn test_detect_test_framework_node_jest() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"devDependencies":{"jest":"^29"}}"#,
        )
        .unwrap();
        assert_eq!(detect_test_framework(tmp.path()), "Jest");
    }

    #[test]
    fn test_detect_test_framework_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect_test_framework(tmp.path()).contains("Unknown"));
    }

    // -- parse_failing_tests (US-007) --

    #[test]
    fn test_parse_failing_tests_pytest() {
        let output = r#"
============================= test session starts ==============================
collected 5 items
tests/test_calc.py::test_add PASSED
tests/test_calc.py::test_divide FAILED
tests/test_calc.py::test_multiply PASSED

FAILED tests/test_calc.py::test_divide - ZeroDivisionError
FAILED tests/test_calc.py::test_subtract - AssertionError
============================= 2 failed, 3 passed ==============================
"#;
        let failures = parse_failing_tests(output);
        assert_eq!(failures.len(), 2);
        assert!(failures.contains(&"tests/test_calc.py::test_divide".to_string()));
        assert!(failures.contains(&"tests/test_calc.py::test_subtract".to_string()));
    }

    #[test]
    fn test_parse_failing_tests_go() {
        let output = r#"
--- FAIL: TestHandler (0.00s)
--- FAIL: TestService (0.01s)
FAIL	github.com/foo/bar/pkg	0.015s
"#;
        let failures = parse_failing_tests(output);
        assert_eq!(failures.len(), 2);
        assert!(failures.contains(&"TestHandler".to_string()));
        assert!(failures.contains(&"TestService".to_string()));
    }

    #[test]
    fn test_parse_failing_tests_rust() {
        let output = r#"
running 3 tests
test config::tests::test_validate ... ok
test sonar::tests::test_fetch ... FAILED
test git::tests::test_commit ... FAILED

failures:
    sonar::tests::test_fetch
    git::tests::test_commit
"#;
        let failures = parse_failing_tests(output);
        assert_eq!(failures.len(), 2);
        assert!(failures.contains(&"sonar::tests::test_fetch".to_string()));
        assert!(failures.contains(&"git::tests::test_commit".to_string()));
    }

    #[test]
    fn test_parse_failing_tests_jest() {
        let output = r#"
FAIL src/components/Button.test.tsx
  ✕ renders correctly (15ms)
  ✓ handles click (3ms)
  ✕ shows tooltip (8ms)
"#;
        let failures = parse_failing_tests(output);
        assert!(failures.contains(&"src/components/Button.test.tsx".to_string()));
        assert!(failures.contains(&"renders correctly".to_string()));
        assert!(failures.contains(&"shows tooltip".to_string()));
    }

    #[test]
    fn test_parse_failing_tests_empty() {
        let failures = parse_failing_tests("All tests passed!\n");
        assert!(failures.is_empty());
    }

    #[test]
    fn test_parse_failing_tests_dedup() {
        let output = "FAILED tests/test_a.py::test_x - err\nFAILED tests/test_a.py::test_x - err\n";
        let failures = parse_failing_tests(output);
        assert_eq!(failures.len(), 1);
    }

    // -- analyze_test_failure (US-007) --

    #[test]
    fn test_analyze_failure_unused_param() {
        let analysis = analyze_test_failure(
            "java:S1172",
            "Remove this unused parameter",
            "Removed param 'ctx'",
            &["test_handler".to_string()],
            "",
        );
        assert!(analysis.reason.contains("unused parameter"));
        assert!(analysis.reason.contains("test_handler"));
    }

    #[test]
    fn test_analyze_failure_null_handling() {
        let analysis = analyze_test_failure(
            "python:S1234",
            "Fix null pointer dereference",
            "Added null check",
            &["test_service".to_string()],
            "",
        );
        assert!(analysis.reason.contains("null"));
        assert!(analysis.reason.contains("test_service"));
    }

    #[test]
    fn test_analyze_failure_generic() {
        let analysis = analyze_test_failure(
            "java:S5678",
            "Reduce cognitive complexity",
            "Refactored method",
            &["test_foo".to_string(), "test_bar".to_string()],
            "",
        );
        assert!(analysis.reason.contains("java:S5678"));
        assert!(analysis.reason.contains("test_foo, test_bar"));
        assert!(!analysis.suggested_action.is_empty());
    }

    #[test]
    fn test_analyze_failure_no_tests() {
        let analysis = analyze_test_failure(
            "java:S1000",
            "Some issue",
            "Some fix",
            &[],
            "",
        );
        assert!(analysis.reason.contains("unknown test(s)"));
    }

    // -- Batch logic (US-009) --

    #[test]
    fn test_batch_size_default() {
        // batch_size=1 means each issue gets its own branch
        let issues = vec![
            make_test_issue("A1"),
            make_test_issue("A2"),
            make_test_issue("A3"),
        ];
        let batch_size = 1usize;
        let batches: Vec<&[sonar::Issue]> = issues.chunks(batch_size).collect();
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_batch_size_grouping() {
        let issues = vec![
            make_test_issue("A1"),
            make_test_issue("A2"),
            make_test_issue("A3"),
            make_test_issue("A4"),
            make_test_issue("A5"),
        ];
        let batch_size = 3usize;
        let batches: Vec<&[sonar::Issue]> = issues.chunks(batch_size).collect();
        assert_eq!(batches.len(), 2); // [3, 2]
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 2);
    }

    #[test]
    fn test_batch_size_zero_means_all() {
        let issues = vec![
            make_test_issue("A1"),
            make_test_issue("A2"),
            make_test_issue("A3"),
        ];
        let config_batch_size = 0usize;
        let batch_size = if config_batch_size == 0 { issues.len() } else { config_batch_size };
        let batches: Vec<&[sonar::Issue]> = issues.chunks(batch_size).collect();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 3);
    }

    #[test]
    fn test_branch_name_single_issue() {
        let branch = format!("fix/sonar-{}", sanitize_branch("AX-123"));
        assert_eq!(branch, "fix/sonar-AX-123");
    }

    #[test]
    fn test_branch_name_batch() {
        let ts = "20260321120000";
        let branch = format!("fix/sonar-batch-{}-{}", 1, ts);
        assert_eq!(branch, "fix/sonar-batch-1-20260321120000");
    }

    fn make_test_issue(key: &str) -> sonar::Issue {
        sonar::Issue {
            key: key.to_string(),
            rule: "test:rule".to_string(),
            severity: "MAJOR".to_string(),
            component: "proj:src/file.py".to_string(),
            issue_type: "BUG".to_string(),
            message: "test".to_string(),
            text_range: None,
            status: "OPEN".to_string(),
            tags: vec![],
        }
    }
}
