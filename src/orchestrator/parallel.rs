//! Parallel issue processing using git worktrees (US-018).
//!
//! Two parallel modes:
//!
//! **Per-issue parallel** (`--parallel N --batch-size 1`):
//!   Each issue gets its own worktree, branch, and PR. Push/PR creation is
//!   serialized via a semaphore to avoid rate limiting.
//!
//! **Wave-based parallel** (`--parallel N`, any batch size):
//!   Issues are grouped into waves — each wave contains only issues that touch
//!   different files, so they can be fixed concurrently without conflicts.
//!   After each wave the successful worktree commits are cherry-picked onto
//!   the shared batch branch. A single PR is created at the end of the batch.

use super::helpers::*;
use super::worktree_pool::WorktreePool;
use super::Orchestrator;
use crate::git;
use crate::report::{self, FixStatus, IssueResult};
use crate::sonar::{self, Issue};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn, Instrument};

impl Orchestrator {
    /// Run the fix loop in parallel using git worktrees (US-018).
    ///
    /// Each issue is processed in its own worktree with its own branch.
    /// Results and usage are collected and merged back into `self`.
    pub(super) async fn run_parallel_fix_loop(
        &mut self,
        initial_issues: &[Issue],
        original_issue_keys: &HashSet<String>,
        already_processed: &HashSet<String>,
        max_issues: usize,
        test_command: &str,
    ) -> Result<()> {
        let parallelism = self.config.parallel as usize;
        info!(
            "=== Parallel fix loop: {} workers, {} max issues ===",
            parallelism, max_issues
        );

        // 1. Collect all issues to process (filtered + limited)
        let issues_to_process: Vec<Issue> = initial_issues
            .iter()
            .filter(|i| {
                original_issue_keys.contains(&i.key)
                    && !already_processed.contains(&i.key)
                    && !self.results.iter().any(|r| r.issue_key == i.key)
            })
            .take(max_issues)
            .cloned()
            .collect();

        // 1b. Pre-filter blocklisted (rule, file) pairs so we don't allocate a
        // worktree + branch just to bail out on the first instruction inside
        // process_issue. Pre-skipped issues are reported as NeedsReview now.
        let (issues_to_process, blocklisted) = partition_blocklisted(
            issues_to_process,
            &self.config.path,
            &self.config.rule_blocklist,
            &self.config.hard_case_blocklist,
        );
        if !blocklisted.is_empty() {
            info!(
                "Pre-filter: skipping {} blocklisted issue(s) before dispatch",
                blocklisted.len()
            );
            for r in blocklisted {
                crate::report::append_changelog(&self.config.path, &r);
                self.results.push(r);
            }
        }

        if issues_to_process.is_empty() {
            info!("No issues to process in parallel mode");
            return Ok(());
        }

        info!(
            "Dispatching {} issues across {} worktrees",
            issues_to_process.len(),
            parallelism
        );

        // 2. Pre-fetch all rule descriptions
        for issue in &issues_to_process {
            if !self.rule_cache.contains_key(&issue.rule) {
                if let Ok(desc) = self.client.get_rule_description(&issue.rule).await {
                    self.rule_cache.insert(issue.rule.clone(), desc);
                }
            }
        }

        // 3. Create worktree pool
        let pool = Arc::new(
            WorktreePool::new(&self.config.path, parallelism)
                .context("Failed to create worktree pool for parallel fix loop")?,
        );

        // 4. Serialize push + PR creation
        let push_semaphore = Arc::new(Semaphore::new(1));

        // 5. Build shared state for workers
        let shared_config = self.config.clone();
        let shared_client = self.client.clone();
        let shared_rule_cache = self.rule_cache.clone();
        let shared_engine_routing = self.engine_routing.clone();
        let shared_prompt_config = self.prompt_config.clone();
        let shared_test_examples = self.cached_test_examples.clone();
        let shared_exec_log = self.exec_log.clone();
        // Cross-worker (file, rule) failure memory — see Orchestrator field doc.
        let shared_failure_memory = Arc::clone(&self.fix_failure_memory);
        // Cross-worker Claude session map (US-081).
        let shared_session_map = Arc::clone(&self.session_map);

        // 6. Dispatch workers
        let concurrency = Arc::new(Semaphore::new(parallelism));
        let mut handles = Vec::with_capacity(issues_to_process.len());

        for (idx, issue) in issues_to_process.into_iter().enumerate() {
            let pool: Arc<WorktreePool> = Arc::clone(&pool);
            let push_sem = Arc::clone(&push_semaphore);
            let conc_sem = Arc::clone(&concurrency);
            let config = shared_config.clone();
            let client = shared_client.clone();
            let rule_cache = shared_rule_cache.clone();
            let engine_routing = shared_engine_routing.clone();
            let prompt_config = shared_prompt_config.clone();
            let test_examples = shared_test_examples.clone();
            let exec_log = shared_exec_log.clone();
            let failure_memory = Arc::clone(&shared_failure_memory);
            let session_map = Arc::clone(&shared_session_map);
            let test_cmd = test_command.to_string();
            let worker_id = idx % parallelism;

            // Attach the span via `.instrument()` so it tracks correctly across
            // `.await` points. `span.enter()` would bind the span to the current
            // OS thread instead of the task — when the task yields and resumes on
            // a different worker thread, the span leaks to whatever other task
            // is running there, producing the bogus nested `worker:worker` log
            // prefixes seen in wave-parallel mode.
            let span = tracing::info_span!(
                "worker",
                id = worker_id,
                issue = %issue.key,
            );
            let handle = tokio::spawn(async move {
                // Limit concurrency to pool size
                let _permit = conc_sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("Semaphore closed: {}", e))?;

                // Acquire a worktree
                let wt_root = pool.acquire_async().await;
                let wt_path = pool.project_dir(&wt_root);
                info!(
                    "[worker-{}/{}] Processing {} ({} {}) in worktree {}",
                    worker_id,
                    issue.key,
                    issue.key,
                    issue.severity,
                    issue.issue_type,
                    wt_path.display()
                );

                // Create a branch in the worktree
                let branch_name = format!(
                    "fix/sonar-{}",
                    issue.key.to_lowercase().replace(':', "-")
                );
                if let Err(e) =
                    git::create_branch_in_worktree(&wt_root, &branch_name, &config.branch)
                {
                    error!(
                        "[worker-{}/{}] Failed to create branch: {}",
                        worker_id, issue.key, e
                    );
                    let _ = git::clean_worktree(&wt_root);
                    pool.release(wt_root);
                    return Ok::<IssueResult, anyhow::Error>(IssueResult {
                        issue_key: issue.key.clone(),
                        rule: issue.rule.clone(),
                        severity: issue.severity.clone(),
                        issue_type: issue.issue_type.clone(),
                        message: issue.message.clone(),
                        file: crate::sonar::component_to_path(&issue.component),
                        lines: format_lines(&issue.text_range),
                        status: FixStatus::Failed(format!("Branch creation failed: {}", e)),
                        change_description: String::new(),
                        tests_added: Vec::new(),
                        pr_url: None,
                        diff_summary: None,
                    });
                }

                // Build a worker Orchestrator with the worktree path
                let mut worker_config = config.clone();
                worker_config.path = wt_path.clone();
                // In parallel mode, skip SonarQube re-scan (too slow, requires main tree).
                // Clearing `scanner` is what actually disables the per-issue rescan loop
                // in fix_loop (`if let Some(ref scanner) = self.config.scanner`).
                // Flipping `skip_scan` alone is a no-op post-validation.
                worker_config.skip_scan = true;
                worker_config.scanner = None;

                let worker = Orchestrator::new_worker(
                    worker_config,
                    client,
                    rule_cache,
                    engine_routing,
                    prompt_config,
                    test_examples,
                    exec_log,
                    failure_memory,
                    session_map,
                );

                // Process the issue in the worktree
                let mut result = worker.process_issue(&issue, &test_cmd).await;

                // If fixed: push + create PR (serialized)
                if matches!(result.status, FixStatus::Fixed) {
                    let _push_permit = push_sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("Push semaphore closed: {}", e))?;

                    info!(
                        "[worker-{}/{}] Pushing and creating PR for {}",
                        worker_id, issue.key, branch_name
                    );

                    match worker.create_per_issue_pr(&result, &branch_name) {
                        Ok(pr_url) => {
                            info!(
                                "[worker-{}/{}] PR created: {}",
                                worker_id, issue.key, pr_url
                            );
                            result.pr_url = Some(pr_url);
                        }
                        Err(e) => {
                            error!(
                                "[worker-{}/{}] Failed to create PR: {}",
                                worker_id, issue.key, e
                            );
                        }
                    }
                }

                // Worker AI calls already write directly to the shared execution log
                // via `run_ai` (no per-worker tracker needed).

                // Clean worktree for reuse (operate on worktree root, not subdir)
                let _ = git::clean_worktree(&wt_root);
                pool.release(wt_root);

                Ok(result)
            }.instrument(span));

