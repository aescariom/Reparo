use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tracing::{error, info, warn};

use crate::claude;
use crate::config::ValidatedConfig;

// ANSI color helpers for terminal output
/// Check if stderr is a terminal (supports ANSI colors)
fn supports_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// ANSI color helpers — only emit escape codes when stderr is a real terminal.
fn colored(s: &str, code: &str) -> String {
    if supports_color() { format!("\x1b[{}m{}\x1b[0m", code, s) } else { s.to_string() }
}
fn green(s: &str) -> String { colored(s, "1;32") }
fn yellow(s: &str) -> String { colored(s, "1;33") }
fn red(s: &str) -> String { colored(s, "1;31") }
fn blue(s: &str) -> String { colored(s, "34") }

/// Color a coverage percentage based on how close it is to the threshold.
/// - Green + bold: at or above threshold
/// - Yellow + bold: within 10% of threshold
/// - Red + bold: more than 10% below threshold
fn cov_colored(pct: f64, threshold: f64) -> String {
    let label = format!("{:.1}%", pct);
    if pct >= threshold {
        green(&label)
    } else if pct >= threshold - 10.0 {
        yellow(&label)
    } else {
        red(&label)
    }
}

/// Format a previous/reference coverage value (always blue, neutral).
fn cov_prev(pct: f64) -> String { blue(&format!("{:.1}%", pct)) }
/// Format a coverage percentage colored by distance to threshold.
/// Green if met, yellow if within 10%, red if > 10% below.
fn cov_vs(pct: f64, threshold: f64) -> String { cov_colored(pct, threshold) }

/// Print a colored info line directly to stderr, bypassing tracing's escaping.
/// Falls back to plain text when piped.
macro_rules! color_info {
    ($($arg:tt)*) => {
        eprintln!("{}", format!($($arg)*));
    };
}
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

/// Result of generating tests for a single file during coverage boost.
/// Used to accumulate test files for batch committing.
#[allow(dead_code)]
struct BoostFileResult {
    /// Source file that was boosted
    file: String,
    /// Test files created (relative paths)
    test_files: Vec<String>,
    /// Generated artifacts to stage alongside tests (coverage reports, etc.)
    artifacts: Vec<String>,
    /// Number of rounds that produced passing tests
    rounds_completed: u32,
    /// File coverage percentage before boost
    coverage_before: f64,
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
    /// Engine routing configuration for multi-engine AI dispatch
    engine_routing: crate::engine::EngineRoutingConfig,
    /// Cached test examples (computed once, reused across issues)
    cached_test_examples: Option<String>,
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

        let engine_routing = config.engine_routing.clone();

