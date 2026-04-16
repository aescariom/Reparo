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

        // 3. Group into waves — each wave has at most one issue per file
        let waves = group_issues_into_waves(&candidates);
        info!(
            "Wave-parallel: {} issues → {} wave(s), {} worker(s)",
            candidates.len(),
            waves.len(),
            parallelism
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

        // 5. Process waves sequentially; issues within each wave run in parallel
        for (wave_idx, wave) in waves.iter().enumerate() {
            info!(
                "=== Wave {}/{}: {} issue(s) ===",
                wave_idx + 1,
                waves.len(),
                wave.len()
            );

            let wave_results = self
                .process_wave(wave, &pool, parallelism, test_command, base_branch)
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
                waves.len(),
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
            if wave_fixed > 1 && !test_command.is_empty() {
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
                            "Post-wave tests FAILED — the combined fixes in this wave interact. \
                             Reverting wave and falling back to sequential mode for these issues.\n{}",
                            truncate(&output, 300)
                        );
                        // Revert all wave commits from the batch branch
                        self.revert_wave_commits(wave, base_branch);
                        // Remove the wave results we just pushed
                        let wave_keys: std::collections::HashSet<String> =
                            wave.iter().map(|i| i.key.clone()).collect();
                        self.results.retain(|r| !wave_keys.contains(&r.issue_key));
                        // Process the wave issues sequentially as fallback
                        self.process_wave_sequentially(wave, test_command).await;
                        post_wave_reverted = true;
                    }
                    Err(e) => {
                        warn!("Post-wave test run error: {} — continuing", e);
                    }
                }
            }

            // 8. Post-wave SonarQube verification.
            //
            // Workers ran with skip_scan=true, so none of the wave's fixes were
            // server-verified. Run a single scan for the whole wave and requeue
            // any issue that the scan still reports. Skipped when the wave was
            // reverted above (sequential fallback already does per-issue scans).
            if !post_wave_reverted && wave_fixed > 0 && self.config.scanner.is_some() {
                if let Some(unresolved) = self
                    .post_wave_sonar_check(wave, base_branch)
                    .await
                {
                    for issue in unresolved {
                        pending_retries.push(issue);
                    }
                }
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
        let mut handles = Vec::with_capacity(wave.len());

        for (idx, issue) in wave.iter().enumerate() {
            let pool_clone = Arc::clone(pool);
            let conc = Arc::clone(&conc_sem);
            let config = self.config.clone();
            let client = self.client.clone();
            let rule_cache = self.rule_cache.clone();
            let engine_routing = self.engine_routing.clone();
            let prompt_config = self.prompt_config.clone();
            let test_examples = self.cached_test_examples.clone();
            let exec_log = self.exec_log.clone();
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

                let worker = Orchestrator::new_worker(
                    worker_config,
                    client,
                    rule_cache,
                    engine_routing,
                    prompt_config,
                    test_examples,
                    exec_log,
                );

                let result = worker.process_issue(&issue_clone, &test_cmd).await;
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

    /// Revert the commits that were applied for a wave (used when post-wave
    /// tests fail).  We reset to the commit that preceded the wave.
    fn revert_wave_commits(&self, wave: &[Issue], base_branch: &str) {
        // Count how many commits were applied for this wave
        let commits_to_drop = match git::get_commits_since(&self.config.path, base_branch) {
            Ok(shas) => shas.len(),
            Err(e) => {
                warn!("Could not count wave commits to revert: {}", e);
                return;
            }
        };
        if commits_to_drop == 0 {
            return;
        }
        // Soft-reset to discard the wave commits while keeping the working tree clean
        let target = format!("HEAD~{}", commits_to_drop);
        let status = std::process::Command::new("git")
            .current_dir(&self.config.path)
            .args(["reset", "--hard", &target])
            .status();
        match status {
            Ok(s) if s.success() => {
                info!(
                    "Reverted {} wave commit(s) for {} issue(s)",
                    commits_to_drop,
                    wave.len()
                );
            }
            _ => warn!("Failed to revert wave commits"),
        }
    }

    /// Fallback: process a set of issues sequentially on the main tree.
    /// Used when the post-wave interaction test fails.
    async fn process_wave_sequentially(&mut self, wave: &[Issue], test_command: &str) {
        for issue in wave {
            let result = self.process_issue(issue, test_command).await;
            match &result.status {
                FixStatus::Fixed => info!("Sequential fallback: {} fixed", issue.key),
                FixStatus::NeedsReview(r) => warn!("Sequential fallback: {} needs review: {}", issue.key, r),
                FixStatus::Failed(e) => error!("Sequential fallback: {} failed: {}", issue.key, e),
                _ => {}
            }
            report::append_changelog(&self.config.path, &result);
            self.results.push(result);
        }
    }
}

/// Group a slice of issues into waves such that no two issues in the same wave
/// touch the same source file.
///
/// The algorithm is greedy: each issue is placed in the first wave that does
/// not already contain an issue for the same file.  This minimises the number
/// of waves (and therefore the number of sequential synchronisation points).
pub(super) fn group_issues_into_waves(issues: &[Issue]) -> Vec<Vec<Issue>> {
    let mut waves: Vec<Vec<Issue>> = Vec::new();
    for issue in issues {
        let file = sonar::component_to_path(&issue.component);
        // Find the first wave that doesn't yet have an issue for this file
        let slot = waves.iter_mut().find(|w| {
            !w.iter()
                .any(|i| sonar::component_to_path(&i.component) == file)
        });
        if let Some(w) = slot {
            w.push(issue.clone());
        } else {
            waves.push(vec![issue.clone()]);
        }
    }
    waves
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
        }
    }

    #[test]
    fn test_group_same_file_into_separate_waves() {
        let issues = vec![
            make_issue("A", "myproject:src/Foo.java"),
            make_issue("B", "myproject:src/Foo.java"),
        ];
        let waves = group_issues_into_waves(&issues);
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
        let waves = group_issues_into_waves(&issues);
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
        let waves = group_issues_into_waves(&issues);
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
        let waves = group_issues_into_waves(&[]);
        assert!(waves.is_empty());
    }
}