            handles.push(handle);
        }

        // 7. Collect results
        let mut total_fixed = 0usize;
        let mut total_failed = 0usize;

        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => {
                    match &result.status {
                        FixStatus::Fixed => {
                            total_fixed += 1;
                            info!("Issue {} fixed successfully", result.issue_key);
                        }
                        FixStatus::NeedsReview(reason) => {
                            total_failed += 1;
                            warn!("Issue {} needs review: {}", result.issue_key, reason);
                        }
                        FixStatus::Failed(err) => {
                            total_failed += 1;
                            error!("Issue {} failed: {}", result.issue_key, err);
                        }
                        FixStatus::Skipped(reason) => {
                            info!("Issue {} skipped: {}", result.issue_key, reason);
                        }
                        FixStatus::RiskSkipped(reason) => {
                            info!("Issue {} risk-skipped: {}", result.issue_key, reason);
                        }
                    }
                    // Append changelog
                    report::append_changelog(&self.config.path, &result);
                    self.results.push(result);
                }
                Ok(Err(e)) => {
                    error!("Worker task error: {}", e);
                    total_failed += 1;
                }
                Err(e) => {
                    error!("Worker task panicked: {}", e);
                    total_failed += 1;
                }
            }
        }

        info!(
            "Parallel processing complete: {} fixed, {} failed/review",
            total_fixed, total_failed
        );

        // Pool is dropped here, cleaning up all worktrees
        Ok(())
    }

    /// Run the fix loop using parallel waves on a shared batch branch.
    ///
    /// Issues are grouped into waves: each wave contains only issues that touch
    /// **different files**, so they can run concurrently in separate worktrees
    /// without creating git conflicts.
    ///
    /// After every wave the successful worktree commits are cherry-picked (in
    /// deterministic order) onto the current HEAD of the calling branch.
    /// The caller is responsible for creating the branch before calling this
    /// and for creating the PR afterwards.
    pub(super) async fn run_wave_parallel_fixes(
        &mut self,
        initial_issues: &[Issue],
        original_issue_keys: &HashSet<String>,
        already_processed: &HashSet<String>,
        max_issues: usize,
        test_command: &str,
        base_branch: &str,
    ) -> Result<()> {
        let parallelism = self.config.parallel as usize;

        // 1. Collect candidate issues (same filter as the sequential loop)
        let candidates: Vec<Issue> = initial_issues
            .iter()
            .filter(|i| {
                original_issue_keys.contains(&i.key)
                    && !already_processed.contains(&i.key)
                    && !self.results.iter().any(|r| r.issue_key == i.key)
            })
            .take(max_issues)
            .cloned()
            .collect();

        // 1b. Pre-filter blocklisted (rule, file) pairs so the wave builder
        // sees only runnable candidates. Without this, ~20-30% of wave slots
        // on large projects were consumed just to run the blocklist check
        // inside process_issue, allocating a worktree + branch each time.
        let (candidates, blocklisted) = partition_blocklisted(
            candidates,
            &self.config.path,
            &self.config.rule_blocklist,
            &self.config.hard_case_blocklist,
        );
        if !blocklisted.is_empty() {
            info!(
                "Pre-filter: skipping {} blocklisted issue(s) before wave dispatch",
                blocklisted.len()
            );
            for r in blocklisted {
                crate::report::append_changelog(&self.config.path, &r);
                self.results.push(r);
            }
        }

        if candidates.is_empty() {
            info!("No issues to process in wave-parallel mode");
            return Ok(());
        }

        // 2. Pre-fetch rule descriptions for all issues
        for issue in &candidates {
            if !self.rule_cache.contains_key(&issue.rule) {
                if let Ok(desc) = self.client.get_rule_description(&issue.rule).await {
                    self.rule_cache.insert(issue.rule.clone(), desc);
                }
            }
        }

        // 3. Group into waves. Each wave enforces three constraints:
        //   (a) affinity bucket (file/package) — no collisions on cherry-pick.
        //   (b) same-(file,rule) — avoids overlapping diff races.
        //   (c) hot-file — at most one fix on any file with ≥3 pending issues.
        //       Hot files (EntityServiceImpl, DomainObjectDAOImpl, …) were
        //       individually passing targeted tests but collectively breaking
        //       the post-wave full suite, triggering expensive bisects.
        let hot_files = detect_hot_files(&candidates);
        if !hot_files.is_empty() {
            let mut sorted: Vec<&String> = hot_files.iter().collect();
            sorted.sort();
            info!(
                "Hot files pre-detected ({}): {:?} — fixes will serialize across waves",
                hot_files.len(),
                sorted
            );
        }
        // Deterministic-fast-path rules (S1118, S1124) are front-loaded so
        // early waves finish without any Claude call.
        let waves = group_issues_into_waves_with_hot(
            &candidates,
            self.config.wave_grouping_depth,
            &hot_files,
        );
        info!(
            "Wave-parallel: {} issues → {} wave(s), {} worker(s), grouping_depth={}",
            candidates.len(),
            waves.len(),
            parallelism,
            self.config.wave_grouping_depth
        );

        // 4. Create worktree pool once for all waves
        let pool = Arc::new(
            WorktreePool::new(&self.config.path, parallelism)
                .context("Failed to create worktree pool for wave-parallel fixes")?,
        );

        // Issues that a wave reported as Fixed but that SonarQube still flags
        // after the post-wave scan. Processed sequentially after the last wave
        // so each gets its own per-issue rescan + iterative refinement.
        let mut pending_retries: Vec<Issue> = Vec::new();

        // When `wave_scan_batch > 1` we defer the per-wave Sonar scan and
        // accumulate the issues from unreviewed waves here. The scan runs
        // once every N waves (or on the last wave), covering all accumulated
        // issues in a single scanner + CE cycle.
        let wave_scan_batch = self.config.wave_scan_batch.max(1) as usize;
        let mut pending_scan_issues: Vec<Issue> = Vec::new();
        // `initial_total_waves` is only for logging context; the actual queue
        // below can grow when adaptive sizing splits a wave in two.
        let initial_total_waves = waves.len();
        // Convert to a mutable queue so we can prepend wave remainders when
        // the adaptive cap kicks in after a bad bisect.
        let mut pending_waves: std::collections::VecDeque<Vec<Issue>> = waves.into();
        // Dynamic cap: shrinks after a wave where bisect dropped ≥50% of
        // commits (post-wave full-suite rejected them), recovers after a
        // clean wave. Starts at full parallelism. When it shrinks, we split
        // the next wave and defer the overflow to a later wave.
        let mut dynamic_wave_cap = parallelism.max(1);
        let mut wave_idx_counter: usize = 0;

        // Wave base advances after each successful wave so subsequent waves
        // branch from HEAD-with-previous-fixes-applied instead of the original
        // base. This eliminates cherry-pick conflicts for same-file fixes
        // landing in different waves (e.g. JwtTokens.java touched by two
        // sibling-package fixes in consecutive waves — without this, wave 2
        // branches from a pre-wave-1 state and its cherry-pick onto the
        // already-advanced HEAD collides).
        let mut current_base: String = base_branch.to_string();

        // 5. Process waves sequentially; issues within each wave run in parallel.
        while let Some(mut wave) = pending_waves.pop_front() {
            // Adaptive sizing: if the dynamic cap is below this wave's size,
            // split the tail off and push it back to the front of the queue.
            // This makes "halve wave size after a bad bisect" transparent to
            // the rest of the loop.
            if wave.len() > dynamic_wave_cap {
                let overflow = wave.split_off(dynamic_wave_cap);
                info!(
                    "Adaptive sizing: capping next wave to {} issue(s); deferring {} to a later wave",
                    dynamic_wave_cap,
                    overflow.len()
                );
                pending_waves.push_front(overflow);
            }
            wave_idx_counter += 1;
            let wave_idx = wave_idx_counter - 1;
            // Waves-remaining estimate for the log line. initial_total_waves
            // was the pre-split count; after adaptive splits, the queue can
            // temporarily hold more. Show the larger of the two so the line
            // never lies about progress.
            let total_est = initial_total_waves.max(wave_idx_counter + pending_waves.len());
            info!(
                "=== Wave {}/{}: {} issue(s) ===",
                wave_idx + 1,
                total_est,
                wave.len()
            );

            let wave_results = self
                .process_wave(&wave, &pool, parallelism, test_command, &current_base)
                .await;

            // 6. Apply successful fixes from this wave onto the batch branch.
            //
            // The worker tasks already released their worktrees back to the
            // pool and collected the fix's commit SHAs (see parallel.rs task
            // closure). Here we just cherry-pick those SHAs — the branch refs
            // live in shared `.git/refs`, so even though each worktree has
            // been reused by a later task, the commit objects remain
            // reachable.
            let mut wave_fixed = 0usize;
            // Rules of issues that actually landed a commit in this wave.
            // Drives the "skip post-wave full-suite for trivial-only waves"
            // optimisation: if every applied fix is a purely-local text edit
            // (S1118, S1488, etc.) the cross-test interaction risk is near
            // zero and the targeted tests already ran successfully.
            let mut wave_fixed_rules: Vec<String> = Vec::new();
            for (issue, result, commit_shas) in wave_results {
                match &result.status {
                    FixStatus::Fixed => {
                        if commit_shas.is_empty() {
                            warn!(
                                "No commits collected for {} — treating as unapplied",
                                issue.key
                            );
                        } else {
                            let mut applied = 0usize;
                            let mut failed_sha: Option<(String, anyhow::Error)> = None;
                            for sha in &commit_shas {
                                match git::cherry_pick(&self.config.path, sha) {
                                    Ok(_) => applied += 1,
                                    Err(e) => {
                                        failed_sha = Some((sha.clone(), e));
                                        break;
                                    }
                                }
                            }
                            if let Some((sha, e)) = failed_sha {
                                error!(
                                    "Cherry-pick {} failed for {}: {} — skipping remaining commits",
                                    sha, issue.key, e
                                );
                            }
                            if applied > 0 {
                                info!(
                                    "Applied {}/{} commit(s) for {} onto batch branch",
                                    applied,
                                    commit_shas.len(),
                                    issue.key
                                );
                                wave_fixed += 1;
                                wave_fixed_rules.push(issue.rule.clone());
                            } else {
                                warn!("No commits for {} could be applied", issue.key);
                            }
                        }
                        report::append_changelog(&self.config.path, &result);
                        self.results.push(result);
                    }
                    FixStatus::NeedsReview(reason) => {
                        warn!("Issue {} needs review: {}", issue.key, reason);
                        report::append_changelog(&self.config.path, &result);
                        self.results.push(result);
                    }
                    FixStatus::Failed(err) => {
                        error!("Issue {} failed: {}", issue.key, err);
                        report::append_changelog(&self.config.path, &result);
                        self.results.push(result);
                    }
                    FixStatus::Skipped(reason) => {
                        info!("Issue {} skipped: {}", issue.key, reason);
                        self.results.push(result);
                    }
                    FixStatus::RiskSkipped(reason) => {
                        info!("Issue {} risk-skipped: {}", issue.key, reason);
                        self.results.push(result);
                    }
                }
                // Worktree release now happens inside the worker task, not here.
            }

            info!(
                "Wave {}/{} complete: {}/{} fixed",
                wave_idx + 1,
                total_est,
                wave_fixed,
                wave.len()
            );

            // 7. After applying wave fixes, run tests once on the batch branch
            // to catch any cross-issue interactions (e.g., two fixes that are
            // individually correct but interact badly together).
            //
            // `post_wave_reverted` lets us skip the post-wave SonarQube scan
            // when the wave was already reverted + re-processed sequentially
            // (sequential `process_issue` already runs its own per-issue scan).
            let mut post_wave_reverted = false;
            let mut bisect_drop_rate: f32 = 0.0;
            // Trivial-only optimisation: if every fix applied in this wave
            // touches a purely-local rule (S1118, S1488, S1130, …), the
            // per-fix targeted tests have already verified correctness; the
            // full-suite run is essentially redundant. Skipping it saves
            // 1–2 min per wave on large runs. Requires at least one fix.
            let all_trivial = wave_fixed > 0
                && wave_fixed_rules
                    .iter()
                    .all(|r| crate::orchestrator::deterministic::is_trivial_local_rule(r));
            if wave_fixed > 1 && !test_command.is_empty() && !all_trivial {
                info!("Running tests on batch branch after wave to check for interactions...");
                match crate::runner::run_tests(
                    &self.config.path,
                    test_command,
                    self.config.test_timeout,
                ) {
                    Ok((true, _)) => {
                        info!("Post-wave test run passed");
                    }
                    Ok((false, output)) => {
                        warn!(
                            "Post-wave tests FAILED — bisecting to preserve passing commits.\n{}",
                            truncate(&output, 300)
                        );
                        // Bisect with early-bail: if the first probe shows
                        // a majority of commits are bad, abort bisect and
                        // hand everything to sequential fallback — cheaper
                        // than completing a logN search that will drop them
                        // all anyway (observed at ~14 min/wave on large sets).
                        let dropped_keys = self
                            .bisect_wave_commits(&wave, &current_base, test_command)
                            .await;
                        if dropped_keys.is_empty() {
                            info!(
                                "Bisect kept all {} wave commit(s); post-wave failure was transient",
                                wave.len()
                            );
                        } else {
                            info!(
                                "Bisect kept {}/{} wave commit(s); dropped {} issue(s): {:?}",
                                wave.len() - dropped_keys.len(),
                                wave.len(),
                                dropped_keys.len(),
                                dropped_keys
                            );
                            // Drop only the failed-issue results
                            let dropped_set: std::collections::HashSet<String> =
                                dropped_keys.iter().cloned().collect();
                            self.results.retain(|r| !dropped_set.contains(&r.issue_key));
                            // Sequential fallback runs only on the dropped issues
                            let dropped_wave: Vec<Issue> = wave
                                .iter()
                                .filter(|i| dropped_set.contains(&i.key))
                                .cloned()
                                .collect();
                            self.process_wave_sequentially(&dropped_wave, test_command).await;
                        }
                        post_wave_reverted = !dropped_keys.is_empty();
                        if !wave.is_empty() {
                            bisect_drop_rate =
                                dropped_keys.len() as f32 / wave.len() as f32;
                        }
                    }
                    Err(e) => {
                        warn!("Post-wave test run error: {} — continuing", e);
                    }
                }
            } else if all_trivial && wave_fixed > 1 {
                info!(
                    "Skipping post-wave full-suite test: all {} fixes on trivial-local rules \
                     (targeted tests already passed per fix)",
                    wave_fixed
                );
            }

            // Adaptive wave-size adjustment (Change #3). If ≥50% of this
            // wave's commits were dropped by bisect, halve the cap for the
            // next wave — the current combination is over-parallelised for
            // this codebase's cross-test interactions. Reset to full
            // parallelism after a clean wave (bisect not triggered or
            // dropped nothing).
            if bisect_drop_rate >= 0.5 {
                let new_cap = (dynamic_wave_cap / 2).max(1);
                if new_cap < dynamic_wave_cap {
                    warn!(
                        "Adaptive sizing: bisect dropped {:.0}% of wave — reducing cap {} → {}",
                        bisect_drop_rate * 100.0,
                        dynamic_wave_cap,
                        new_cap
                    );
                    dynamic_wave_cap = new_cap;
                }
            } else if !post_wave_reverted && dynamic_wave_cap < parallelism {
                // Clean wave — gradually restore parallelism.
                let new_cap = (dynamic_wave_cap * 2).min(parallelism);
                info!(
                    "Adaptive sizing: clean wave — restoring cap {} → {}",
                    dynamic_wave_cap, new_cap
                );
                dynamic_wave_cap = new_cap;
            }

            // 8. Post-wave SonarQube verification.
            //
            // Workers ran with skip_scan=true, so none of the wave's fixes were
            // server-verified. When `wave_scan_batch == 1` we scan this wave
            // now (original behavior). When > 1 we accumulate this wave's
            // issues and scan once every N waves (or on the last wave),
            // amortising the ~20-30 s scanner + CE cost across many fixes.
            // Skipped entirely when the wave was reverted above (sequential
            // fallback already does per-issue scans).
            if !post_wave_reverted && wave_fixed > 0 && self.config.scanner.is_some() {
                pending_scan_issues.extend(wave.iter().cloned());
            }

            let is_last_wave = pending_waves.is_empty();
            let waves_since_scan = (wave_idx + 1) % wave_scan_batch == 0;
            let should_run_scan = !pending_scan_issues.is_empty()
                && self.config.scanner.is_some()
                && (is_last_wave || waves_since_scan);
            if should_run_scan {
                if wave_scan_batch > 1 {
                    info!(
                        "Batched post-wave scan: {} issue(s) accumulated across up to {} wave(s)",
                        pending_scan_issues.len(),
                        wave_scan_batch
                    );
                }
                let batched = std::mem::take(&mut pending_scan_issues);
                if let Some(unresolved) = self
                    .post_wave_sonar_check(&batched, &current_base)
                    .await
                {
                    for issue in unresolved {
                        pending_retries.push(issue);
                    }
                }
            }

            // Advance the wave base to whatever the main tree landed on.
            // Subsequent waves' workers will branch from this SHA, so fixes
            // on files touched by earlier waves start from the already-patched
            // state and their diffs apply cleanly on cherry-pick.
            // If `wave_fixed == 0` the HEAD hasn't moved, so this is a no-op.
            match git::resolve_sha(&self.config.path, "HEAD") {
                Ok(sha) => current_base = sha,
                Err(e) => warn!(
                    "Could not snapshot HEAD after wave {}: {} — next wave may conflict",
                    wave_idx + 1,
                    e
                ),
            }
        }

        // 9. Sequential retry pass for issues that parallel workers marked Fixed
        // but that SonarQube still reports. Each goes through the full sequential
        // `process_issue` (fix + build + tests + per-issue rescan), so its retry
        // loop can iteratively refine on top of whatever commit the wave left.
        if !pending_retries.is_empty() {
            info!(
                "Sequential retry pass: {} issue(s) still reported after parallel waves",
                pending_retries.len()
            );
            self.process_wave_sequentially(&pending_retries, test_command).await;
        }

        info!(
            "Wave-parallel complete: {} total results",
            self.results.len()
        );

        // Pool cleanup happens on drop
        Ok(())
    }

    /// Process a single wave: dispatch each issue to a separate worktree worker
    /// and collect (issue, result, wt_root) triples.
    ///
    /// The worktrees are NOT released here — the caller is responsible for
    /// calling `pool.release(wt_root)` after applying the results.
    async fn process_wave(
        &self,
        wave: &[Issue],
        pool: &Arc<WorktreePool>,
        parallelism: usize,
        test_command: &str,
        base_branch: &str,
    ) -> Vec<(Issue, IssueResult, Vec<String>)> {
        let conc_sem = Arc::new(Semaphore::new(parallelism));

        // US-082: intra-wave batching. Group findings by (component, rule)
        // before dispatch so 8 occurrences of S2629 in BasicHibernateDAOImpl
        // become a single AI call instead of 8. Safe at this layer because a
        // wave is atomic — there's no Sonar rescan between intra-wave issues
        // that could resurrect individual findings (which is why the global
        // grouping in mod.rs is restricted to lint:* findings).
        //
        // The synthesized representative carries `key = batch:N:KEY` and a
        // message enumerating every line; downstream code in fix_loop.rs
        // already handles batched issues correctly because the lint path has
        // exercised this for a long time.
        //
        // For waves where every issue has a unique (component, rule), grouping
        // is a no-op (groups of size 1 round-trip via `into_representative`
        // returning the original issue unchanged). So this is free when it
        // doesn't apply and a big win when it does.
        let issues_to_dispatch: Vec<Issue> = if self.config.disable_wave_batching {
            wave.to_vec()
        } else {
            let pre = wave.len();
            let groups = crate::orchestrator::grouping::group_issues(
                wave.to_vec(),
                self.config.max_group_size,
            );
            let batched_count = groups.iter().filter(|g| g.is_batched()).count();
            let collapsed: Vec<Issue> =
                groups.into_iter().map(|g| g.into_representative()).collect();
            if batched_count > 0 {
                info!(
                    "Wave batching: collapsed {} Sonar finding(s) into {} dispatched item(s) ({} batched group(s))",
                    pre,
                    collapsed.len(),
                    batched_count
                );
            }
            collapsed
        };

        let mut handles = Vec::with_capacity(issues_to_dispatch.len());

        for (idx, issue) in issues_to_dispatch.iter().enumerate() {
            let pool_clone = Arc::clone(pool);
            let conc = Arc::clone(&conc_sem);
            let config = self.config.clone();
            let client = self.client.clone();
            let rule_cache = self.rule_cache.clone();
            let engine_routing = self.engine_routing.clone();
            let prompt_config = self.prompt_config.clone();
            let test_examples = self.cached_test_examples.clone();
            let exec_log = self.exec_log.clone();
            // Cross-wave (file, rule) failure memory shared with all workers.
            let failure_memory = Arc::clone(&self.fix_failure_memory);
            // US-081: cross-wave Claude session map shared with all workers.
            let session_map = Arc::clone(&self.session_map);
            let test_cmd = test_command.to_string();
            let base = base_branch.to_string();
            let issue_clone = issue.clone();
            let worker_id = idx % parallelism;

            // See the sibling spawn's comment: `.instrument()` binds the span to
            // the future so it travels with the task across `.await` yields,
            // unlike `span.enter()` which binds to the current OS thread.
            let span = tracing::info_span!(
                "wave_worker",
                id = worker_id,
                issue = %issue_clone.key,
            );
            let handle = tokio::spawn(async move {
                let _permit = conc
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("Semaphore closed: {}", e))?;

                // Acquire a worktree
                let wt_root = pool_clone.acquire_async().await;
                let wt_path = pool_clone.project_dir(&wt_root);

                // Create a dedicated branch in the worktree (branched from base)
                let branch_name = format!(
                    "fix/sonar-wave-{}",
                    issue_clone.key.to_lowercase().replace(':', "-")
                );
                if let Err(e) =
                    git::create_branch_in_worktree(&wt_root, &branch_name, &base)
                {
                    error!(
                        "[wave-worker-{}/{}] Branch creation failed: {}",
                        worker_id, issue_clone.key, e
                    );
                    let _ = git::clean_worktree(&wt_root);
                    // Release the worktree so the next task can use it — same
                    // deadlock rationale as the success path.
                    pool_clone.release(wt_root);
                    return Ok::<(Issue, IssueResult, Vec<String>), anyhow::Error>((
                        issue_clone.clone(),
                        IssueResult {
                            issue_key: issue_clone.key.clone(),
                            rule: issue_clone.rule.clone(),
                            severity: issue_clone.severity.clone(),
                            issue_type: issue_clone.issue_type.clone(),
                            message: issue_clone.message.clone(),
                            file: sonar::component_to_path(&issue_clone.component),
                            lines: format_lines(&issue_clone.text_range),
                            status: FixStatus::Failed(format!("Branch creation failed: {}", e)),
                            change_description: String::new(),
                            tests_added: Vec::new(),
                            pr_url: None,
                            diff_summary: None,
                        },
                        Vec::new(),
                    ));
                }

                info!(
                    "[wave-worker-{}/{}] Fixing {} in {}",
                    worker_id, issue_clone.key, issue_clone.key, wt_path.display()
                );

                // Build worker orchestrator (path = worktree).
                // `scanner = None` is what actually disables the per-issue rescan loop
                // in fix_loop; `skip_scan` is only read by `Config::validate()`.
                let mut worker_config = config.clone();
                worker_config.path = wt_path.clone();
                worker_config.skip_scan = true;
                worker_config.scanner = None;
                // Worktrees come out of the pool in a clean state (pool calls
                // `git clean` on release), so `mvn clean` before the fix is
                // pure overhead — skip it in fix_loop::Step A-2.
                worker_config.fresh_worktree = true;

                let worker = Orchestrator::new_worker(
                    worker_config,
                    client,
                    rule_cache,
                    engine_routing,
                    prompt_config,
                    test_examples,
                    exec_log,
                    failure_memory,
                    session_map,
                );

                // Per-issue wall-clock deadline. Prevents a single stuck issue
                // (timeouts-in-timeouts, Maven daemon stalls, Claude hanging)
                // from holding up an entire wave. The inner claude/build
                // timeouts already limit each step; this is the outer
                // safety net at the orchestration layer.
                //
                // Budget: generous enough that a worst-case legitimate path
                // (fix 600s + build 60s + targeted tests 60s + build-repair
                // 720s + build 60s + tests 60s ≈ 25 min) fits, but short
                // enough to unblock the wave instead of waiting for the
                // configured 1h timeout.
                // 3× claude_timeout mirrors the existing prompt_floor cap in
                // run_ai_with_invocation — a natural "at most three AI calls
                // plus overhead" envelope. With claude_timeout=600 → 1800 s.
                let issue_deadline = std::time::Duration::from_secs(
                    worker.config.claude_timeout.saturating_mul(3)
                );
                let result = match tokio::time::timeout(
                    issue_deadline,
                    worker.process_issue(&issue_clone, &test_cmd),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        warn!(
                            "[wave-worker-{}] per-issue deadline ({} s) elapsed for {} — aborting, releasing worktree",
                            worker_id,
                            issue_deadline.as_secs(),
                            issue_clone.key
                        );
                        IssueResult {
                            issue_key: issue_clone.key.clone(),
                            rule: issue_clone.rule.clone(),
                            severity: issue_clone.severity.clone(),
                            issue_type: issue_clone.issue_type.clone(),
                            message: issue_clone.message.clone(),
                            file: sonar::component_to_path(&issue_clone.component),
                            lines: format_lines(&issue_clone.text_range),
                            status: FixStatus::Failed(format!(
                                "Per-issue wall-clock deadline exceeded ({} s) — aborted",
                                issue_deadline.as_secs()
                            )),
                            change_description: String::new(),
                            tests_added: Vec::new(),
                            pr_url: None,
                            diff_summary: None,
                        }
                    }
                };
                info!(
                    "[wave-worker-{}] process_issue returned for {} with status {:?}",
                    worker_id,
                    issue_clone.key,
                    std::mem::discriminant(&result.status),
                );

                // Collect the SHAs produced by this fix BEFORE releasing the
                // worktree. `git::get_commits_since` needs the worktree path
                // to resolve the branch's HEAD; once the worktree is detached
                // and reused by the next task, we can't rediscover the
                // per-issue commits. The branch *ref* survives in shared
                // `.git/refs`, so the SHAs remain cherry-pickable from the
                // main tree.
                let commit_shas: Vec<String> = if matches!(result.status, FixStatus::Fixed) {
                    match git::get_commits_since(&wt_path, &base) {
                        Ok(shas) => shas,
                        Err(e) => {
                            warn!(
                                "get_commits_since failed for {} in worktree {}: {}",
                                issue_clone.key,
                                wt_path.display(),
                                e
                            );
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };

                drop(worker);
                info!("[wave-worker-{}] worker dropped for {}", worker_id, issue_clone.key);

                // Release the worktree BACK TO THE POOL INSIDE THE TASK.
                //
                // The previous design returned `wt_root` in the result tuple
                // and released it in `run_wave_parallel_fixes` only AFTER
                // `process_wave` returned all 22 results. That deadlocks any
                // wave larger than the pool size: tasks 5..N get semaphore
                // permits but spin in `pool.acquire_async()` because tasks
                // 1..pool_size still hold worktrees hostage inside their
                // already-completed results. `process_wave` can't return
                // until those tasks complete, those tasks can't complete
                // without worktrees, worktrees can't be released until
                // `process_wave` returns. Deadlock — the hang the user saw
                // at "handle N/22 resolved" with N == pool_size.
                let _ = git::clean_worktree(&wt_root);
                pool_clone.release(wt_root);
                info!(
                    "[wave-worker-{}] worktree released for {} ({} commit(s) collected)",
                    worker_id,
                    issue_clone.key,
                    commit_shas.len()
                );

                Ok((issue_clone, result, commit_shas))
            }.instrument(span));

            handles.push(handle);
        }

        // Collect results in submission order (preserves determinism for cherry-pick).
        // Each iteration logs before and after `handle.await` so a hang in the
        // collection loop (e.g., tokio can't schedule the next task because all
        // worker threads are blocked in sync subprocess calls) is pinpointed
        // to a specific handle index.
        let total = handles.len();
        let mut results = Vec::with_capacity(total);
        for (idx, handle) in handles.into_iter().enumerate() {
            tracing::debug!("process_wave: awaiting handle {}/{}", idx + 1, total);
            match handle.await {
                Ok(Ok(triple)) => {
                    info!(
                        "process_wave: handle {}/{} resolved for {}",
                        idx + 1,
                        total,
                        triple.0.key
                    );
                    results.push(triple);
                }
                Ok(Err(e)) => error!("Wave worker task error (handle {}): {}", idx + 1, e),
                Err(e) => error!("Wave worker panicked (handle {}): {}", idx + 1, e),
            }
        }
        info!("process_wave: all {} handles collected", total);
        results
    }

    /// Run a single SonarQube scan after a wave and return any wave issues
    /// that the server still reports. Returns `None` if the scan couldn't run
    /// (missing scanner, server error, etc.) — in that case we log and move on
    /// rather than pessimistically requeuing every issue.
    ///
    /// For issues still reported: the per-issue `Fixed` result is demoted out
    /// of `self.results` so the follow-up sequential retry can replace it with
    /// its own final status.
    async fn post_wave_sonar_check(
        &mut self,
        wave: &[Issue],
        _base_branch: &str,
    ) -> Option<Vec<Issue>> {
        let scanner = self.config.scanner.as_ref()?;
        info!(
            "Post-wave SonarQube scan for {} fixed issue(s)...",
            wave.len()
        );
        let ce_task_id = match self
            .client
            .run_scanner(&self.config.path, scanner, &self.config.branch)
        {
            Ok(id) => id,
            Err(e) => {
                warn!("Post-wave scan failed: {} — skipping verification", e);
                return None;
            }
        };
        if let Err(e) = self.client.wait_for_analysis(ce_task_id.as_deref()).await {
            warn!("Post-wave analysis wait failed: {} — skipping verification", e);
            return None;
        }
        let open_issues = match self.client.fetch_issues().await {
            Ok(issues) => issues,
            Err(e) => {
                warn!("Post-wave issue fetch failed: {} — skipping verification", e);
                return None;
            }
        };
        let open_keys: std::collections::HashSet<&str> =
            open_issues.iter().map(|i| i.key.as_str()).collect();

        let mut unresolved = Vec::new();
        let wave_fixed_keys: std::collections::HashSet<&str> = self
            .results
            .iter()
            .filter(|r| matches!(r.status, FixStatus::Fixed))
            .map(|r| r.issue_key.as_str())
            .collect();
        for issue in wave {
            if !wave_fixed_keys.contains(issue.key.as_str()) {
                continue; // wave worker already marked this non-Fixed
            }
            if open_keys.contains(issue.key.as_str()) {
                unresolved.push(issue.clone());
            }
        }

        if unresolved.is_empty() {
            info!("Post-wave scan: all {} wave fix(es) verified by SonarQube", wave.len());
            return Some(unresolved);
        }

        warn!(
            "Post-wave scan: {} fix(es) still reported by SonarQube — queuing for sequential retry",
            unresolved.len()
        );
        let unresolved_keys: std::collections::HashSet<String> =
            unresolved.iter().map(|i| i.key.clone()).collect();
        self.results
            .retain(|r| !unresolved_keys.contains(&r.issue_key));
        Some(unresolved)
    }

    /// Identify commits that broke the wave's post-run tests and drop them.
    ///
    /// Algorithm: linear-reject with early bail.
    ///
    ///   1. Apply commits one at a time onto `base_branch`, testing after each
    ///      addition.
    ///   2. If the kept set still passes tests → keep the commit.
    ///   3. If it fails → drop that single commit, revert to the last-known
    ///      passing state, and continue.
    ///   4. Early bail: if ≥50% of commits are dropped in the first half of
    ///      the scan, abandon bisect and drop everything remaining (the
    ///      sequential fallback is ~5× faster than finishing this scan when
    ///      we already know most will fail).
    ///
    /// Cost: O(N) test-suite runs worst case vs O(K·log N) for the previous
    /// binary search. On observed hot-file-conflict waves (N=5, all bad) this
    /// is 5 test runs instead of 12–14. On the common case (N=5, 0–1 bad)
    /// it's also 5 runs vs the old 1–3, so we pay a modest penalty on clean
    /// waves to win big on the ones that triggered the bisect in the first
    /// place.
    ///
    /// Returns the issue keys whose commits were dropped.
    async fn bisect_wave_commits(
        &mut self,
        wave: &[Issue],
        base_branch: &str,
        test_command: &str,
    ) -> Vec<String> {
        let commits = match git::get_commits_since(&self.config.path, base_branch) {
            Ok(c) => c,
            Err(e) => {
                warn!("Bisect: could not list wave commits ({}); falling back to full revert", e);
                self.hard_reset_to(base_branch);
                return wave.iter().map(|i| i.key.clone()).collect();
            }
        };
        if commits.is_empty() {
            return Vec::new();
        }

        // Map each commit SHA to the issue key recorded in its message.
        // Wip commits follow the pattern: "reparo-wip: fix <key> - ..."
        let sha_to_key = map_shas_to_issue_keys(&self.config.path, &commits);
        let total = commits.len();

        let mut kept: Vec<String> = Vec::with_capacity(total);
        let mut dropped_keys: Vec<String> = Vec::new();

        // Start from base, then add commits one at a time. After each
        // addition, test. A commit that breaks tests is reverted
        // (rebuilding kept without it) and its issue key recorded.
        self.hard_reset_to(base_branch);

        for (i, sha) in commits.iter().enumerate() {
            // Early bail: if half the wave has already been dropped,
            // hand the rest to the sequential fallback directly. Each
            // extra test run costs ~1–2 min and we're on track to drop
            // them all anyway.
            if !kept.is_empty() || !dropped_keys.is_empty() {
                let half = total / 2;
                if dropped_keys.len() >= half.max(1) && i >= half {
                    warn!(
                        "Bisect early-bail: dropped {}/{} already; handing remaining {} commit(s) \
                         to sequential fallback",
                        dropped_keys.len(),
                        total,
                        total - i
                    );
                    for rest_sha in &commits[i..] {
                        if let Some(k) = sha_to_key.get(rest_sha) {
                            if !dropped_keys.contains(k) {
                                dropped_keys.push(k.clone());
                            }
                        }
                    }
                    break;
                }
            }

            // Try applying this commit on top of the current kept set.
            if git::cherry_pick(&self.config.path, sha).is_err() {
                // Cherry-pick failure → can't include, record as dropped.
                let _ = std::process::Command::new("git")
                    .current_dir(&self.config.path)
                    .args(["cherry-pick", "--abort"])
                    .output();
                if let Some(k) = sha_to_key.get(sha) {
                    if !dropped_keys.contains(k) {
                        dropped_keys.push(k.clone());
                    }
                }
                continue;
            }

            if self.test_passes(test_command) {
                kept.push(sha.clone());
            } else {
                // Revert this commit by rewinding and replaying kept.
                if let Some(k) = sha_to_key.get(sha) {
                    if !dropped_keys.contains(k) {
                        dropped_keys.push(k.clone());
                    }
                }
                self.hard_reset_to(base_branch);
                if !self.replay_commits(base_branch, &kept) {
                    // Kept set no longer applies cleanly — something
                    // anomalous happened (tree state drift). Drop
                    // everything to be safe.
                    warn!(
                        "Bisect: replay of {} kept commit(s) failed after revert; \
                         dropping all remaining",
                        kept.len()
                    );
                    for rest_sha in &commits[i..] {
                        if let Some(k) = sha_to_key.get(rest_sha) {
                            if !dropped_keys.contains(k) {
                                dropped_keys.push(k.clone());
                            }
                        }
                    }
                    // Also mark previously "kept" as dropped since we
                    // couldn't restore them.
                    for kept_sha in &kept {
                        if let Some(k) = sha_to_key.get(kept_sha) {
                            if !dropped_keys.contains(k) {
                                dropped_keys.push(k.clone());
                            }
                        }
                    }
                    kept.clear();
                    self.hard_reset_to(base_branch);
                    break;
                }
            }
        }

        // HEAD is already at `base + kept` from the loop. Ensure it.
        if !self.replay_commits(base_branch, &kept) {
            // Extremely defensive — if even the final replay fails,
            // hard-reset to base so the working tree is at least consistent.
            self.hard_reset_to(base_branch);
        }
        dropped_keys
    }

    /// Hard-reset HEAD to `base_branch`. Used by bisect to start each replay
    /// from a known-clean state.
    fn hard_reset_to(&self, base_branch: &str) {
        let _ = std::process::Command::new("git")
            .current_dir(&self.config.path)
            .args(["reset", "--hard", base_branch])
            .output();
    }

    /// Reset to `base_branch` then cherry-pick the given commits in order.
    /// Returns false on any cherry-pick failure (the tree is left at whatever
    /// partial state the caller should treat as failed).
    fn replay_commits(&self, base_branch: &str, commits: &[String]) -> bool {
        self.hard_reset_to(base_branch);
        for sha in commits {
            if git::cherry_pick(&self.config.path, sha).is_err() {
                return false;
            }
        }
        true
    }

    /// Run the full test suite; return true when it passes.
    fn test_passes(&self, test_command: &str) -> bool {
        matches!(
            crate::runner::run_tests(&self.config.path, test_command, self.config.test_timeout),
            Ok((true, _))
        )
    }

    /// Fallback: process a set of issues sequentially on the main tree.
    /// Used when the post-wave interaction test fails.
    async fn process_wave_sequentially(&mut self, wave: &[Issue], test_command: &str) {
        // Entering sequential fallback after a wave-revert/bisect: the working
        // tree was just hard-reset, so the first fix here must re-clean.
        self.needs_clean
            .store(true, std::sync::atomic::Ordering::Relaxed);
        for issue in wave {
            let result = self.process_issue(issue, test_command).await;
            match &result.status {
                FixStatus::Fixed => info!("Sequential fallback: {} fixed", issue.key),
                FixStatus::NeedsReview(r) => warn!("Sequential fallback: {} needs review: {}", issue.key, r),
                FixStatus::Failed(e) => error!("Sequential fallback: {} failed: {}", issue.key, e),
                _ => {}
            }
            if !matches!(result.status, FixStatus::Fixed) {
                self.needs_clean
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            report::append_changelog(&self.config.path, &result);
            self.results.push(result);
        }
    }
}

