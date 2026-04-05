mod coverage;
mod dedup;
mod fix_loop;
pub(crate) mod helpers;

use anyhow::Result;
use std::time::Instant;
use tracing::{error, info, warn};

use crate::claude;
use crate::config::ValidatedConfig;
use crate::git;
use crate::report::{self, FixStatus, IssueResult};
use crate::runner;
use crate::sonar::{self, Issue, SonarClient};

use helpers::*;

pub struct Orchestrator {
    pub(crate) config: ValidatedConfig,
    pub(crate) client: SonarClient,
    pub(crate) results: Vec<IssueResult>,
    /// Total issues found in SonarQube (before --max-issues filter)
    pub(crate) total_issues_found: usize,
    /// Prompt configuration from YAML (US-019)
    pub(crate) prompt_config: crate::yaml_config::PromptsYaml,
    /// Execution state for resume support (US-017)
    pub(crate) exec_state: Option<crate::state::ExecutionState>,
    /// Rule description cache (US-020): rule_key → description
    pub(crate) rule_cache: std::collections::HashMap<String, String>,
    /// Engine routing configuration for multi-engine AI dispatch
    pub(crate) engine_routing: crate::engine::EngineRoutingConfig,
    /// Cached test examples (computed once, reused across issues)
    pub(crate) cached_test_examples: Option<String>,
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
    pub(crate) fn run_ai(&self, prompt: &str, tier: &claude::ClaudeTier) -> anyhow::Result<String> {
        let invocation = crate::engine::resolve_engine_for_tier(tier, &self.engine_routing)?;
        let tier_timeout = tier.effective_timeout(self.config.claude_timeout);

        // Prompt-aware timeout floor: larger prompts indicate more complex tasks
        // requiring more reasoning and output generation time.  Use 120s base +
        // ~100ms per prompt character, capped at 3× the configured base timeout.
        let prompt_floor = ((prompt.len() as u64) / 10 + 120)
            .min(self.config.claude_timeout.saturating_mul(3));
        let timeout = tier_timeout.max(prompt_floor);

        if timeout > tier_timeout {
            tracing::info!(
                "Timeout boosted by prompt size: {}s → {}s (prompt {} chars)",
                tier_timeout, timeout, prompt.len()
            );
        }

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
        if self.config.skip_fixes {
            info!("Skipping fix loop (--skip-fixes)");
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }
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

            // Circuit breaker: stop if too many consecutive build failures
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
