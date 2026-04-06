//! Parallel issue processing using git worktrees (US-018).
//!
//! When `--parallel N` (N > 1) is set, issues are dispatched to N git worktrees
//! and processed concurrently.  Each issue gets its own branch and PR.
//! Push + PR creation is serialized via a semaphore to avoid rate limiting.

use super::helpers::*;
use super::worktree_pool::WorktreePool;
use super::Orchestrator;
use crate::git;
use crate::report::{self, FixStatus, IssueResult};
use crate::sonar::Issue;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

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
            let test_cmd = test_command.to_string();
            let worker_id = idx % parallelism;

            let handle = tokio::spawn(async move {
                // Limit concurrency to pool size
                let _permit = conc_sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("Semaphore closed: {}", e))?;

                let span = tracing::info_span!(
                    "worker",
                    id = worker_id,
                    issue = %issue.key,
                );
                let _guard = span.enter();

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
                // In parallel mode, skip SonarQube re-scan (too slow, requires main tree)
                worker_config.skip_scan = true;

                let worker = Orchestrator::new_worker(
                    worker_config,
                    client,
                    rule_cache,
                    engine_routing,
                    prompt_config,
                    test_examples,
                    crate::usage::UsageTracker::new(),
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

                // Record usage from worker
                let worker_usage = worker.usage_tracker.snapshot();

                // Clean worktree for reuse (operate on worktree root, not subdir)
                let _ = git::clean_worktree(&wt_root);
                pool.release(wt_root);

                // Return both the result and usage entries
                Ok(result)
            });

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
}