/// Map each commit SHA to the issue key recorded in its commit message.
///
/// Wip commits created by `process_issue` follow the format
/// `reparo-wip: fix <issue_key> - <short description>`. We read each commit's
/// subject line with `git log -1 --format=%s <sha>` and extract the key.
/// Commits with no recognizable key (e.g. test-scaffolding commits) are
/// silently skipped — bisect can still drop them, it just won't know which
/// issue to re-queue.
fn map_shas_to_issue_keys(
    repo_path: &std::path::Path,
    shas: &[String],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for sha in shas {
        let out = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(["log", "-1", "--format=%s", sha])
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Expected: "reparo-wip: fix <key> - <desc>"
        if let Some(rest) = subject.strip_prefix("reparo-wip: fix ") {
            // Keys may be UUIDs (contain '-'), so split on whitespace only.
            let key = rest.split_whitespace().next().unwrap_or("").trim();
            if !key.is_empty() {
                map.insert(sha.clone(), key.to_string());
            }
        }
    }
    map
}

/// Group a slice of issues into waves such that no two issues in the same wave
/// share an affinity key (see `sonar::component_to_affinity_key`).
///
/// - `depth == 0` keeps the legacy behavior: only two issues on the same file
///   conflict.
/// - `depth == 1` (default) treats two issues in the same directory (package)
///   as conflicting — fixes to sibling files often collide in cherry-pick
///   because they share imports/constructors.
///
/// The algorithm is greedy: each issue is placed in the first wave that does
/// not already contain an issue for the same affinity key. This minimises the
/// number of waves (and therefore the number of sequential synchronisation
/// points).
#[cfg(test)]
pub(super) fn group_issues_into_waves(issues: &[Issue], depth: usize) -> Vec<Vec<Issue>> {
    group_issues_into_waves_with_hot(issues, depth, &HashSet::new())
}