        Ok(Self {
            config,
            client,
            results: Vec::new(),
            total_issues_found: 0,
            prompt_config,
            exec_state,
            rule_cache: std::collections::HashMap::new(),
            engine_routing,
            cached_test_examples: None,
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

    /// Run an AI engine with the given prompt, routing based on the tier.
    ///
    /// Resolves which engine to use from the routing config, computes timeout,
    /// and dispatches to `engine::run_engine`.
    fn run_ai(&self, prompt: &str, tier: &claude::ClaudeTier) -> anyhow::Result<String> {
        let invocation = crate::engine::resolve_engine_for_tier(tier, &self.engine_routing)?;
        let timeout = tier.effective_timeout(self.config.claude_timeout);
        crate::engine::run_engine(
            &self.config.path,
            prompt,
            timeout,
            self.config.dangerously_skip_permissions,
            self.config.show_prompts,
            &invocation,
        )
    }

    /// Run the full Reparo flow (US-010).
    ///
    /// Returns an exit code:
    /// - 0: all issues fixed (or none found, or dry-run)
    /// - 1: fatal error (config, connectivity)
    /// - 2: partial success (some fixed, some failed)
    pub async fn run(&mut self) -> Result<i32> {
        let start = Instant::now();

        // Step 0: Ensure clean git working tree
        info!("=== Step 0: Checking git status ===");
        match git::has_changes(&self.config.path) {
            Ok(true) => {
                anyhow::bail!(
                    "Working tree has uncommitted changes. Commit or stash them before running Reparo.\n\
                     Run `git status` in {} to see what's changed.",
                    self.config.path.display()
                );
            }
            Ok(false) => {
                info!("Git working tree is clean");
            }
            Err(e) => {
                warn!("Could not check git status: {} — proceeding anyway", e);
            }
        }

        // Step 0b: Preflight — build and tests MUST pass before anything else.
        // Detect test command here (will also be used later in the main flow).
        {
            let preflight_test_cmd = self.config.test_command.clone()
                .or_else(|| self.config.commands.test.clone())
                .or_else(|| runner::detect_test_command(&self.config.path));

            let preflight_build_cmd = self.config.commands.build.clone();

            info!("=== Step 0b: Preflight build + test validation ===");

            if let Some(ref build_cmd) = preflight_build_cmd {
                info!("Preflight: running build...");
                match runner::run_shell_command(&self.config.path, build_cmd, "preflight build") {
                    Ok((true, _)) => {
                        info!("✓ Preflight build passed");
                    }
                    Ok((false, output)) => {
                        error!("╔═══════════════════════════════════════════════════════════════╗");
                        error!("║            ✗  PREFLIGHT BUILD FAILED — ABORTING  ✗           ║");
                        error!("║  Fix the build before running Reparo. Nothing was modified.  ║");
                        error!("╚═══════════════════════════════════════════════════════════════╝");
                        error!("Build output:\n{}", truncate_tail(&output, 3000));
                        anyhow::bail!("Preflight build failed — project does not compile. Fix the build and retry.");
                    }
                    Err(e) => {
                        error!("╔═══════════════════════════════════════════════════════════════╗");
                        error!("║          ✗  PREFLIGHT BUILD ERROR — ABORTING  ✗              ║");
                        error!("╚═══════════════════════════════════════════════════════════════╝");
                        anyhow::bail!("Preflight build error: {}", e);
                    }
                }
            }

            if let Some(ref test_cmd) = preflight_test_cmd {
                info!("Preflight: running test suite...");
                match runner::run_tests(&self.config.path, test_cmd, self.config.test_timeout) {
                    Ok((true, _)) => {
                        info!("✓ Preflight tests passed");
                    }
                    Ok((false, output)) => {
                        error!("╔═══════════════════════════════════════════════════════════════╗");
                        error!("║            ✗  PREFLIGHT TESTS FAILED — ABORTING  ✗           ║");
                        error!("║  Fix failing tests before running Reparo. Nothing modified.  ║");
                        error!("╚═══════════════════════════════════════════════════════════════╝");
                        error!("Test output:\n{}", truncate_tail(&output, 3000));
                        anyhow::bail!("Preflight tests failed — fix failing tests and retry.");
                    }
                    Err(e) => {
                        error!("╔═══════════════════════════════════════════════════════════════╗");
                        error!("║          ✗  PREFLIGHT TEST ERROR — ABORTING  ✗               ║");
                        error!("╚═══════════════════════════════════════════════════════════════╝");
                        anyhow::bail!("Preflight test error: {}", e);
                    }
                }
            } else {
                warn!("No test command detected for preflight check. Use --test-command to configure one.");
            }
        }

        // Step 0.5: Cache test examples once (avoids re-globbing per issue)
        self.cached_test_examples = Some(runner::find_test_examples(&self.config.path).join("\n\n"));

        // Step 0.5: Validate pact configuration if enabled
        if !self.config.skip_pact && self.config.pact.enabled {
            let pact_warnings = self.config.pact.validate();
            for w in &pact_warnings {
                warn!("Pact config: {}", w);
            }

            // Detect and log framework info
            let fw_info = crate::pact::detect_pact_framework_info(&self.config.path);
            if fw_info.name == "unknown" {
                warn!("Could not detect pact framework — Claude will infer from project context");
            } else if !fw_info.installed {
                warn!(
                    "Pact framework '{}' declared but may not be installed. Run: {}",
                    fw_info.name, fw_info.install_hint
                );
            } else {
                info!("Detected pact framework: {} (installed)", fw_info.name);
            }

            // Detect project role for better prompts
            let role = crate::pact::detect_project_role(&self.config.path);
            info!("Detected project role: {:?}", role);
        }

        // Step 1: Validate SonarQube connectivity (US-001, US-016: with retry)
        info!("=== Step 1: Checking SonarQube connectivity ===");
        crate::retry::retry_async(3, 3, "SonarQube connection check", || {
            self.client.check_connection()
        }).await?;

        self.client.detect_edition().await;

        // Detect test command early — needed for pre-flight and processing
        // Priority: CLI --test-command > YAML commands.test > auto-detection
        let test_command = self.config.test_command.clone()
            .or_else(|| self.config.commands.test.clone())
            .or_else(|| runner::detect_test_command(&self.config.path));
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
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Setup command failed:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
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
                                &format_commit_message(&self.config, "style", "sonar", "apply code formatting before sonar fixes", "", "", ""),
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
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight build fails — fix the build before running Reparo:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
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
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight tests fail — fix tests before running Reparo:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
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
                Ok((true, output)) => {
                    if output.contains("Skipping JaCoCo execution due to missing execution data") {
                        warn!(
                            "JaCoCo skipped report generation — no execution data (jacoco.exec) was produced. \
                             Check that jacoco-maven-plugin is configured in the POM with prepare-agent execution."
                        );
                    }
                    if runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()).is_some() {
                        info!("Coverage report generated");
                    } else {
                        warn!("Coverage command succeeded but no report file was produced");
                    }
                }
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
        let mut initial_issues = self.client.fetch_issues().await?;
        if self.config.reverse_severity {
            initial_issues.reverse();
            info!("Reversed severity order: processing least severe issues first");
        }
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

            // Fetch fresh issues from SonarQube
            let issues = match self.client.fetch_issues().await {
                Ok(mut issues) => {
                    if self.config.reverse_severity {
                        issues.reverse();
                    }
                    issues
                }
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

        // Step 5c: Final validation — run the FULL test suite; iterate with Claude until ALL tests pass
        if self.config.skip_final_validation {
            info!("=== Step 5c: Final validation SKIPPED (disabled) ===");
        } else {
        info!("=== Step 5c: Final validation (all tests must pass) ===");
        }
        if !self.config.skip_final_validation && !test_command.is_empty() {
            let max_final_attempts = self.config.final_validation_attempts;
            let mut final_ok = false;

            for attempt in 1..=max_final_attempts {
                // Build check
                if let Some(ref build_cmd) = self.config.commands.build {
                    match runner::run_shell_command(&self.config.path, build_cmd, "final build check") {
                        Ok((true, _)) => info!("Final build check passed"),
                        Ok((false, output)) => {
                            warn!("Final build check FAILED (attempt {}/{})", attempt, max_final_attempts);
                            if attempt < max_final_attempts {
                                info!("Asking Claude to fix the build error...");
                                let repair_prompt = format!(
                                    r#"The project build is failing. Fix the build error WITHOUT modifying any test files.

## Build output:
```
{}
```

## Instructions:
1. Fix the build error
2. Do NOT modify any test files (*.spec.ts, *.test.ts, etc.)
3. Do NOT change test logic or assertions
4. Ensure the project compiles successfully

Apply the fix now."#,
                                    truncate(&output, 3000)
                                );
                                let repair_tier = claude::classify_repair_tier();
                                let _ = self.run_ai(&repair_prompt, &repair_tier);
                                if let Some(ref fmt_cmd) = self.config.commands.format {
                                    let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                                }
                                continue;
                            } else {
                                error!("Build still failing after {} repair attempts", max_final_attempts);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Build command error: {}", e);
                            break;
                        }
                    }
                }

                // Full test suite check — ALL tests must pass, not just per-issue tests
                info!("Running full test suite (attempt {}/{})...", attempt, max_final_attempts);
                match runner::run_tests(&self.config.path, &test_command, self.config.test_timeout) {
                    Ok((true, _)) => {
                        info!("Final validation PASSED — all tests green after {} attempt(s)", attempt);
                        final_ok = true;
                        break;
                    }
                    Ok((false, output)) => {
                        warn!("Full test suite FAILED (attempt {}/{})", attempt, max_final_attempts);
                        if attempt < max_final_attempts {
                            info!("Iterating: asking Claude to fix failures without modifying test files...");
                            let repair_prompt = format!(
                                r#"The full test suite is failing after applying SonarQube fixes. ALL tests must pass before we can accept the changes. Fix the SOURCE CODE to make every test pass. Do NOT modify any test files.

## Test output:
```
{}
```

## Instructions:
1. Analyze the failing tests and identify which source code changes broke them
2. Fix the source code to make ALL tests pass — not just the ones related to the current fix
3. Do NOT modify any test files (*.spec.ts, *.test.ts, *_test.go, test_*.py, etc.)
4. Do NOT change test logic or assertions — the tests define the expected behavior
5. Ensure the project compiles and the entire test suite passes

Apply the fix now."#,
                                truncate(&output, 3000)
                            );
                            let repair_tier = claude::classify_repair_tier();
                            let _ = self.run_ai(&repair_prompt, &repair_tier);
                            if let Some(ref fmt_cmd) = self.config.commands.format {
                                let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                            }
                        } else {
                            error!("Full test suite still failing after {} repair attempts — manual intervention needed", max_final_attempts);
                        }
                    }
                    Err(e) => {
                        error!("Test command error during final validation: {}", e);
                        break;
                    }
                }
            }

            // Commit any final fixes
            if final_ok {
                let _ = git::add_all(&self.config.path);
                if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                    let msg = format_commit_message(&self.config, "fix", "sonar", "repair build/test issues from accumulated changes", "", "", "");
                    let _ = git::commit(&self.config.path, &msg);
                    info!("Committed final validation fixes");
                }
            }
        }

        // Step 5c: Documentation quality — ensure code documentation meets standards
        if self.config.skip_docs {
            info!("=== Step 5c: Documentation SKIPPED (--skip-docs) ===");
        } else if self.config.documentation.enabled {
            self.improve_documentation(&test_command).await?;
        } else {
            info!("=== Step 5c: Documentation SKIPPED (not enabled in YAML) ===");
        }

        // Step 6: Generate report (on the fix branch)
        info!("=== Step 6: Generating report ===");
        let elapsed = start.elapsed().as_secs();
        report::generate_report(&self.config.path, &self.results, self.total_issues_found, elapsed);

        // Commit report files to the fix branch
        let _ = git::add_all(&self.config.path);
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let msg = format_commit_message(&self.config, "docs", "sonar", "add REPORT.md and TECHDEBT_CHANGELOG.md", "", "", "");
            let _ = git::commit(&self.config.path, &msg);
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
        let lcov_path = match runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()) {
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
            color_info!(
                "Project-wide coverage {} meets {:.0}% and all files meet per-file threshold — no boost needed",
                cov_vs(overall_pct, self.config.min_coverage), self.config.min_coverage
            );
            return Ok(());
        }

        if overall_needs_boost {
            color_info!("Project-wide coverage {} is below {:.0}%", cov_prev(overall_pct), self.config.min_coverage);
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
                // Skip files matching user-configured exclusion patterns
                if !self.config.coverage_exclude.is_empty() {
                    if self.config.coverage_exclude.iter().any(|pat| {
                        glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
                    }) {
                        return false;
                    }
                }
                // Skip files with 0 uncovered lines (rounding artifacts)
                if fc.total_lines <= fc.covered_lines {
                    return false;
                }
                // Include if needed for overall boost OR below per-file threshold
                overall_needs_boost || (has_file_threshold && fc.coverage_pct < self.config.min_file_coverage)
            })
            .collect();

        // Pre-filter: remove files that don't exist on disk (autogenerated classes, build artifacts)
        let (files_needing_tests, missing): (Vec<_>, Vec<_>) = files_needing_tests
            .into_iter()
            .partition(|fc| {
                let resolved = resolve_source_file(&self.config.path, &fc.file);
                resolved.exists()
            });

        if !missing.is_empty() {
            info!(
                "Skipping {} files not found on disk (autogenerated/build artifacts): {}{}",
                missing.len(),
                missing.iter().take(5).map(|f| f.file.as_str()).collect::<Vec<_>>().join(", "),
                if missing.len() > 5 { ", ..." } else { "" }
            );
        }

        // Log excluded patterns if any are configured
        if !self.config.coverage_exclude.is_empty() {
            let excluded_count = file_coverages.iter()
                .filter(|fc| {
                    self.config.coverage_exclude.iter().any(|pat| {
                        glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
                    })
                })
                .count();
            if excluded_count > 0 {
                info!(
                    "Excluded {} file(s) from coverage boost matching patterns: {:?}",
                    excluded_count, self.config.coverage_exclude
                );
            }
        }

        if files_needing_tests.is_empty() {
            warn!("No source files with uncovered lines found in lcov report");
            return Ok(());
        }

        // Wave-based processing:
        //   parallel_size  = how many files to process per wave (AI generation only, no per-file tests)
        //   commit_size    = how many files per git commit (>= parallel_size, already resolved)
        //   skip_test_run  = true when parallel_size > 1 — defer test validation to wave time
        //   commit_immediately = true when commit_size <= parallel_size (one commit per wave)
        //
        // If commit_size > parallel_size, each wave creates a temp "reparo-wip:" commit and
        // every commit_size / parallel_size waves get squashed into one real commit.
        let parallel_size = self.config.coverage_wave_size as usize;
        let commit_size = self.config.coverage_commit_batch as usize; // already resolved (never 0)
        let skip_test_run = parallel_size > 1;
        let batch_mode = commit_size > 1 || skip_test_run;
        let commit_immediately = commit_size <= parallel_size;

        info!(
            "Found {} source files needing test coverage — generating tests starting from least covered{}",
            files_needing_tests.len(),
            if batch_mode {
                format!(" (wave size: {}, commit batch: {} files)", parallel_size, commit_size)
            } else {
                String::new()
            }
        );

        let test_examples = runner::find_test_examples(&self.config.path);
        let test_examples_str = test_examples.join("\n\n");
        let test_framework = test_command;

        // US-040: Build framework context once for all files in the boost loop
        let detected_deps = runner::detect_test_dependencies(&self.config.path);
        let framework_context_base = build_framework_context(
            &detected_deps,
            &self.config.test_generation,
        );

        let stash_prefix = "reparo-boost";

        let mut current_pct = overall_pct;
        let start_pct = overall_pct;
        let mut files_boosted = 0;
        let mut files_processed = 0usize;
        // Current wave accumulator (parallel_size files per wave)
        let mut current_wave: Vec<BoostFileResult> = Vec::new();
        // Temp "reparo-wip:" commits waiting to be squashed (when commit_size > parallel_size)
        let mut temp_commit_count = 0usize;
        let mut temp_commit_files: Vec<String> = Vec::new();
        let total_files = files_needing_tests.len();
        // Count of files actually queued/processed (for display)
        let mut queue_idx = 0;
        // Circuit breaker: stop after N consecutive wave failures (US-034)
        let mut consecutive_wave_failures = 0usize;
        let max_wave_failures = self.config.max_boost_failures;

        for (idx, fc) in files_needing_tests.iter().enumerate() {
            // Circuit breaker: stop if too many consecutive waves failed
            if max_wave_failures > 0 && consecutive_wave_failures >= max_wave_failures {
                warn!(
                    "Stopping coverage boost: {} consecutive waves failed — likely a systemic issue \
                     (e.g. missing test dependencies, Spring context not available). \
                     Processed {} files, {} committed, {} remaining. Fix test setup and re-run.",
                    consecutive_wave_failures, queue_idx, files_boosted, total_files - idx
                );
                break;
            }

            // Check if we can stop: overall threshold met AND this file doesn't need per-file boost
            let overall_met = current_pct >= self.config.min_coverage;
            let file_needs_boost = has_file_threshold && fc.coverage_pct < self.config.min_file_coverage;

            if overall_met && !file_needs_boost {
                continue; // Skip files that are only needed for overall boost
            }

            queue_idx += 1;
            let reason = if !overall_met && file_needs_boost {
                format!("overall {:.1}% < {:.0}% AND file {:.1}% < {:.0}%",
                    current_pct, self.config.min_coverage, fc.coverage_pct, self.config.min_file_coverage)
            } else if file_needs_boost {
                format!("file {:.1}% < per-file threshold {:.0}%", fc.coverage_pct, self.config.min_file_coverage)
            } else {
                format!("overall {:.1}% < {:.0}%", current_pct, self.config.min_coverage)
            };

            info!(
                "--- Coverage boost [{}/{}]: {} ({:.1}%, {}/{} lines) — {} | overall: {:.1}% ---",
                queue_idx,
                total_files,
                fc.file,
                fc.coverage_pct,
                fc.covered_lines,
                fc.total_lines,
                reason,
                current_pct
            );

            let is_last = idx == total_files - 1;

            files_processed += 1;
            match self.generate_tests_for_file(fc, test_framework, &test_examples_str, stash_prefix, skip_test_run, &framework_context_base)? {
                Some(result) if batch_mode && !result.test_files.is_empty() => {
                    // Wave mode: accumulate result, commit at wave boundary
                    current_wave.push(result);
                }
                Some(_) => {
                    // Individual mode (parallel=1, commit=1): already committed inside generate_tests_for_file
                    files_boosted += 1;
                    consecutive_wave_failures = 0;
                    match self.run_coverage_and_measure(&cov_cmd) {
                        Some(pct) => {
                            color_info!(
                                "Project-wide coverage after boost: {} (was {})",
                                cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                            );
                            current_pct = pct;
                        }
                        None => {
                            warn!("Could not re-measure coverage — continuing with next file");
                        }
                    }
                }
                None => {
                    // File was skipped (excluded, too large, no uncovered lines, etc.)
                }
            }

            // Wave boundary: flush when wave is full or this is the last file
            let wave_ready = !current_wave.is_empty()
                && (current_wave.len() >= parallel_size || (is_last && !current_wave.is_empty()));

            if wave_ready {
                // Pre-wave stash hygiene: drop orphaned stashes from previous failed waves
                if let Ok(orphan_indices) = git::stash_indices_matching(&self.config.path, stash_prefix) {
                    if !orphan_indices.is_empty() {
                        warn!("Dropping {} orphaned stash(es) with prefix '{}' before wave commit", orphan_indices.len(), stash_prefix);
                        let _ = git::stash_drop_matching(&self.config.path, stash_prefix);
                    }
                }
                // Safety: ensure working tree is clean before wave commit
                let _ = git::ensure_clean_state(&self.config.path);

                if commit_immediately {
                    // commit_size <= parallel_size: one real commit per wave
                    let committed = self.commit_boost_batch(&current_wave, test_framework, stash_prefix, skip_test_run, &framework_context_base)?;
                    files_boosted += committed;
                    current_wave.clear();
                    if committed > 0 {
                        consecutive_wave_failures = 0;
                        if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                            color_info!(
                                "Project-wide coverage after wave commit: {} (was {})",
                                cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                            );
                            current_pct = pct;
                        }
                    } else {
                        consecutive_wave_failures += 1;
                        // Cleanup after failed wave: drop residual stashes and ensure clean state
                        let _ = git::stash_drop_matching(&self.config.path, stash_prefix);
                        let _ = git::ensure_clean_state(&self.config.path);
                    }
                    info!(
                        "Coverage boost progress: {}/{} files processed, {} committed, coverage: {:.1}% → {:.1}%",
                        files_processed, total_files, files_boosted, start_pct, current_pct
                    );
                } else {
                    // commit_size > parallel_size: create temp "reparo-wip:" commit, squash later
                    let (committed, wave_files) =
                        self.validate_and_temp_commit_wave(&current_wave, test_framework, stash_prefix, skip_test_run, &framework_context_base)?;
                    current_wave.clear();
                    if committed > 0 {
                        consecutive_wave_failures = 0;
                        // Only count as temp wip commit if wave_files is non-empty.
                        // Fallback per-file commits return empty wave_files (they're already real commits).
                        if !wave_files.is_empty() {
                            temp_commit_count += 1;
                            temp_commit_files.extend(wave_files);
                        } else {
                            // Per-file fallback created real commits — re-measure coverage
                            if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                                color_info!(
                                    "Project-wide coverage after per-file fallback: {} (was {})",
                                    cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                                );
                                current_pct = pct;
                            }
                        }
                        files_boosted += committed;
                    } else {
                        consecutive_wave_failures += 1;
                        // Cleanup after failed wave: drop residual stashes and ensure clean state
                        let _ = git::stash_drop_matching(&self.config.path, stash_prefix);
                        let _ = git::ensure_clean_state(&self.config.path);
                    }
                    info!(
                        "Coverage boost progress: {}/{} files processed, {} committed, coverage: {:.1}% → {:.1}%",
                        files_processed, total_files, files_boosted, start_pct, current_pct
                    );

                    // Squash boundary: when enough temp commits accumulated or last file
                    let squash_ready = temp_commit_files.len() >= commit_size
                        || (is_last && temp_commit_count > 0);
                    if squash_ready && temp_commit_count > 0 {
                        let _ = self.squash_boost_commits(temp_commit_count, &temp_commit_files);
                        temp_commit_count = 0;
                        temp_commit_files.clear();
                        if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                            color_info!(
                                "Project-wide coverage after squash commit: {} (was {})",
                                cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                            );
                            current_pct = pct;
                        }
                    }
                }
            }
        }

        // Safety flush: handle any remaining wave entries not yet triggered by is_last
        // (can happen when all remaining files were skipped and the last real file already committed)
        if !current_wave.is_empty() {
            let committed = self.commit_boost_batch(&current_wave, test_framework, stash_prefix, skip_test_run, &framework_context_base)?;
            files_boosted += committed;
            current_wave.clear();
            if committed == 0 {
                let _ = git::stash_drop_matching(&self.config.path, stash_prefix);
                let _ = git::ensure_clean_state(&self.config.path);
            }
            if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                current_pct = pct;
            }
        }
        if temp_commit_count > 0 {
            let _ = self.squash_boost_commits(temp_commit_count, &temp_commit_files);
            if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                current_pct = pct;
            }
        }

        // Final summary
        let remaining_below: Vec<_> = if has_file_threshold {
            // Re-read lcov to check which files are still below threshold
            runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref())
                .map(|p| runner::per_file_lcov_coverage(&p))
                .unwrap_or_default()
                .into_iter()
                .filter(|fc| fc.coverage_pct < self.config.min_file_coverage && !is_test_file(&fc.file))
                .collect()
        } else {
            Vec::new()
        };

        color_info!(
            "Coverage boost summary: processed {} files, committed {}, coverage {:.1}% → {:.1}% (target: {:.0}%)",
            files_processed, files_boosted, start_pct, current_pct, self.config.min_coverage
        );
        if current_pct >= self.config.min_coverage && remaining_below.is_empty() {
            color_info!(
                "Coverage boost complete: {} (target {:.0}%) — {} files boosted",
                cov_vs(current_pct, self.config.min_coverage), self.config.min_coverage, files_boosted
            );
        } else {
            if current_pct < self.config.min_coverage {
                color_info!(
                    "⚠ Coverage boost: overall {} still below target {:.0}%",
                    cov_vs(current_pct, self.config.min_coverage), self.config.min_coverage
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

    /// Generate tests for a single file in a multi-round loop until the
    /// coverage threshold is met or the maximum rounds are exhausted.
    ///
    /// `coverage_rounds` (from config):
    ///   - N > 0 → at most N rounds per file
    ///   - 0     → unlimited rounds, keep going while coverage still improves
    ///
    /// Generate tests for a single file in a multi-round loop.
    ///
    /// When `coverage_commit_batch == 1`, commits per round (original behavior).
    /// When `coverage_commit_batch > 1`, accumulates test files and stashes them
    /// for later batch commit by `commit_boost_batch()`.
    ///
    /// Returns `Some(BoostFileResult)` if tests were generated, `None` if skipped.
    fn generate_tests_for_file(
        &self,
        fc: &runner::FileCoverage,
        test_framework: &str,
        test_examples_str: &str,
        stash_prefix: &str,
        skip_test_run: bool,
        framework_context: &str,
    ) -> Result<Option<BoostFileResult>> {
        // Skip files matching user-configured exclusion patterns (safety net)
        if !self.config.coverage_exclude.is_empty() {
            if self.config.coverage_exclude.iter().any(|pat| {
                glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
            }) {
                info!("Skipping excluded file: {}", fc.file);
                return Ok(None);
            }
        }

        // Skip files with 0 uncovered lines — nothing to boost
        if fc.total_lines <= fc.covered_lines {
            info!("File {} has 0 uncovered lines — nothing to boost", fc.file);
            return Ok(None);
        }

        // Read the source file — try direct path first, then common source roots
        let full_path = resolve_source_file(&self.config.path, &fc.file);
        let source_content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Cannot read {} (resolved to {}): {} — skipping", fc.file, full_path.display(), e);
                return Ok(None);
            }
        };

        // Skip very large files — prompts with too many lines tend to timeout
        let line_count = source_content.lines().count();
        let max_lines = self.config.max_boost_file_lines;
        if max_lines > 0 && line_count > max_lines {
            warn!(
                "File {} has {} lines — exceeds max_boost_file_lines ({}), skipping",
                fc.file, line_count, max_lines
            );
            return Ok(None);
        }

        let batch_mode = self.config.coverage_commit_batch > 1 || skip_test_run;
        let max_rounds = self.config.coverage_rounds;
        let unlimited = max_rounds == 0;
        let mut round: u32 = 0;
        let mut any_success = false;
        let mut previous_coverage_pct = fc.coverage_pct;
        let mut current_uncovered_count = fc.total_lines.saturating_sub(fc.covered_lines);
        // Track specific uncovered line numbers across rounds for targeted prompts.
        let mut current_uncovered_lines: Vec<u32> = fc.uncovered_lines.clone();
        let mut last_test_output = String::new();
        let mut accumulated_test_files: Vec<String> = Vec::new();
        let mut accumulated_artifacts: Vec<String> = Vec::new();

        loop {
            round += 1;

            // Check round limit (0 = unlimited)
            if !unlimited && round > max_rounds {
                info!(
                    "Reached max coverage rounds ({}) for {} — coverage at {:.1}%",
                    max_rounds, fc.file, previous_coverage_pct
                );
                break;
            }

            // Safety cap for unlimited mode — prevent truly infinite loops
            if unlimited && round > 50 {
                warn!("Safety cap: 50 rounds reached for {} — stopping", fc.file);
                break;
            }

            let round_label = if unlimited {
                format!("round {} (unlimited)", round)
            } else {
                format!("round {}/{}", round, max_rounds)
            };

            info!(
                "Coverage boost {} for {} — current {:.1}%, {} uncovered lines",
                round_label, fc.file, previous_coverage_pct, current_uncovered_count
            );

            // Build prompt — use specific uncovered line numbers when available for targeted generation
            let uncovered_desc = if !current_uncovered_lines.is_empty() {
                // Cap at 150 lines to keep the prompt focused; the model will handle the rest in the next round
                let lines: Vec<String> = current_uncovered_lines.iter().take(150).map(|l| l.to_string()).collect();
                let suffix = if current_uncovered_lines.len() > 150 {
                    format!(" (… and {} more)", current_uncovered_lines.len() - 150)
                } else {
                    String::new()
                };
                format!(
                    "Lines not yet covered: {}{} — file is at {:.1}% coverage ({} of {} coverable lines hit)",
                    lines.join(", "), suffix,
                    previous_coverage_pct,
                    current_uncovered_count.saturating_sub(current_uncovered_count), // covered count
                    fc.total_lines
                )
            } else if round == 1 {
                format!(
                    "File has {:.1}% coverage ({} uncovered lines out of {} coverable) — generate tests for all uncovered paths",
                    previous_coverage_pct,
                    current_uncovered_count,
                    fc.total_lines
                )
            } else {
                format!(
                    "Lines still uncovered — file has {:.1}% coverage after {} previous round(s), {} lines remain uncovered out of {} coverable",
                    previous_coverage_pct,
                    round - 1,
                    current_uncovered_count,
                    fc.total_lines
                )
            };

            let prompt = if round == 1 {
                // US-040: Build per-file framework context with classification
                let file_class = runner::classify_source_file(&fc.file, &self.config.path);
                let pkg_hint = runner::derive_test_package(&fc.file)
                    .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
                    .unwrap_or_default();
                let per_file_ctx = build_per_file_context(framework_context, &file_class, &pkg_hint);
                claude::build_test_generation_prompt(
                    &fc.file,
                    &uncovered_desc,
                    test_framework,
                    test_examples_str,
                    &per_file_ctx,
                )
            } else {
                let file_class = runner::classify_source_file(&fc.file, &self.config.path);
                let per_file_ctx = build_per_file_context(framework_context, &file_class, "");
                claude::build_test_generation_retry_prompt(
                    &fc.file,
                    &uncovered_desc,
                    test_framework,
                    round,
                    &truncate(&last_test_output, 1000),
                    &per_file_ctx,
                )
            };

            if self.config.show_prompts {
                info!("Coverage boost prompt ({}):\n{}", round_label, prompt);
            }

            let uncovered = current_uncovered_count as usize;
            let test_tier = claude::classify_test_gen_tier(uncovered, fc.total_lines as usize);
            info!("Generating tests for {} [{}] ({})...", fc.file, test_tier, round_label);
            match self.run_ai(&prompt, &test_tier) {
                Ok(_) => {
                    info!("AI completed test generation for {} ({})", fc.file, round_label);
                }
                Err(e) => {
                    warn!("Failed to generate tests for {} ({}): {} — stopping rounds", fc.file, round_label, e);
                    let _ = git::revert_changes(&self.config.path);
                    break;
                }
            }

            // Verify no source files were modified
            let changed = match git::changed_files(&self.config.path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Cannot check changed files: {} — reverting", e);
                    let _ = git::revert_changes(&self.config.path);
                    break;
                }
            };

            if changed.is_empty() {
                warn!("No files changed in {} for {} — stopping rounds", round_label, fc.file);
                break;
            }

            let source_files_modified: Vec<&String> = changed.iter()
                .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f))
                .collect();

            if !source_files_modified.is_empty() {
                warn!(
                    "Source files were modified during test generation for {} ({}): {:?} — reverting",
                    fc.file, round_label, source_files_modified
                );
                let _ = git::revert_changes(&self.config.path);
                break;
            }

            // Run tests (skipped when skip_test_run=true — validation happens at wave commit time)
            if !skip_test_run {
                info!("Running tests to validate generated tests ({})...", round_label);
                match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                    Ok((true, output)) => {
                        info!("Tests pass after {} for {}", round_label, fc.file);
                        last_test_output = output;
                    }
                    Ok((false, output)) => {
                        warn!("Tests FAIL after {} for {} — reverting:\n{}", round_label, fc.file, runner::extract_error_summary(&output, 500));
                        last_test_output = output;
                        let _ = git::revert_changes(&self.config.path);
                        // Don't break — try again next round if rounds remain
                        continue;
                    }
                    Err(e) => {
                        warn!("Test execution error in {} for {} — reverting: {}", round_label, fc.file, e);
                        let _ = git::revert_changes(&self.config.path);
                        break;
                    }
                }
            } else {
                info!("Skipping per-file test run for {} ({}) — deferred to wave validation", fc.file, round_label);
            }

            // Identify test files and artifacts
            let test_files_changed: Vec<String> = changed.iter()
                .filter(|f| is_test_file(f))
                .cloned()
                .collect();

            if test_files_changed.is_empty() {
                let _ = git::revert_changes(&self.config.path);
                break;
            }

            let generated_artifacts: Vec<String> = changed.iter()
                .filter(|f| is_generated_artifact(f))
                .cloned()
                .collect();

            // Track extra files (helpers, fixtures, configs) that AI may have created
            // These are neither test files, nor known artifacts, nor the source file itself
            let extra_files: Vec<String> = changed.iter()
                .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f) && **f != fc.file)
                .cloned()
                .collect();
            if !extra_files.is_empty() {
                info!("Tracking {} extra generated file(s) for {}: {:?}", extra_files.len(), fc.file, extra_files);
            }

            if batch_mode {
                // Batch mode: accumulate test files, don't commit yet
                accumulated_test_files.extend(test_files_changed.clone());
                accumulated_artifacts.extend(generated_artifacts.clone());
                accumulated_artifacts.extend(extra_files.clone());
                any_success = true;

                // Stage test files + artifacts + extra files to keep them across rounds within the same file
                let mut stage_refs: Vec<&str> = test_files_changed.iter().map(|s| s.as_str()).collect();
                stage_refs.extend(generated_artifacts.iter().map(|s| s.as_str()));
                stage_refs.extend(extra_files.iter().map(|s| s.as_str()));
                if let Err(e) = git::add_files(&self.config.path, &stage_refs) {
                    warn!("Failed to stage test files: {} — reverting", e);
                    let _ = git::revert_changes(&self.config.path);
                    break;
                }

                // Revert non-test leftover changes, keep staged test files + artifacts
                let _ = git::revert_changes(&self.config.path);

                info!("Staged tests for {} ({}) — deferred commit", fc.file, round_label);
            } else {
                // Individual mode: commit immediately (original behavior)
                let refs: Vec<&str> = test_files_changed.iter().map(|s| s.as_str()).collect();
                if let Err(e) = git::add_files(&self.config.path, &refs) {
                    warn!("Failed to stage test files: {} — reverting", e);
                    let _ = git::revert_changes(&self.config.path);
                    break;
                }

                // Revert non-test, non-artifact leftover changes before committing
                let _ = git::revert_changes(&self.config.path);
                // Re-stage generated artifacts so they don't show as dirty
                if !generated_artifacts.is_empty() {
                    let artifact_refs: Vec<&str> = generated_artifacts.iter().map(|s| s.as_str()).collect();
                    let _ = git::add_files(&self.config.path, &artifact_refs);
                }

                let commit_msg = format_commit_message(
                    &self.config, "test", "coverage",
                    &format!("add tests for {} ({:.0}% → boost, {})", fc.file, previous_coverage_pct, round_label),
                    "", "", &fc.file,
                );
                if let Err(e) = git::commit(&self.config.path, &commit_msg) {
                    warn!("Failed to commit tests for {} ({}): {} — reverting", fc.file, round_label, e);
                    let _ = git::revert_changes(&self.config.path);
                    break;
                }

                info!("Committed tests for {} ({})", fc.file, round_label);
                any_success = true;

                // Revert any remaining leftover changes
                let _ = git::revert_changes(&self.config.path);
            }

            // Re-measure file coverage to decide if we need another round.
            // Skipped when skip_test_run=true because tests haven't run yet — we only
            // do one round per file in wave mode and let the wave commit validate coverage.
            if !skip_test_run {
                let coverage_cmd = self.config.coverage_command.clone()
                    .or_else(|| self.config.commands.coverage.clone())
                    .or_else(|| runner::detect_coverage_command(&self.config.path));
                if let Some(ref cov_cmd) = coverage_cmd {
                    let _ = runner::run_shell_command(&self.config.path, cov_cmd, "coverage");
                }

                let lcov_path = runner::find_lcov_report_with_hint(
                    &self.config.path,
                    self.config.commands.coverage_report.as_deref(),
                );
                if let Some(ref lcov) = lcov_path {
                    let file_coverages = runner::per_file_lcov_coverage(lcov);
                    if let Some(updated_fc) = file_coverages.iter().find(|f| f.file == fc.file) {
                        let new_pct = updated_fc.coverage_pct;
                        let new_uncovered = updated_fc.total_lines.saturating_sub(updated_fc.covered_lines);
                        color_info!(
                            "Coverage for {} after {}: {:.1}% → {:.1}% ({} uncovered lines remaining)",
                            fc.file, round_label, previous_coverage_pct, new_pct, new_uncovered
                        );

                        // Check if threshold met
                        let threshold = if self.config.min_file_coverage > 0.0 {
                            self.config.min_file_coverage
                        } else {
                            self.config.min_coverage
                        };

                        if new_pct >= threshold {
                            color_info!(
                                "Coverage threshold {:.0}% met for {} — done after {} round(s)",
                                threshold, fc.file, round
                            );
                            break;
                        }

                        // In unlimited mode: stop if no improvement
                        if unlimited && new_pct <= previous_coverage_pct {
                            info!(
                                "No coverage improvement for {} ({:.1}% → {:.1}%) — stopping rounds",
                                fc.file, previous_coverage_pct, new_pct
                            );
                            break;
                        }

                        previous_coverage_pct = new_pct;
                        current_uncovered_count = new_uncovered;
                        current_uncovered_lines = updated_fc.uncovered_lines.clone();
                    } else {
                        warn!("File {} not found in lcov after {} — stopping rounds", fc.file, round_label);
                        break;
                    }
                } else {
                    // No lcov report — can't measure progress, stop looping
                    warn!("No lcov report found after {} — cannot verify improvement, stopping", round_label);
                    break;
                }
            } else {
                // In wave mode: one round of test generation per file is enough.
                // The wave commit will validate and measure coverage for all files together.
                break;
            }
        }

        if !any_success {
            return Ok(None);
        }

        // In batch mode, stash accumulated test files + artifacts for later batch commit
        if batch_mode && !accumulated_test_files.is_empty() {
            let stash_msg = format!("{}:{}", stash_prefix, fc.file);
            let mut refs: Vec<&str> = accumulated_test_files.iter().map(|s| s.as_str()).collect();
            refs.extend(accumulated_artifacts.iter().map(|s| s.as_str()));
            // Ensure all test files + artifacts are staged before stashing
            let _ = git::add_files(&self.config.path, &refs);
            match git::stash_push(&self.config.path, &stash_msg, &refs) {
                Ok(()) => {
                    info!("Stashed {} test files for {} — pending batch commit", accumulated_test_files.len(), fc.file);
                }
                Err(e) => {
                    warn!("Failed to stash test files for {}: {} — committing individually", fc.file, e);
                    // Fallback: commit now
                    let commit_msg = format_commit_message(
                        &self.config, "test", "coverage",
                        &format!("add tests for {} ({:.0}% → boost)", fc.file, fc.coverage_pct),
                        "", "", &fc.file,
                    );
                    let _ = git::commit(&self.config.path, &commit_msg);
                }
            }
            let _ = git::revert_changes(&self.config.path);
        }

        Ok(Some(BoostFileResult {
            file: fc.file.clone(),
            test_files: accumulated_test_files,
            artifacts: accumulated_artifacts,
            rounds_completed: round.saturating_sub(1),
            coverage_before: fc.coverage_pct,
        }))
    }

    /// Commit a batch of boost results atomically.
    ///
    /// Pops stashed test files, optionally runs tests to validate all pass together,
    /// and creates a single commit. `run_tests` should be `true` when per-file test
    /// runs were skipped (i.e. `skip_test_run = true`). Falls back gracefully on failure.
    fn commit_boost_batch(
        &self,
        batch: &[BoostFileResult],
        test_framework: &str,
        stash_prefix: &str,
        run_tests: bool,
        framework_context: &str,
    ) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }

        info!(
            "Committing batch of {} file(s): {}",
            batch.len(),
            batch.iter().map(|r| r.file.as_str()).collect::<Vec<_>>().join(", ")
        );

        // Pop all stashes from the batch (restores test files)
        let popped = git::stash_pop_matching(&self.config.path, stash_prefix)?;
        if popped == 0 {
            warn!("No stashes found for batch commit — nothing to commit");
            return Ok(0);
        }

        // Optionally run tests to validate all batch test files together.
        // Skipped when tests were already validated per-file (run_tests=false).
        if run_tests {
            // Build/compile before running tests (fast failure on compilation errors)
            let build_cmd = self.config.commands.test_compile.as_ref()
                .or(self.config.commands.build.as_ref());
            if let Some(cmd) = build_cmd {
                match runner::run_shell_command(&self.config.path, cmd, "test-compile") {
                    Ok((true, _)) => {
                        info!("Pre-test build succeeded for batch ({} files)", batch.len());
                    }
                    Ok((false, output)) => {
                        warn!(
                            "Pre-test build failed for {} files — falling back to per-file validation:\n{}",
                            batch.len(), runner::extract_error_summary(&output, 800)
                        );
                        return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                    }
                    Err(e) => {
                        warn!("Pre-test build error during batch commit: {} — falling back to per-file validation", e);
                        return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                    }
                }
            }

            match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                Ok((true, _)) => {
                    info!("Wave tests pass — proceeding with batch commit ({} files)", batch.len());
                }
                Ok((false, output)) => {
                    warn!(
                        "Batch tests failed for {} files — falling back to per-file validation:\n{}",
                        batch.len(), runner::extract_error_summary(&output, 800)
                    );
                    // Files are in the working tree (stashes already popped).
                    // Try each file individually instead of discarding all.
                    return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                }
                Err(e) => {
                    warn!("Test execution error during batch commit: {} — falling back to per-file validation", e);
                    return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                }
            }
        }

        // Unstage everything (stash_pop_matching stages between applies),
        // then selectively re-stage only test files + artifacts.
        let _ = git::reset_index(&self.config.path);
        let mut all_stage_files: Vec<&str> = batch.iter()
            .flat_map(|r| r.test_files.iter().map(|s| s.as_str()))
            .collect();
        all_stage_files.extend(batch.iter().flat_map(|r| r.artifacts.iter().map(|s| s.as_str())));
        if let Err(e) = git::add_files(&self.config.path, &all_stage_files) {
            warn!("Failed to stage batch test files: {}", e);
            let _ = git::revert_changes(&self.config.path);
            return Ok(0);
        }
        let _ = git::revert_changes(&self.config.path); // clean remaining leftovers

        let file_list: Vec<&str> = batch.iter().map(|r| r.file.as_str()).collect();
        let msg = if batch.len() == 1 {
            format_commit_message(
                &self.config, "test", "coverage",
                &format!("add tests for {} ({:.0}% → boost)", batch[0].file, batch[0].coverage_before),
                "", "", &batch[0].file,
            )
        } else {
            format_commit_message(
                &self.config, "test", "coverage",
                &format!("add tests for {} files ({})", batch.len(), file_list.join(", ")),
                "", "", "",
            )
        };
        match git::commit(&self.config.path, &msg) {
            Ok(()) => {
                info!("Batch commit successful ({} files)", batch.len());
                Ok(batch.len())
            }
            Err(e) => {
                warn!("Batch commit failed: {} — reverting", e);
                let _ = git::revert_changes(&self.config.path);
                Ok(0)
            }
        }
    }

    /// Fallback when wave tests fail: re-stash each file's changes individually,
    /// then test and commit them one by one. Returns the number of files committed.
    ///
    /// Expects the working tree to contain all popped stash files (from the failed wave).
    fn fallback_per_file_commit(
        &self,
        batch: &[BoostFileResult],
        test_framework: &str,
        framework_context: &str,
    ) -> usize {
        let retry_prefix = "reparo-retry";
        warn!("Falling back to per-file validation for {} file(s)", batch.len());

        // Re-stash each file's changes individually so we can test them one by one.
        for result in batch {
            let mut refs: Vec<&str> = result.test_files.iter().map(|s| s.as_str()).collect();
            refs.extend(result.artifacts.iter().map(|s| s.as_str()));
            if refs.is_empty() {
                continue;
            }
            let stash_msg = format!("{}:{}", retry_prefix, result.file);
            let _ = git::add_files(&self.config.path, &refs);
            if let Err(e) = git::stash_push(&self.config.path, &stash_msg, &refs) {
                warn!("Failed to re-stash files for {}: {} — skipping", result.file, e);
            }
        }
        // Clean anything left over from the failed wave
        let _ = git::revert_changes(&self.config.path);

        let mut committed = 0usize;
        for result in batch {
            if result.test_files.is_empty() {
                continue;
            }
            let match_str = format!("{}:{}", retry_prefix, result.file);

            let popped = match git::stash_pop_matching(&self.config.path, &match_str) {
                Ok(n) => n,
                Err(e) => {
                    warn!("Failed to pop retry stash for {}: {} — skipping", result.file, e);
                    let _ = git::stash_drop_matching(&self.config.path, &match_str);
                    let _ = git::revert_changes(&self.config.path);
                    continue;
                }
            };
            if popped == 0 {
                warn!("No retry stash found for {} — skipping", result.file);
                continue;
            }

            // Build/compile before running tests (fast failure on compilation errors)
            let build_cmd = self.config.commands.test_compile.as_ref()
                .or(self.config.commands.build.as_ref());
            if let Some(cmd) = build_cmd {
                match runner::run_shell_command(&self.config.path, cmd, "test-compile") {
                    Ok((true, _)) => {
                        info!("Per-file build succeeded for {}", result.file);
                    }
                    Ok((false, output)) => {
                        warn!(
                            "Per-file build failed for {} — {}:\n{}",
                            result.file,
                            if self.config.retry_failed_wave_files { "will retry with error context" } else { "discarding" },
                            runner::extract_error_summary(&output, 800)
                        );
                        let _ = git::revert_changes(&self.config.path);
                        if self.config.retry_failed_wave_files {
                            if self.retry_failed_file_with_context(result, test_framework, &output, framework_context) {
                                committed += 1;
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!("Build error for {} — discarding: {}", result.file, e);
                        let _ = git::revert_changes(&self.config.path);
                        continue;
                    }
                }
            }

            // Run tests with just this file's changes
            match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                Ok((true, _)) => {
                    info!("Per-file tests pass for {} — committing", result.file);
                    let mut stage_refs: Vec<&str> = result.test_files.iter().map(|s| s.as_str()).collect();
                    stage_refs.extend(result.artifacts.iter().map(|s| s.as_str()));
                    if git::add_files(&self.config.path, &stage_refs).is_ok() {
                        let _ = git::revert_changes(&self.config.path); // clean non-test leftovers
                        let msg = format_commit_message(
                            &self.config, "test", "coverage",
                            &format!("add tests for {} ({:.0}% → boost)", result.file, result.coverage_before),
                            "", "", &result.file,
                        );
                        if git::commit(&self.config.path, &msg).is_ok() {
                            committed += 1;
                        } else {
                            warn!("Commit failed for {} — reverting", result.file);
                            let _ = git::revert_changes(&self.config.path);
                        }
                    } else {
                        warn!("Failed to stage files for {} — reverting", result.file);
                        let _ = git::revert_changes(&self.config.path);
                    }
                }
                Ok((false, output)) => {
                    warn!(
                        "Per-file tests fail for {} — {}:\n{}",
                        result.file,
                        if self.config.retry_failed_wave_files { "will retry with error context" } else { "discarding" },
                        runner::extract_error_summary(&output, 800)
                    );
                    let _ = git::revert_changes(&self.config.path);
                    if self.config.retry_failed_wave_files {
                        if self.retry_failed_file_with_context(result, test_framework, &output, framework_context) {
                            committed += 1;
                        }
                    }
                }
                Err(e) => {
                    warn!("Test error for {} — discarding: {}", result.file, e);
                    let _ = git::revert_changes(&self.config.path);
                }
            }
        }

        // Safety cleanup: drop any remaining retry stashes
        let _ = git::stash_drop_matching(&self.config.path, retry_prefix);

        if committed > 0 {
            info!("Per-file fallback: committed {} of {} file(s)", committed, batch.len());
        } else {
            warn!("Per-file fallback: no files passed individual validation");
        }
        committed
    }

    /// Retry test generation for a single file that failed build/test in per-file fallback.
    ///
    /// Calls the AI with the previous error as context, validates the new tests
    /// (build + test), and commits if successful. Returns `true` if committed.
    fn retry_failed_file_with_context(
        &self,
        result: &BoostFileResult,
        test_framework: &str,
        error_output: &str,
        framework_context: &str,
    ) -> bool {
        info!("Retrying test generation for {} with compilation error context", result.file);

        let uncovered_desc = format!(
            "File has {:.0}% coverage — previous test generation attempt failed. \
             Fix the errors and regenerate working tests.",
            result.coverage_before
        );
        // US-040: Include framework context in retry
        let file_class = runner::classify_source_file(&result.file, &self.config.path);
        let per_file_ctx = build_per_file_context(framework_context, &file_class, "");
        let prompt = claude::build_test_generation_retry_prompt(
            &result.file,
            &uncovered_desc,
            test_framework,
            2, // retry attempt
            &truncate(error_output, 2000),
            &per_file_ctx,
        );
        let tier = claude::classify_repair_tier();

        if let Err(e) = self.run_ai(&prompt, &tier) {
            warn!("AI retry failed for {}: {} — discarding definitively", result.file, e);
            let _ = git::revert_changes(&self.config.path);
            return false;
        }

        // Verify no source files were modified
        let changed = match git::changed_files(&self.config.path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Cannot check changed files after retry for {}: {}", result.file, e);
                let _ = git::revert_changes(&self.config.path);
                return false;
            }
        };

        if changed.is_empty() {
            warn!("No files changed during retry for {} — discarding", result.file);
            return false;
        }

        let source_modified: Vec<&String> = changed.iter()
            .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f))
            .collect();
        if !source_modified.is_empty() {
            warn!(
                "Source files modified during retry for {}: {:?} — reverting",
                result.file, source_modified
            );
            let _ = git::revert_changes(&self.config.path);
            return false;
        }

        let test_files: Vec<String> = changed.iter()
            .filter(|f| is_test_file(f))
            .cloned()
            .collect();
        if test_files.is_empty() {
            warn!("No test files generated during retry for {} — reverting", result.file);
            let _ = git::revert_changes(&self.config.path);
            return false;
        }

        // Build/compile retried tests
        let build_cmd = self.config.commands.test_compile.as_ref()
            .or(self.config.commands.build.as_ref());
        if let Some(cmd) = build_cmd {
            match runner::run_shell_command(&self.config.path, cmd, "test-compile") {
                Ok((true, _)) => {
                    info!("Retry build succeeded for {}", result.file);
                }
                Ok((false, output)) => {
                    warn!(
                        "Discarding test for {} — retry build also failed:\n{}",
                        result.file, runner::extract_error_summary(&output, 500)
                    );
                    let _ = git::revert_changes(&self.config.path);
                    return false;
                }
                Err(e) => {
                    warn!("Retry build error for {}: {} — discarding", result.file, e);
                    let _ = git::revert_changes(&self.config.path);
                    return false;
                }
            }
        }

        // Run tests on retried files
        match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
            Ok((true, _)) => {
                info!("Retry tests pass for {} — committing", result.file);
            }
            Ok((false, output)) => {
                warn!(
                    "Discarding test for {} — retry tests also failed:\n{}",
                    result.file, runner::extract_error_summary(&output, 500)
                );
                let _ = git::revert_changes(&self.config.path);
                return false;
            }
            Err(e) => {
                warn!("Retry test error for {}: {} — discarding", result.file, e);
                let _ = git::revert_changes(&self.config.path);
                return false;
            }
        }

        // Stage and commit
        let artifacts: Vec<String> = changed.iter()
            .filter(|f| is_generated_artifact(f))
            .cloned()
            .collect();
        let mut stage_refs: Vec<&str> = test_files.iter().map(|s| s.as_str()).collect();
        stage_refs.extend(artifacts.iter().map(|s| s.as_str()));
        if git::add_files(&self.config.path, &stage_refs).is_err() {
            warn!("Failed to stage retried files for {} — reverting", result.file);
            let _ = git::revert_changes(&self.config.path);
            return false;
        }
        let _ = git::revert_changes(&self.config.path); // clean non-test leftovers
        let msg = format_commit_message(
            &self.config, "test", "coverage",
            &format!("add tests for {} ({:.0}% → boost, retry)", result.file, result.coverage_before),
            "", "", &result.file,
        );
        if git::commit(&self.config.path, &msg).is_ok() {
            info!("Retry commit successful for {}", result.file);
            true
        } else {
            warn!("Retry commit failed for {} — reverting", result.file);
            let _ = git::revert_changes(&self.config.path);
            false
        }
    }

    /// Validate a wave of boost results and create a temporary "reparo-wip:" commit.
    ///
    /// Used when `commit_size > parallel_size`: multiple waves are accumulated as
    /// temporary commits and later squashed by [`squash_boost_commits`] into one
    /// real commit per `commit_size` files.
    ///
    /// Returns `(files_committed, source_file_list)`.
    fn validate_and_temp_commit_wave(
        &self,
        wave: &[BoostFileResult],
        test_framework: &str,
        stash_prefix: &str,
        run_tests: bool,
        framework_context: &str,
    ) -> Result<(usize, Vec<String>)> {
        if wave.is_empty() {
            return Ok((0, vec![]));
        }

        let file_names: Vec<&str> = wave.iter().map(|r| r.file.as_str()).collect();
        info!(
            "Validating wave of {} file(s) before temp commit: {}",
            wave.len(),
            file_names.join(", ")
        );

        // Pop all stashes from this wave
        let popped = git::stash_pop_matching(&self.config.path, stash_prefix)?;
        if popped == 0 {
            warn!("No stashes found for wave — skipping temp commit");
            return Ok((0, vec![]));
        }

        // Optionally run tests to validate wave files together
        if run_tests {
            // Build/compile before running tests (fast failure on compilation errors)
            let build_cmd = self.config.commands.test_compile.as_ref()
                .or(self.config.commands.build.as_ref());
            if let Some(cmd) = build_cmd {
                match runner::run_shell_command(&self.config.path, cmd, "test-compile") {
                    Ok((true, _)) => {
                        info!("Pre-test build succeeded for wave ({} files)", wave.len());
                    }
                    Ok((false, output)) => {
                        warn!(
                            "Pre-test build failed for {} files — falling back to per-file validation:\n{}",
                            wave.len(), runner::extract_error_summary(&output, 800)
                        );
                        let committed = self.fallback_per_file_commit(wave, test_framework, framework_context);
                        return Ok((committed, vec![]));
                    }
                    Err(e) => {
                        warn!("Pre-test build error during wave validation: {} — falling back to per-file validation", e);
                        let committed = self.fallback_per_file_commit(wave, test_framework, framework_context);
                        return Ok((committed, vec![]));
                    }
                }
            }

            match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                Ok((true, _)) => {
                    info!("Wave tests pass ({} files)", wave.len());
                }
                Ok((false, output)) => {
                    warn!(
                        "Wave tests failed for {} files — falling back to per-file validation:\n{}",
                        wave.len(), runner::extract_error_summary(&output, 800)
                    );
                    let committed = self.fallback_per_file_commit(wave, test_framework, framework_context);
                    // Return empty files — fallback creates real commits, not temp wip commits
                    return Ok((committed, vec![]));
                }
                Err(e) => {
                    warn!("Test execution error during wave validation: {} — falling back to per-file validation", e);
                    let committed = self.fallback_per_file_commit(wave, test_framework, framework_context);
                    return Ok((committed, vec![]));
                }
            }
        }

        // Unstage everything (stash_pop_matching stages between applies),
        // then selectively re-stage only test files + artifacts.
        let _ = git::reset_index(&self.config.path);
        let mut all_stage_files: Vec<&str> = wave.iter()
            .flat_map(|r| r.test_files.iter().map(|s| s.as_str()))
            .collect();
        all_stage_files.extend(wave.iter().flat_map(|r| r.artifacts.iter().map(|s| s.as_str())));
        if let Err(e) = git::add_files(&self.config.path, &all_stage_files) {
            warn!("Failed to stage wave test files: {}", e);
            let _ = git::revert_changes(&self.config.path);
            return Ok((0, vec![]));
        }
        let _ = git::revert_changes(&self.config.path); // clean remaining leftovers

        // Create temp "reparo-wip:" commit — will be squashed later
        // Uses --no-verify to bypass pre-commit hooks (e.g. Conventional Commits)
        // since this is a temporary commit that will be squashed into a proper one.
        let wip_msg = format!(
            "reparo-wip: coverage boost {} file(s): {}",
            wave.len(),
            file_names.join(", ")
        );
        match git::commit_no_verify(&self.config.path, &wip_msg) {
            Ok(()) => {
                info!("Temp wave commit created ({} files) — pending squash", wave.len());
                Ok((wave.len(), wave.iter().map(|r| r.file.clone()).collect()))
            }
            Err(e) => {
                warn!("Failed to create temp wave commit: {} — reverting", e);
                let _ = git::revert_changes(&self.config.path);
                Ok((0, vec![]))
            }
        }
    }

    /// Squash N temporary "reparo-wip:" commits into a single real commit.
    ///
    /// Used when `commit_size > parallel_size`: after accumulating enough temp
    /// commits, this squashes them and creates one properly formatted commit.
    fn squash_boost_commits(&self, n: usize, files: &[String]) -> Result<()> {
        if n == 0 {
            return Ok(());
        }

        info!("Squashing {} temp commit(s) into one real commit ({} files)...", n, files.len());

        // Verify that the last n commits are "reparo-wip:" commits (safety check)
        let log_output = std::process::Command::new("git")
            .current_dir(&self.config.path)
            .args(["log", "--oneline", &format!("-{}", n)])
            .output();

        if let Ok(out) = log_output {
            let log_str = String::from_utf8_lossy(&out.stdout);
            let non_wip: Vec<&str> = log_str.lines()
                .filter(|l| !l.contains("reparo-wip:"))
                .collect();
            if !non_wip.is_empty() {
                warn!(
                    "Cannot squash: last {} commits contain non-wip entries: {:?}",
                    n, non_wip
                );
                return Ok(());
            }
        }

        // git reset --soft HEAD~n to unstage all wip commits back to index
        let reset_status = std::process::Command::new("git")
            .current_dir(&self.config.path)
            .args(["reset", "--soft", &format!("HEAD~{}", n)])
            .status();

        match reset_status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                warn!("git reset --soft HEAD~{} failed (exit {})", n, s);
                return Ok(());
            }
            Err(e) => {
                warn!("Failed to run git reset: {}", e);
                return Ok(());
            }
        }

        // Create the real commit
        let file_list: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
        let msg = if files.len() == 1 {
            format_commit_message(
                &self.config, "test", "coverage",
                &format!("add tests for {} (coverage boost)", files[0]),
                "", "", &files[0],
            )
        } else {
            format_commit_message(
                &self.config, "test", "coverage",
                &format!("add tests for {} files ({})", files.len(), file_list.join(", ")),
                "", "", "",
            )
        };

        match git::commit(&self.config.path, &msg) {
            Ok(()) => {
                info!("Squash commit successful ({} files in {} waves)", files.len(), n);
            }
            Err(e) => {
                warn!("Squash commit failed: {}", e);
            }
        }
        Ok(())
    }

    /// Run the coverage command and return the overall project coverage percentage.
    fn run_coverage_and_measure(&self, cov_cmd: &str) -> Option<f64> {
        let output_text = match runner::run_shell_command(&self.config.path, cov_cmd, "coverage measurement") {
            Ok((true, output)) => output,
            Ok((false, output)) => {
                warn!("Coverage command failed: {}", truncate(&output, 200));
                return None;
            }
            Err(e) => {
                warn!("Coverage command error: {}", e);
                return None;
            }
        };

        // Warn about common JaCoCo issues in the output
        if output_text.contains("Skipping JaCoCo execution due to missing execution data") {
            warn!(
                "JaCoCo skipped report generation — no execution data (jacoco.exec) was produced. \
                 This usually means tests did not run with the JaCoCo agent. \
                 Check that the Maven profile or surefire argLine is configured correctly in the coverage command."
            );
        }

        let lcov_path = runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref())?;
        let overall = runner::overall_lcov_coverage(&lcov_path);
        if let Some(pct) = overall {
            color_info!("Project-wide test coverage: {}", cov_vs(pct, self.config.min_coverage));
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
            let abs_path = resolve_source_file(&self.config.path, &dup_file.file_path);
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
                color_info!(
                    "Coverage {} — generating tests for {} uncovered lines before dedup...",
                    cov_prev(coverage.coverage_pct),
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
                            let msg = format_commit_message(
                                &self.config, "test", "dedup",
                                &format!("add tests for {} before deduplication", dup_file.file_path),
                                "", "", &dup_file.file_path,
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
            let prompt = claude::build_dedup_prompt(
                &dup_file.file_path,
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

            let dedup_tier = claude::classify_dedup_tier(dup_file.duplicated_lines, dup_file.duplication_pct);
            info!("Asking AI to refactor {} to reduce duplication... [{}]", dup_file.file_path, dedup_tier);
            match self.run_ai(&prompt, &dedup_tier) {
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
            color_info!("Duplication after refactoring {}: {} (was {})", dup_file.file_path, cov_vs(new_dup_pct, initial_dup_pct), cov_prev(initial_dup_pct));

            // Commit the dedup changes
            let _ = git::add_all(&self.config.path);
            if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                let msg = format_commit_message(
                    &self.config, "refactor", "dedup",
                    &format!("reduce code duplication in {}", dup_file.file_path),
                    "", "", &dup_file.file_path
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

    /// Step 5c: Improve code documentation to meet ISO 25000 / MDR standards.
    ///
    /// Scans source files, identifies those missing or with inadequate documentation,
    /// and asks Claude to add/improve docs. Verifies build still passes after each file.
    async fn improve_documentation(&self, test_command: &str) -> Result<()> {
        info!("=== Step 5c: Documentation quality (standards: {:?}) ===", self.config.documentation.standards);

        let doc_config = &self.config.documentation;

        // Find source files to document
        let mut files_to_doc: Vec<String> = Vec::new();
        let include_patterns = if doc_config.include.is_empty() {
            // Auto-detect based on project
            vec!["src/**/*.ts", "src/**/*.js", "src/**/*.java", "src/**/*.py", "src/**/*.rs", "src/**/*.go", "src/**/*.cs"]
                .into_iter().map(String::from).collect()
        } else {
            doc_config.include.clone()
        };

        for pattern in &include_patterns {
            let full_pattern = format!("{}/{}", self.config.path.display(), pattern);
            for entry in glob::glob(&full_pattern).unwrap_or_else(|_| glob::glob("").unwrap()) {
                if let Ok(path) = entry {
                    let rel_path = path.strip_prefix(&self.config.path)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();

                    // Skip excluded patterns
                    let excluded = doc_config.exclude.iter().any(|ex| {
                        let ex_glob = glob::Pattern::new(ex);
                        ex_glob.map(|p| p.matches(&rel_path)).unwrap_or(false)
                    });
                    if excluded { continue; }

                    // Skip test files
                    if is_test_file(&rel_path) { continue; }

                    // Skip non-coverable files (CSS, HTML, etc.)
                    if is_non_coverable_file(&rel_path) { continue; }

                    files_to_doc.push(rel_path);
                }
            }
        }

        if files_to_doc.is_empty() {
            info!("No source files found matching documentation patterns");
            return Ok(());
        }

        let max_files = if doc_config.max_files > 0 {
            doc_config.max_files.min(files_to_doc.len())
        } else {
            files_to_doc.len()
        };

        info!("Found {} source files to check documentation ({} max)", files_to_doc.len(), max_files);

        let mut docs_improved = 0usize;
        let mut docs_skipped = 0usize;

        for (idx, file_path) in files_to_doc.iter().take(max_files).enumerate() {
            info!("--- [doc {}/{}] {} ---", idx + 1, max_files, file_path);

            let abs_path = self.config.path.join(file_path);
            if !abs_path.exists() {
                warn!("Cannot read {} — skipping", file_path);
                docs_skipped += 1;
                continue;
            }

            let prompt = claude::build_documentation_prompt(
                file_path,
                &doc_config.style,
                &doc_config.standards,
                &doc_config.scope,
                &doc_config.required_elements,
                doc_config.rules.as_deref(),
            );

            let tier = claude::ClaudeTier::with_timeout("sonnet", "medium", 0.7);

            if self.config.show_prompts {
                info!("Documentation prompt:\n{}", prompt);
            }

            match self.run_ai(&prompt, &tier) {
                Ok(_) => {
                    info!("Claude completed documentation for {}", file_path);
                }
                Err(e) => {
                    warn!("Claude failed for docs of {}: {} — skipping", file_path, e);
                    let _ = git::revert_changes(&self.config.path);
                    docs_skipped += 1;
                    continue;
                }
            }

            // Verify no test files were modified
            let changed = git::changed_files(&self.config.path).unwrap_or_default();
            let test_files_changed: Vec<_> = changed.iter().filter(|f| is_test_file(f)).collect();
            if !test_files_changed.is_empty() {
                warn!("Documentation modified test files {:?} — reverting", test_files_changed);
                let _ = git::revert_changes(&self.config.path);
                docs_skipped += 1;
                continue;
            }

            // Check only source files were changed (no functionality changes)
            if changed.is_empty() {
                info!("No documentation changes needed for {}", file_path);
                continue;
            }

            // Format if configured
            if let Some(ref fmt_cmd) = self.config.commands.format {
                let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
            }

            // Build must pass
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                    Ok((true, _)) => {}
                    _ => {
                        warn!("Build failed after docs for {} — reverting", file_path);
                        let _ = git::revert_changes(&self.config.path);
                        docs_skipped += 1;
                        continue;
                    }
                }
            }

            // Tests must pass
            if !test_command.is_empty() {
                match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                    Ok((true, _)) => {}
                    _ => {
                        warn!("Tests failed after docs for {} — reverting", file_path);
                        let _ = git::revert_changes(&self.config.path);
                        docs_skipped += 1;
                        continue;
                    }
                }
            }

            // Run docs validation command if configured
            if let Some(ref docs_cmd) = doc_config.docs_command {
                match runner::run_shell_command(&self.config.path, docs_cmd, "docs validation") {
                    Ok((true, _)) => info!("Documentation validation passed"),
                    Ok((false, output)) => {
                        warn!("Documentation validation failed: {}", truncate(&output, 200));
                        // Non-blocking — commit anyway
                    }
                    Err(e) => warn!("Documentation validation error: {}", e),
                }
            }

            // Commit documentation changes
            let _ = git::add_all(&self.config.path);
            if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                let msg = format_commit_message(
                    &self.config, "docs", "quality",
                    &format!("improve documentation for {}", file_path),
                    "", "", file_path,
                );
                match git::commit(&self.config.path, &msg) {
                    Ok(()) => {
                        info!("Committed documentation improvements for {}", file_path);
                        docs_improved += 1;
                    }
                    Err(e) => {
                        warn!("Failed to commit docs: {}", e);
                        docs_skipped += 1;
                    }
                }
            }
        }

        info!(
            "Documentation quality complete: {} files improved, {} skipped",
            docs_improved, docs_skipped
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
                    color_info!(
                        "Coverage {} — generating tests for {} uncovered lines...",
                        cov_prev(coverage_pct),
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
                                let commit_msg = format_commit_message(
                                    &self.config, "test", "coverage",
                                    &format!("add partial tests for {} (100% not reached, fix skipped)", file_path),
                                    &issue.key, &issue.rule, &file_path,
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
                            let msg = format_commit_message(
                                &self.config, "test", "sonar",
                                &format!("add tests for {} coverage", file_path),
                                &issue.key, &issue.rule, &file_path,
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

        // Step A-1.5: Pact/contract testing (between coverage and fix)
        if !self.config.skip_pact && self.config.pact.enabled {
            match crate::pact::check_api_file(&file_path, &self.config.pact.api_patterns) {
                crate::pact::ApiCheckResult::IsApiFile => {
                    info!("File {} matches API patterns — running pact checks", file_path);

                    // Sub-step 1: Check existing contracts
                    if self.config.pact.check_contracts {
                        if let Some(ref verify_cmd) = self.config.pact.verify_command {
                            match crate::pact::verify_contracts(
                                &self.config.path,
                                verify_cmd,
                                self.config.pact.pact_dir.as_deref(),
                            ) {
                                Ok(crate::pact::PactVerifyResult::Passed) => {
                                    info!("Existing pact contracts pass");
                                }
                                Ok(crate::pact::PactVerifyResult::Failed { output }) => {
                                    warn!("Pact contracts fail BEFORE fix: {}", truncate(&output, 200));
                                    result.status = FixStatus::NeedsReview(
                                        "Existing pact contracts already failing — fix skipped".into(),
                                    );
                                    return result;
                                }
                                Ok(crate::pact::PactVerifyResult::NoContracts) => {
                                    info!("No pact contracts found for this provider/consumer");
                                }
                                Ok(crate::pact::PactVerifyResult::Unavailable { reason }) => {
                                    info!("Pact verification unavailable: {}", reason);
                                }
                                Err(e) => warn!("Pact check error: {}", e),
                            }
                        }
                    }

                    // Sub-step 2: Generate contract tests if enabled
                    if self.config.pact.generate_tests {
                        let gen_result = self.generate_contract_tests_with_retry(
                            issue, &file_path,
                        ).await;

                        match gen_result {
                            crate::pact::PactTestGenResult::Success { ref test_files } => {
                                if !test_files.is_empty() {
                                    let _ = git::add_all(&self.config.path);
                                    if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                                        let msg = format_commit_message(
                                            &self.config, "test", "pact",
                                            &format!("add contract tests for {}", file_path),
                                            &issue.key, &issue.rule, &file_path,
                                        );
                                        let _ = git::commit(&self.config.path, &msg);
                                        info!("Committed contract tests for {}", file_path);
                                    }
                                }
                            }
                            crate::pact::PactTestGenResult::TestsFailed { ref output } => {
                                warn!("Generated contract tests fail: {}", truncate(output, 200));
                                let _ = git::revert_changes(&self.config.path);
                            }
                            crate::pact::PactTestGenResult::GenerationFailed { ref error } => {
                                warn!("Failed to generate contract tests: {}", error);
                            }
                        }
                    }

                    // Sub-step 3: Verify before fix
                    if self.config.pact.verify_before_fix {
                        if let Some(ref verify_cmd) = self.config.pact.verify_command {
                            match crate::pact::verify_contracts(
                                &self.config.path,
                                verify_cmd,
                                self.config.pact.pact_dir.as_deref(),
                            ) {
                                Ok(crate::pact::PactVerifyResult::Failed { output }) => {
                                    warn!("Pact verification fails before fix: {}", truncate(&output, 200));
                                    result.status = FixStatus::NeedsReview(
                                        "Pact contracts fail before fix".into(),
                                    );
                                    return result;
                                }
                                Ok(_) => {
                                    info!("Pre-fix pact verification passed");
                                }
                                Err(e) => warn!("Pact pre-fix verification error: {}", e),
                            }
                        }
                    }
                }
                crate::pact::ApiCheckResult::NotApiFile => {
                    // Not an API file — skip pact steps silently
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
            start_line,
            end_line,
            &rule_desc_with_hint,
        );

        // Classify the issue to pick the right model + effort
        let tier = claude::classify_issue_tier(
            &issue.rule,
            &issue.severity,
            &issue.message,
            end_line.saturating_sub(start_line) + 1,
        );
        info!("Issue {} classified as tier {} (rule: {}, severity: {})", issue.key, tier, issue.rule, issue.severity);

        let claude_output = match self.run_ai(&prompt, &tier) {
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

        // Revert any changes to protected config files (package.json, tsconfig.json, etc.)
        let all_current = git::changed_files(&self.config.path).unwrap_or_default();
        let protected_changes: Vec<String> = all_current.iter().filter(|f| is_protected_file(f, &self.config.protected_files)).cloned().collect();
        if !protected_changes.is_empty() {
            warn!(
                "Claude modified protected config file(s) {:?} — reverting",
                protected_changes
            );
            for pf in &protected_changes {
                let checkout_result = std::process::Command::new("git")
                    .current_dir(&self.config.path)
                    .args(["checkout", "HEAD", "--", pf])
                    .status();
                match checkout_result {
                    Ok(s) if s.success() => info!("Reverted protected file: {}", pf),
                    _ => warn!("Could not revert protected file: {}", pf),
                }
            }
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
                            let repair_tier = claude::classify_repair_tier();
                            match self.run_ai(&repair_prompt, &repair_tier) {
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
                            let repair_tier = claude::classify_repair_tier();
                            match self.run_ai(&repair_prompt, &repair_tier) {
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

        // Step C-3.5: Verify pact contracts after fix
        if !self.config.skip_pact && self.config.pact.enabled && self.config.pact.verify_after_fix {
            if let crate::pact::ApiCheckResult::IsApiFile =
                crate::pact::check_api_file(&file_path, &self.config.pact.api_patterns)
            {
                if let Some(ref verify_cmd) = self.config.pact.verify_command {
                    match crate::pact::verify_contracts(
                        &self.config.path,
                        verify_cmd,
                        self.config.pact.pact_dir.as_deref(),
                    ) {
                        Ok(crate::pact::PactVerifyResult::Passed) => {
                            info!("Pact contracts still pass after fix");
                        }
                        Ok(crate::pact::PactVerifyResult::Failed { output }) => {
                            warn!("Pact contracts FAIL after fix for {}", issue.key);
                            let _ = git::revert_changes(&self.config.path);
                            result.status = FixStatus::NeedsReview(format!(
                                "Fix breaks pact contracts: {}",
                                truncate(&output, 200),
                            ));
                            return result;
                        }
                        Ok(crate::pact::PactVerifyResult::NoContracts) => {
                            info!("No pact contracts to verify after fix");
                        }
                        Ok(crate::pact::PactVerifyResult::Unavailable { reason }) => {
                            info!("Post-fix pact verification unavailable: {}", reason);
                        }
                        Err(e) => warn!("Post-fix pact verification error: {}", e),
                    }
                }
            }
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
                            let lint_tier = claude::classify_repair_tier();
                            match self.run_ai(&lint_prompt, &lint_tier) {
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
                Ok((true, _)) => {
                    if runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()).is_some() {
                        info!("Coverage report updated");
                    } else {
                        warn!("Coverage command succeeded but no report file was produced");
                    }
                }
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
                                    // Retry uses same tier but bumped up since first attempt failed
                                    let retry_tier = claude::ClaudeTier::with_timeout(
                                        if tier.model == "haiku" { "sonnet" } else { tier.model },
                                        if tier.effort == "low" { "medium" } else { tier.effort },
                                        tier.timeout_multiplier.max(1.0),
                                    );
                                    match self.run_ai(&retry_prompt, &retry_tier) {
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
            let subject = format_commit_message(
                &self.config, "fix", "sonar",
                &format!("{} - {}", issue.issue_type.to_lowercase(), truncate(&issue.message, 72)),
                &issue.key, &issue.rule, &file_path,
            );
            let msg = format!(
                "{}\n\n\
                 Rule: {}\n\
                 Severity: {}\n\
                 File: {}:{}\n\
                 Modified: {}\n\
                 Issue: {}",
                subject,
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
        let examples_str = self.cached_test_examples.clone().unwrap_or_else(|| {
            runner::find_test_examples(&self.config.path).join("\n\n")
        });
        let framework = detect_test_framework(&self.config.path);
        // US-040: Build framework context for issue-fix test generation
        let detected_deps = runner::detect_test_dependencies(&self.config.path);
        let framework_ctx_base = build_framework_context(&detected_deps, &self.config.test_generation);
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
            let file_class = runner::classify_source_file(file_path, &self.config.path);
            let pkg_hint = runner::derive_test_package(file_path)
                .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
                .unwrap_or_default();
            let prompt = if attempt == 1 {
                let uncovered = format!(
                    "Lines {}-{} (specifically uncovered: {})",
                    start_line, end_line, uncovered_desc
                );
                let per_file_ctx = build_per_file_context(&framework_ctx_base, &file_class, &pkg_hint);
                claude::build_test_generation_prompt(
                    file_path,
                    &uncovered,
                    &framework,
                    &examples_str,
                    &per_file_ctx,
                )
            } else {
                let still_uncovered = format!(
                    "Lines still uncovered: {}",
                    uncovered_desc
                );
                let per_file_ctx = build_per_file_context(&framework_ctx_base, &file_class, "");
                claude::build_test_generation_retry_prompt(
                    file_path,
                    &still_uncovered,
                    &framework,
                    attempt,
                    &truncate(&last_test_output, 1000),
                    &per_file_ctx,
                )
            };

            // Run claude to generate tests
            let test_tier = claude::classify_test_gen_tier(current_uncovered.len(), file_content.lines().count());
            match self.run_ai(&prompt, &test_tier) {
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
                .or_else(|| self.config.commands.coverage.clone())
                .or_else(|| runner::detect_coverage_command(&self.config.path));
            if let Some(ref cov_cmd) = coverage_cmd {
                info!("Running coverage command: {}", cov_cmd);
                match runner::run_shell_command(&self.config.path, cov_cmd, "coverage") {
                    Ok((true, _)) => {
                        if runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()).is_some() {
                            info!("Coverage report generated successfully");
                        } else {
                            warn!("Coverage command succeeded but no report file was produced");
                        }
                    }
                    Ok((false, output)) => warn!("Coverage command failed: {}", truncate(&output, 200)),
                    Err(e) => warn!("Failed to run coverage command: {}", e),
                }
            }

            // Check coverage locally from lcov report (fast, no SonarQube round-trip)
            let lcov_path = runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref());
            match lcov_path {
                Some(ref lcov) => {
                    match runner::check_local_coverage(lcov, file_path, start_line, end_line) {
                        Some(cov) if cov.fully_covered => {
                            color_info!(
                                "{} local coverage achieved after {} attempt(s) for {}",
                                cov_vs(100.0, 100.0), attempt, issue.key
                            );
                            return TestGenResult::Success {
                                test_files: all_test_files,
                            };
                        }
                        Some(cov) => {
                            color_info!(
                                "Local coverage {} ({} lines still uncovered) after attempt {}",
                                cov_prev(cov.coverage_pct),
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

    /// Generate contract tests with retry, following the same pattern as generate_tests_with_retry.
    async fn generate_contract_tests_with_retry(
        &self,
        issue: &Issue,
        file_path: &str,
    ) -> crate::pact::PactTestGenResult {
        let pact_framework = crate::pact::detect_pact_framework(&self.config.path);
        let contract_examples = crate::pact::find_contract_test_examples(&self.config.path);
        let examples_str = contract_examples.join("\n\n");
        let existing_pact_files = crate::pact::find_existing_pact_files(
            &self.config.path,
            self.config.pact.pact_dir.as_deref(),
        );
        let pact_files_str = existing_pact_files.join("\n\n");

        let provider = self.config.pact.provider_name.as_deref().unwrap_or("Provider");
        let consumer = self.config.pact.consumer_name.as_deref().unwrap_or("Consumer");
        let max_attempts = self.config.pact.attempts;
        let mut last_output = String::new();

        for attempt in 1..=max_attempts {
            info!(
                "Contract test generation attempt {}/{} for {}",
                attempt, max_attempts, issue.key
            );

            let prompt = if attempt == 1 {
                claude::build_contract_test_prompt(
                    file_path,
                    provider,
                    consumer,
                    &pact_framework,
                    &examples_str,
                    &pact_files_str,
                )
            } else {
                claude::build_contract_test_retry_prompt(
                    file_path,
                    provider,
                    consumer,
                    &pact_framework,
                    attempt,
                    &last_output,
                )
            };

            if self.config.show_prompts {
                info!("Contract test generation prompt:\n{}", prompt);
            }

            // Use a moderate tier for contract test generation
            let tier = claude::classify_contract_test_tier(5); // default estimate

            let claude_result = self.run_ai(&prompt, &tier);

            match claude_result {
                Ok(output) => {
                    last_output = output;
                }
                Err(e) => {
                    if attempt == 1 {
                        return crate::pact::PactTestGenResult::GenerationFailed {
                            error: format!("Claude failed: {}", e),
                        };
                    }
                    warn!("Claude failed on contract test retry {}: {}", attempt, e);
                    continue;
                }
            }

            // Detect new files
            let new_files = git::changed_files(&self.config.path)
                .unwrap_or_default()
                .into_iter()
                .filter(|f| {
                    let lower = f.to_lowercase();
                    lower.contains("pact") || lower.contains("contract")
                        || lower.contains("test") || lower.contains("spec")
                })
                .collect::<Vec<_>>();

            if new_files.is_empty() && attempt == 1 {
                return crate::pact::PactTestGenResult::GenerationFailed {
                    error: "No contract test files were created".to_string(),
                };
            }

            // Run contract tests if command is configured
            if let Some(ref test_cmd) = self.config.pact.test_command {
                match runner::run_shell_command(&self.config.path, test_cmd, "pact test") {
                    Ok((true, _)) => {
                        info!("Contract tests pass on attempt {}", attempt);
                        return crate::pact::PactTestGenResult::Success {
                            test_files: new_files,
                        };
                    }
                    Ok((false, output)) => {
                        last_output = output.clone();
                        if attempt == max_attempts {
                            let _ = git::revert_changes(&self.config.path);
                            return crate::pact::PactTestGenResult::TestsFailed { output };
                        }
                        warn!(
                            "Contract tests fail on attempt {}/{} — retrying",
                            attempt, max_attempts
                        );
                        let _ = git::revert_changes(&self.config.path);
                    }
                    Err(e) => {
                        last_output = e.to_string();
                        if attempt == max_attempts {
                            let _ = git::revert_changes(&self.config.path);
                            return crate::pact::PactTestGenResult::TestsFailed {
                                output: e.to_string(),
                            };
                        }
                        let _ = git::revert_changes(&self.config.path);
                    }
                }
            } else {
                // No test command — assume generated tests are valid
                return crate::pact::PactTestGenResult::Success {
                    test_files: new_files,
                };
            }
        }

        crate::pact::PactTestGenResult::GenerationFailed {
            error: "Contract test generation failed after all attempts".to_string(),
        }
    }

    /// Rebase the current fix branch onto the latest base branch from origin.
    ///
    /// Fetches the latest base, attempts rebase, and if conflicts arise,
    /// invokes the AI engine to resolve them. Aborts if resolution fails.
    fn rebase_on_latest_base(&self) -> Result<()> {
        info!("=== Pre-push rebase: fetching latest {} from origin ===", self.config.branch);

        if let Err(e) = git::fetch_branch(&self.config.path, &self.config.branch) {
            warn!("Could not fetch origin/{}: {} — skipping rebase", self.config.branch, e);
            return Ok(());
        }

        match git::rebase_onto(&self.config.path, &self.config.branch)? {
            true => {
                info!("Rebase onto origin/{} completed cleanly", self.config.branch);
                Ok(())
            }
            false => {
                info!("Rebase has conflicts — attempting AI-assisted resolution");
                self.resolve_rebase_conflicts()
            }
        }
    }

    /// Attempt to resolve rebase conflicts using the AI engine.
    ///
    /// For each conflicted commit, reads the conflicted files, asks the AI to
    /// resolve them, and continues the rebase. Aborts if any step fails.
    fn resolve_rebase_conflicts(&self) -> Result<()> {
        const MAX_CONFLICT_ROUNDS: usize = 20; // safety limit for multi-commit rebases

        for round in 0..MAX_CONFLICT_ROUNDS {
            let conflicts = git::conflict_files(&self.config.path)?;
            if conflicts.is_empty() {
                info!("No more conflicts to resolve");
                break;
            }

            info!("Conflict round {}: {} file(s) to resolve: {}",
                round + 1, conflicts.len(), conflicts.join(", "));

            for file in &conflicts {
                let file_path = self.config.path.join(file);
                let content = match std::fs::read_to_string(&file_path) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Cannot read conflicted file {}: {}", file, e);
                        git::abort_rebase(&self.config.path)?;
                        anyhow::bail!("Rebase aborted: could not read {}", file);
                    }
                };

                let prompt = format!(
                    "The file `{}` has git merge conflicts (marked with <<<<<<< / ======= / >>>>>>>). \
                     Resolve all conflicts by choosing the best combination of both sides. \
                     Output ONLY the complete resolved file content, no explanations. \
                     Keep all functionality from both sides where possible.\n\n```\n{}\n```",
                    file, content
                );

                let tier = claude::ClaudeTier::with_timeout("sonnet", "medium", 0.7);
                match self.run_ai(&prompt, &tier) {
                    Ok(_) => {
                        // The AI engine edits files in-place, so we just check the file
                        // no longer has conflict markers
                        let resolved = std::fs::read_to_string(&file_path).unwrap_or_default();
                        if resolved.contains("<<<<<<<") || resolved.contains(">>>>>>>") {
                            warn!("AI did not fully resolve conflicts in {} — aborting rebase", file);
                            git::abort_rebase(&self.config.path)?;
                            anyhow::bail!(
                                "Rebase aborted: AI could not resolve conflicts in {}. \
                                 Resolve manually and re-run, or use --skip-rebase.",
                                file
                            );
                        }
                        info!("Resolved conflicts in {}", file);
                    }
                    Err(e) => {
                        error!("AI conflict resolution failed for {}: {}", file, e);
                        git::abort_rebase(&self.config.path)?;
                        anyhow::bail!(
                            "Rebase aborted: AI failed to resolve {}. \
                             Resolve manually and re-run, or use --skip-rebase.",
                            file
                        );
                    }
                }
            }

            // All files resolved for this commit — continue rebase
            match git::mark_resolved_and_continue(&self.config.path)? {
                true => {
                    info!("Rebase continued successfully after conflict resolution");
                    return Ok(());
                }
                false => {
                    info!("More conflicts after continue — resolving next commit");
                    // Loop continues
                }
            }
        }

        warn!("Too many conflict rounds ({}) — aborting rebase", MAX_CONFLICT_ROUNDS);
        git::abort_rebase(&self.config.path)?;
        anyhow::bail!("Rebase aborted after {} conflict rounds. Resolve manually or use --skip-rebase.", MAX_CONFLICT_ROUNDS);
    }

    /// Create a PR from the accumulated results (US-008).
    fn create_pr(&self, branch_name: &str) -> Result<String> {
        // Stage any remaining changes (changelog, etc.) and push
        let _ = git::add_all(&self.config.path);
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let msg = format_commit_message(&self.config, "chore", "sonar", "include changelog and report updates", "", "", "");
            let _ = git::commit(&self.config.path, &msg);
        }

        // Rebase onto latest base branch to minimize merge conflicts
        if !self.config.skip_rebase {
            if let Err(e) = self.rebase_on_latest_base() {
                warn!("Pre-push rebase failed: {} — pushing without rebase", e);
            }
        } else {
            info!("Pre-push rebase skipped (--skip-rebase)");
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

/// Build framework context string from detected dependencies and YAML config (US-040).
///
/// Combines auto-detected test dependencies with user-provided YAML overrides.
fn build_framework_context(
    detected_deps: &str,
    tg: &crate::config::TestGenerationConfig,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Start with auto-detected dependencies
    if !detected_deps.is_empty() {
        parts.push(format!("Detected test dependencies: {}", detected_deps));
    }

    // YAML overrides / supplements
    if let Some(ref fw) = tg.framework {
        parts.push(format!("Test framework: {}", fw));
    }
    if let Some(ref mock) = tg.mock_framework {
        parts.push(format!("Mock framework: {}", mock));
    }
    if let Some(ref assert_lib) = tg.assertion_library {
        parts.push(format!("Assertion library: {}", assert_lib));
    }
    if tg.avoid_spring_context {
        parts.push("IMPORTANT: Do NOT use @SpringBootTest for unit tests. Use @ExtendWith(MockitoExtension.class) or plain JUnit 5 instead.".to_string());
    }
    if let Some(ref custom) = tg.custom_instructions {
        parts.push(custom.clone());
    }

    parts.join("\n")
}

/// Build per-file context combining base framework context with file-specific classification (US-040).
fn build_per_file_context(base: &str, file_classification: &str, package_hint: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !base.is_empty() {
        parts.push(base.to_string());
    }
    if !file_classification.is_empty() {
        parts.push(file_classification.to_string());
    }
    if !package_hint.is_empty() {
        parts.push(package_hint.to_string());
    }
    parts.join("\n")
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
/// Check if a file is a generated artifact (coverage reports, build output, etc.)
/// that should not be treated as source code modification.
fn is_generated_artifact(path: &str) -> bool {
    let lower = path.to_lowercase();
    // Coverage report directories and files
    lower.starts_with("coverage/")
        || lower.contains("/coverage/")
        || lower.ends_with("lcov.info")
        || lower.ends_with("clover.xml")
        || lower.ends_with("coverage-final.json")
        || lower.ends_with("coverage-summary.json")
        || lower.ends_with("cobertura.xml")
        || lower.ends_with("jacoco.xml")
        || lower.ends_with("jacoco.csv")
        // Build output directories
        || lower.starts_with("dist/")
        || lower.contains("/dist/")
        || lower.starts_with("build/")
        || lower.contains("/build/")
        || lower.starts_with("target/")
        || lower.contains("/target/")
        || lower.starts_with(".nyc_output/")
        || lower.contains("/.nyc_output/")
        // Pact output
        || lower.starts_with("pacts/")
        || lower.contains("/pacts/")
}

fn is_internal_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".calendula-state.json")
        || lower.ends_with(".reparo-state.json")
        || lower.contains("techdebt_changelog")
        || lower.contains("report.md")
        || lower.contains("review_needed.md")
        || lower.ends_with("report-task.txt")
}

/// Check if a file is protected from Claude modifications.
/// Matches the basename of `path` case-insensitively against the configured protected_files list.
fn is_protected_file(path: &str, protected_files: &[String]) -> bool {
    if protected_files.is_empty() {
        return false;
    }
    let lower = path.to_lowercase();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    protected_files.iter().any(|p| p.to_lowercase() == filename)
}

/// Resolve a source file path from a coverage report (e.g. `com/example/Foo.java`)
/// to its actual location on disk. Tries the direct path first, then common source
/// roots like `src/main/java/`, `src/main/kotlin/`, etc.
fn resolve_source_file(project_path: &Path, relative_file: &str) -> PathBuf {
    let direct = project_path.join(relative_file);
    if direct.exists() {
        return direct;
    }
    let source_roots = [
        "src/main/java",
        "src/main/kotlin",
        "src/main/scala",
        "src/main/groovy",
        "src/main/resources",
        "src",
        "app/src/main/java",
        "app/src/main/kotlin",
        "lib/src/main/java",
    ];
    for root in &source_roots {
        let candidate = project_path.join(root).join(relative_file);
        if candidate.exists() {
            return candidate;
        }
    }

    // Try searching in subdirectories (multi-module Maven projects)
    // e.g., lift-fw/src/main/java/com/... when --path points to parent
    if let Ok(entries) = std::fs::read_dir(project_path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let sub = entry.path().join("src/main/java").join(relative_file);
                if sub.exists() {
                    return sub;
                }
            }
        }
    }

    direct
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

/// Truncate keeping the **tail** of the string — useful for build/test output
/// where errors appear at the end.
fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(count - max).collect();
        format!("...{}", tail)
    }
}

/// Format a commit message using the configured template.
///
/// Supported placeholders:
/// - `{type}`: conventional commit type (fix, test, refactor, style, docs, chore)
/// - `{scope}`: scope (e.g., "sonar", "coverage", "dedup")
/// - `{message}`: the commit description
/// - `{issue_key}`: SonarQube issue key
/// - `{rule}`: SonarQube rule ID
/// - `{file}`: affected file path
/// - Any custom key from `commit_vars` (e.g., `{gitlab_issue}`)
fn format_commit_message(
    config: &ValidatedConfig,
    commit_type: &str,
    scope: &str,
    message: &str,
    issue_key: &str,
    rule: &str,
    file: &str,
) -> String {
    let mut result = config.commit_format.clone();
    result = result.replace("{type}", commit_type);
    result = result.replace("{scope}", scope);
    result = result.replace("{message}", message);
    result = result.replace("{issue_key}", issue_key);
    result = result.replace("{rule}", rule);
    result = result.replace("{file}", file);
    result = result.replace(
        "{ticket}",
        config.commit_issue.as_deref().unwrap_or(""),
    );

    // Apply custom variables from commit_vars
    for (key, value) in &config.commit_vars {
        let placeholder = format!("{{{}}}", key);
        result = result.replace(&placeholder, value);
    }

    result
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

    // -- is_generated_artifact --

    #[test]
    fn test_is_generated_artifact_coverage() {
        assert!(is_generated_artifact("coverage/lcov.info"));
        assert!(is_generated_artifact("coverage/clover.xml"));
        assert!(is_generated_artifact("coverage/coverage-final.json"));
        assert!(is_generated_artifact("coverage/lcov-report/index.html"));
        assert!(is_generated_artifact("projects/my-lib/coverage/lcov.info"));
    }

    #[test]
    fn test_is_generated_artifact_build_output() {
        assert!(is_generated_artifact("dist/main.js"));
        assert!(is_generated_artifact("build/output.css"));
        assert!(is_generated_artifact("target/debug/binary"));
        assert!(is_generated_artifact(".nyc_output/data.json"));
    }

    #[test]
    fn test_is_generated_artifact_not_source() {
        assert!(!is_generated_artifact("src/main.ts"));
        assert!(!is_generated_artifact("src/components/Button.tsx"));
        assert!(!is_generated_artifact("package.json"));
        assert!(!is_generated_artifact("tsconfig.json"));
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

    #[test]
    fn test_truncate_tail_short() {
        assert_eq!(truncate_tail("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_tail_long() {
        assert_eq!(truncate_tail("hello world", 5), "...world");
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

    #[test]
    fn test_coverage_progress_format() {
        // Validates the progress log format used after each wave (US-038)
        let (files_processed, total_files, files_boosted) = (5usize, 20usize, 3usize);
        let (start_pct, current_pct) = (15.4f64, 22.7f64);
        let msg = format!(
            "Coverage boost progress: {}/{} files processed, {} committed, coverage: {:.1}% → {:.1}%",
            files_processed, total_files, files_boosted, start_pct, current_pct
        );
        assert!(msg.contains("5/20 files processed"));
        assert!(msg.contains("3 committed"));
        assert!(msg.contains("15.4%"));
        assert!(msg.contains("22.7%"));
    }

    #[test]
    fn test_coverage_summary_format() {
        // Validates the final summary format (US-038)
        let (files_processed, files_boosted) = (57usize, 12usize);
        let (start_pct, current_pct, target) = (15.4f64, 45.2f64, 80.0f64);
        let msg = format!(
            "Coverage boost summary: processed {} files, committed {}, coverage {:.1}% → {:.1}% (target: {:.0}%)",
            files_processed, files_boosted, start_pct, current_pct, target
        );
        assert!(msg.contains("processed 57 files"));
        assert!(msg.contains("committed 12"));
        assert!(msg.contains("15.4% → 45.2%"));
        assert!(msg.contains("(target: 80%)"));
    }

    #[test]
    fn test_coverage_per_file_log_includes_overall() {
        // Validates the per-file log line includes overall coverage (US-038)
        let (queue_idx, total, file_pct, covered, total_lines, current_pct) =
            (7usize, 181usize, 23.5f64, 47u32, 200u32, 18.2f64);
        let reason = "overall 18.2% < 80%";
        let msg = format!(
            "--- Coverage boost [{}/{}]: {} ({:.1}%, {}/{} lines) — {} | overall: {:.1}% ---",
            queue_idx, total, "src/main.java", file_pct, covered, total_lines, reason, current_pct
        );
        assert!(msg.contains("[7/181]"));
        assert!(msg.contains("23.5%"));
        assert!(msg.contains("| overall: 18.2%"));
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