/// Like `group_issues_into_waves` but also enforces at-most-one hot-file
/// issue per wave, and front-loads deterministic-fast-path issues.
///
/// A "hot file" is one with many pending issues (≥ HOT_FILE_THRESHOLD).
/// Empirically these files (EntityServiceImpl, DomainObjectDAOImpl,
/// ReflectionUtils, …) receive fixes in nearly every wave. Each individual
/// fix passes its targeted tests in isolation, but combining multiple
/// hot-file fixes in the same batch frequently fails the full-suite
/// post-wave check — triggering a ~15 min bisect that drops 80–100% of the
/// wave. Serialising hot-file fixes (at most one per wave) costs some
/// parallelism but eliminates that storm.
///
/// Deterministic-fast-path rules (S1118, S1124) are placed at the front of
/// the input so early waves finish in seconds: no Claude call, just a
/// templated edit. The user sees green commits sooner; if the run is
/// interrupted, we've harvested the cheap wins first.
pub(super) fn group_issues_into_waves_with_hot(
    issues: &[Issue],
    depth: usize,
    hot_files: &HashSet<String>,
) -> Vec<Vec<Issue>> {
    // Stable sort: deterministic rules first, then the rest in input order.
    // (Rust's sort is stable, so equal-key entries keep their relative order.)
    let mut sorted: Vec<&Issue> = issues.iter().collect();
    sorted.sort_by_key(|i| {
        // false < true, so negate: deterministic comes first.
        !crate::orchestrator::deterministic::has_deterministic_fix(&i.rule)
    });

    let mut waves: Vec<Vec<Issue>> = Vec::new();
    for issue in sorted {
        let affinity = sonar::component_to_affinity_key(&issue.component, depth);
        let path = sonar::component_to_path(&issue.component);
        let is_hot = hot_files.contains(&path);
        let slot = waves.iter_mut().find(|w| {
            for i in w.iter() {
                // (a) affinity bucket (file/package).
                if sonar::component_to_affinity_key(&i.component, depth) == affinity {
                    return false;
                }
                // (b) same (file, rule) — always produces overlapping diffs.
                if i.component == issue.component && i.rule == issue.rule {
                    return false;
                }
                // (c) hot-file collision — only the *same* hot file
                //     conflicts. Two issues on *different* hot files are
                //     independent, so different worktrees process them in
                //     parallel.
                //
                //     Run 2026-04-28 evidence: with the previous "any-hot
                //     vs any-hot" rule, the second half of the run (waves
                //     58–124) collapsed to wave_worker_0 only — every
                //     remaining issue touched some hot file, so the rule
                //     forced wave_size=1 across 4 workers, leaving 75%
                //     of capacity idle. Allowing distinct hot files to
                //     co-exist restores wave_size up to N_workers as long
                //     as the affinity bucket (rule a) and same-file/rule
                //     (rule b) constraints are still respected.
                if is_hot && sonar::component_to_path(&i.component) == path {
                    return false;
                }
            }
            true
        });
        if let Some(w) = slot {
            w.push(issue.clone());
        } else {
            waves.push(vec![issue.clone()]);
        }
    }
    waves
}

/// Identify files with ≥ HOT_FILE_THRESHOLD issues in the candidate set.
/// Pre-marking these at schedule time spreads their fixes across waves
/// instead of discovering the interaction at bisect time.
pub(super) fn detect_hot_files(issues: &[Issue]) -> HashSet<String> {
    const HOT_FILE_THRESHOLD: usize = 3;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for issue in issues {
        *counts.entry(sonar::component_to_path(&issue.component)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(file, n)| if n >= HOT_FILE_THRESHOLD { Some(file) } else { None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonar::{Issue, TextRange};

    fn make_issue(key: &str, component: &str) -> Issue {
        Issue {
            key: key.to_string(),
            rule: "java:S1128".to_string(),
            severity: "MINOR".to_string(),
            component: component.to_string(),
            issue_type: "CODE_SMELL".to_string(),
            message: "test".to_string(),
            text_range: Some(TextRange { start_line: 1, end_line: 2, start_offset: None, end_offset: None }),
            status: "OPEN".to_string(),
            tags: vec![],
            effort: None,
        }
    }

    #[test]
    fn test_group_same_file_into_separate_waves() {
        let issues = vec![
            make_issue("A", "myproject:src/Foo.java"),
            make_issue("B", "myproject:src/Foo.java"),
        ];
        let waves = group_issues_into_waves(&issues, 0);
        assert_eq!(waves.len(), 2, "two issues on the same file need two waves");
        assert_eq!(waves[0].len(), 1);
        assert_eq!(waves[1].len(), 1);
    }

    #[test]
    fn test_group_different_files_into_one_wave() {
        let issues = vec![
            make_issue("A", "myproject:src/Foo.java"),
            make_issue("B", "myproject:src/Bar.java"),
            make_issue("C", "myproject:src/Baz.java"),
        ];
        let waves = group_issues_into_waves(&issues, 0);
        assert_eq!(waves.len(), 1, "three different files fit in one wave");
        assert_eq!(waves[0].len(), 3);
    }

    #[test]
    fn test_group_mixed_files() {
        // A and B on Foo, C and D on Bar — 2 waves: (A,C) and (B,D)
        let issues = vec![
            make_issue("A", "p:src/Foo.java"),
            make_issue("B", "p:src/Foo.java"),
            make_issue("C", "p:src/Bar.java"),
            make_issue("D", "p:src/Bar.java"),
        ];
        let waves = group_issues_into_waves(&issues, 0);
        assert_eq!(waves.len(), 2);
        // Each wave has one Foo and one Bar issue
        for wave in &waves {
            let files: Vec<_> = wave.iter().map(|i| sonar::component_to_path(&i.component)).collect();
            assert_eq!(files.len(), 2);
            assert!(files.contains(&"src/Foo.java".to_string()));
            assert!(files.contains(&"src/Bar.java".to_string()));
        }
    }

    #[test]
    fn test_group_empty_issues() {
        let waves = group_issues_into_waves(&[], 0);
        assert!(waves.is_empty());
    }

    #[test]
    fn test_group_package_depth_separates_same_package() {
        // Same parent directory (auth/) — with depth=1 they must serialize.
        let issues = vec![
            make_issue("A", "p:src/main/java/com/x/auth/Foo.java"),
            make_issue("B", "p:src/main/java/com/x/auth/Bar.java"),
            make_issue("C", "p:src/main/java/com/x/other/Baz.java"),
        ];
        let waves = group_issues_into_waves(&issues, 1);
        // A and C can run together (different parent dirs); B shares auth/ with A
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].len(), 2);
        assert_eq!(waves[1].len(), 1);
    }

    #[test]
    fn test_group_same_rule_same_file_separates_even_across_packages() {
        // Two S1168 on the SAME file must land in different waves even when
        // depth=1 would normally consider them "same package" (which is a
        // superset already). Regression: an older version deduped only by
        // affinity, so same-rule-same-file pairs in different packages could
        // race on cherry-pick.
        let a = Issue {
            key: "A".into(), rule: "java:S1168".into(), severity: "MAJOR".into(),
            component: "p:src/main/java/com/x/a/Foo.java".into(),
            issue_type: "CODE_SMELL".into(), message: "t".into(),
            text_range: None, status: "OPEN".into(), tags: vec![], effort: None,
        };
        let b = Issue { key: "B".into(), ..a.clone() };
        // Different package, same rule+file? Impossible — reuse `a` path to
        // assert the more common real case: two issues on exact same file.
        let waves = group_issues_into_waves(&[a, b], 1);
        assert_eq!(waves.len(), 2, "same-(rule,file) must serialise");
    }

    #[test]
    fn test_group_package_depth_0_same_as_legacy() {
        let issues = vec![
            make_issue("A", "p:src/main/java/com/x/auth/Foo.java"),
            make_issue("B", "p:src/main/java/com/x/auth/Bar.java"),
        ];
        // depth=0 — different files, same package is fine in one wave.
        let waves = group_issues_into_waves(&issues, 0);
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 2);
    }

    #[test]
    fn test_detect_hot_files_threshold() {
        let issues = vec![
            make_issue("A1", "p:src/Hot.java"),
            make_issue("A2", "p:src/Hot.java"),
            make_issue("A3", "p:src/Hot.java"),
            make_issue("B1", "p:src/Cold.java"),
            make_issue("B2", "p:src/Cold.java"),
        ];
        let hot = detect_hot_files(&issues);
        assert!(hot.contains("src/Hot.java"), "3 issues should be hot");
        assert!(!hot.contains("src/Cold.java"), "2 issues should not be hot");
    }

    #[test]
    fn test_hot_files_distinct_files_parallelise() {
        // Five hot-file issues in five DIFFERENT files. Run 2026-04-28
        // showed the previous "any-hot vs any-hot" rule serialised these
        // into 5 separate waves, leaving 3 of 4 worker slots idle. The
        // updated rule only blocks same-hot-file collisions, so distinct
        // hot files belong in the same wave (subject to affinity at
        // depth 0 ≡ no affinity bucketing).
        let issues = vec![
            make_issue("A", "p:src/HotA.java"),
            make_issue("B", "p:src/HotB.java"),
            make_issue("C", "p:src/HotC.java"),
            make_issue("D", "p:src/HotD.java"),
            make_issue("E", "p:src/HotE.java"),
        ];
        let hot: HashSet<String> = [
            "src/HotA.java", "src/HotB.java", "src/HotC.java",
            "src/HotD.java", "src/HotE.java",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let waves = group_issues_into_waves_with_hot(&issues, 0, &hot);
        assert_eq!(
            waves.len(),
            1,
            "five distinct hot files should pack into one wave"
        );
        assert_eq!(waves[0].len(), 5);
    }

    #[test]
    fn test_same_hot_file_still_serialises() {
        // Three issues on the SAME hot file must go into three waves —
        // serialisation is needed because their fixes touch the same
        // working tree.
        let issues = vec![
            make_issue("H1", "p:src/Hot.java"),
            make_issue("H2", "p:src/Hot.java"),
            make_issue("H3", "p:src/Hot.java"),
        ];
        let hot: HashSet<String> =
            ["src/Hot.java"].iter().map(|s| s.to_string()).collect();
        let waves = group_issues_into_waves_with_hot(&issues, 0, &hot);
        assert_eq!(waves.len(), 3, "same hot file must still serialise");
        for w in &waves {
            assert_eq!(w.len(), 1);
        }
    }

    #[test]
    fn test_hot_file_mixed_with_cold_one_per_wave() {
        // Two hot issues + three cold: each wave may carry at most one hot.
        let issues = vec![
            make_issue("H1", "p:src/Hot.java"),
            make_issue("C1", "p:src/Cold1.java"),
            make_issue("C2", "p:src/Cold2.java"),
            make_issue("H2", "p:src/Hot.java"),
        ];
        let hot: HashSet<String> =
            ["src/Hot.java"].iter().map(|s| s.to_string()).collect();
        let waves = group_issues_into_waves_with_hot(&issues, 0, &hot);
        // Can't have two Hot issues in the same wave (same file anyway).
        // Verify: each wave has ≤1 hot-file issue.
        for w in &waves {
            let hot_count = w
                .iter()
                .filter(|i| hot.contains(&sonar::component_to_path(&i.component)))
                .count();
            assert!(hot_count <= 1, "wave has {} hot issues", hot_count);
        }
    }

    #[test]
    fn test_deterministic_rules_front_loaded() {
        let trivial = Issue {
            key: "triv".into(),
            rule: "java:S1118".into(),
            severity: "MINOR".into(),
            component: "p:src/Util.java".into(),
            issue_type: "CODE_SMELL".into(),
            message: "t".into(),
            text_range: None,
            status: "OPEN".into(),
            tags: vec![],
            effort: None,
        };
        let complex = Issue {
            key: "cplx".into(),
            rule: "java:S3740".into(),
            component: "p:src/Other.java".into(),
            ..trivial.clone()
        };
        // Input order: complex first, then deterministic.
        let issues = vec![complex.clone(), trivial.clone()];
        let waves = group_issues_into_waves_with_hot(&issues, 0, &HashSet::new());
        // Single wave (different files). Deterministic must appear first.
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0][0].key, "triv", "deterministic issue should be first");
    }
}
