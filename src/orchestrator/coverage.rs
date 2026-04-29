use super::Orchestrator;
use super::helpers::*;
use crate::claude;
use crate::compliance;
use crate::git;
use crate::runner;
use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};

impl Orchestrator {
    /// After each file, verifies no source code was modified, then commits the tests.
    pub(super) async fn boost_coverage_to_threshold(&self, test_command: &str) -> Result<()> {
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

        // US-036: Reduce redundant coverage measurements.
        // In the cleanup and retry passes, measuring after every file is wasteful
        // — coverage is an aggregate metric and each AI call contributes a small
        // delta. We measure every N files instead and always measure when
        // approaching the threshold.
        const MEASURE_EVERY_N: usize = 3;
        // Track total measurements to show in the summary log.
        let mut total_measurements: u32 = 0;

        // Initial coverage measurement
        let overall_pct = match self.run_coverage_and_measure(&cov_cmd) {
            Some(pct) => {
                total_measurements += 1;
                pct
            }
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
        // 3. US-064: Files below min_file_branch_coverage (even with good line coverage)
        let overall_needs_boost = overall_pct < self.config.min_coverage;
        let has_file_threshold = self.config.min_file_coverage > 0.0;
        let has_branch_threshold = self.config.min_branch_coverage > 0.0;
        let has_file_branch_threshold = self.config.min_file_branch_coverage > 0.0;

        // US-064: Compute overall branch coverage from per-file data
        let (total_br, covered_br) = file_coverages.iter().fold((0u64, 0u64), |(t, c), fc| {
            (t + fc.total_branches, c + fc.covered_branches)
        });
        let overall_branch_pct = if total_br == 0 {
            100.0
        } else {
            (covered_br as f64 / total_br as f64) * 100.0
        };
        let overall_branch_needs_boost =
            has_branch_threshold && overall_branch_pct < self.config.min_branch_coverage && total_br > 0;

        let files_below_file_threshold: Vec<_> = if has_file_threshold || has_file_branch_threshold {
            file_coverages.iter()
                .filter(|fc| {
                    !is_test_file(&fc.file)
                        && ((has_file_threshold && fc.coverage_pct < self.config.min_file_coverage)
                            || (has_file_branch_threshold
                                && fc.total_branches > 0
                                && fc.branch_coverage_pct < self.config.min_file_branch_coverage))
                })
                .collect()
        } else {
            Vec::new()
        };

        if !overall_needs_boost
            && !overall_branch_needs_boost
            && files_below_file_threshold.is_empty()
        {
            color_info!(
                "Project-wide coverage {} meets {:.0}% and all files meet thresholds — no boost needed",
                cov_vs(overall_pct, self.config.min_coverage), self.config.min_coverage
            );
            if total_br > 0 {
                info!(
                    "Branch coverage: {:.1}% ({}/{} branches)",
                    overall_branch_pct, covered_br, total_br
                );
            }
            return Ok(());
        }

        if overall_needs_boost {
            color_info!("Project-wide coverage {} is below {:.0}%", cov_prev(overall_pct), self.config.min_coverage);
        }
        if overall_branch_needs_boost {
            color_info!(
                "Project-wide BRANCH coverage {:.1}% is below {:.0}% target ({}/{} branches)",
                overall_branch_pct, self.config.min_branch_coverage, covered_br, total_br
            );
        }
        if !files_below_file_threshold.is_empty() {
            info!(
                "{} file(s) below per-file thresholds (line {:.0}% / branch {:.0}%)",
                files_below_file_threshold.len(),
                self.config.min_file_coverage,
                self.config.min_file_branch_coverage
            );
        }

        // Merge all sets: files for overall boost + files below per-file line threshold
        // + files below per-file branch threshold (US-064). Deduplicated implicitly
        // because we iterate file_coverages which already contains each file once.
        let files_needing_tests: Vec<_> = file_coverages.iter()
            .filter(|fc| {
                if is_test_file(&fc.file) {
                    return false;
                }
                // Skip files with 100% line AND 100% branch coverage
                let line_full = fc.coverage_pct >= 100.0;
                let branch_full = fc.total_branches == 0 || fc.branch_coverage_pct >= 100.0;
                if line_full && branch_full {
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
                // Skip files with 0 uncovered lines AND 0 uncovered branches (rounding artifacts)
                let no_uncovered_lines = fc.total_lines <= fc.covered_lines;
                let no_uncovered_branches =
                    fc.total_branches == 0 || fc.covered_branches >= fc.total_branches;
                if no_uncovered_lines && no_uncovered_branches {
                    return false;
                }
                // Include if needed for overall boost OR below any per-file threshold
                let file_needs_line = has_file_threshold && fc.coverage_pct < self.config.min_file_coverage;
                let file_needs_branch = has_file_branch_threshold
                    && fc.total_branches > 0
                    && fc.branch_coverage_pct < self.config.min_file_branch_coverage;
                overall_needs_boost
                    || overall_branch_needs_boost
                    || file_needs_line
                    || file_needs_branch
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

        // Sort by complexity descending: most uncovered lines first, then largest file.
        // More complex files dispatch to worktrees first; parallel processing finishes
        // in minimum wall-clock time when the longest tasks start earliest.
        let mut files_needing_tests = files_needing_tests;
        files_needing_tests.sort_by(|a, b| {
            b.uncovered_lines.len().cmp(&a.uncovered_lines.len())
                .then_with(|| b.total_lines.cmp(&a.total_lines))
        });

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
            "Found {} source files needing test coverage — processing most complex first{}",
            files_needing_tests.len(),
            if batch_mode {
                format!(" (wave size: {}, commit batch: {} files)", parallel_size, commit_size)
            } else {
                String::new()
            }
        );

        let test_framework = test_command;

        // US-040: Build framework context once for all files in the boost loop
        let detected_deps = runner::detect_test_dependencies(&self.config.path);
        let framework_context_base = build_framework_context(
            &detected_deps,
            &self.config.test_generation,
        );

        // US-054: When framework context is present it already conveys stack/style;
        // reduce examples to 1 file × 12 lines.  Without framework context keep the
        // full 2 × 20 to give the AI enough style signal.
        // US-038: reuse cached_test_examples from the orchestrator when framework
        // context is empty (avoids a second glob scan of the filesystem).
        let test_examples_str = if framework_context_base.is_empty() {
            self.cached_test_examples
                .clone()
                .unwrap_or_else(|| runner::find_test_examples(&self.config.path).join("\n\n"))
        } else {
            runner::find_test_examples_limited(&self.config.path, 1, 12).join("\n\n")
        };

        let stash_prefix = "reparo-boost";

        // ── Parallel path: process files concurrently in git worktrees ──
        if self.config.coverage_parallel > 1 {
            return self.boost_coverage_parallel(
                &files_needing_tests,
                test_framework,
                &test_examples_str,
                &framework_context_base,
                overall_pct,
                &cov_cmd,
            ).await;
        }

        // ── Sequential path (default): original wave-based loop ──
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
            // US-exec-log: record a per-file step so the DB has granular data
            let current_phase = *self.current_phase_id.lock().unwrap();
            let step_id = self.exec_step_start(
                current_phase,
                "boost_file",
                Some(&fc.file),
                Some(fc.coverage_pct),
            );
            match self.generate_tests_for_file(fc, test_framework, &test_examples_str, stash_prefix, skip_test_run, &framework_context_base, false)? {
                Some(result) if batch_mode && !result.test_files.is_empty() => {
                    // Wave mode: accumulate result, commit at wave boundary
                    self.exec_step_finish(
                        step_id,
                        crate::execution_log::ItemStatus::Completed,
                        None,
                        Some("queued for wave commit"),
                    );
                    current_wave.push(result);
                }
                Some(result) => {
                    // Individual mode (parallel=1, commit=1): already committed inside generate_tests_for_file
                    files_boosted += 1;
                    consecutive_wave_failures = 0;
                    // Use the coverage already measured inside generate_tests_for_file when
                    // available — avoids re-running the full coverage command (and its embedded
                    // test suite) a second time immediately after it just ran.
                    let new_pct = if let Some(pct) = result.measured_overall_pct {
                        total_measurements += 1;
                        color_info!(
                            "Project-wide coverage after boost: {} (was {})",
                            cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                        );
                        current_pct = pct;
                        Some(pct)
                    } else {
                        match self.run_coverage_and_measure(&cov_cmd) {
                            Some(pct) => {
                                total_measurements += 1;
                                color_info!(
                                    "Project-wide coverage after boost: {} (was {})",
                                    cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                                );
                                current_pct = pct;
                                Some(pct)
                            }
                            None => {
                                warn!("Could not re-measure coverage — continuing with next file");
                                None
                            }
                        }
                    };
                    self.exec_step_finish(
                        step_id,
                        crate::execution_log::ItemStatus::Completed,
                        new_pct,
                        None,
                    );
                }
                None => {
                    // File was skipped (excluded, too large, no uncovered lines, etc.)
                    self.exec_step_finish(
                        step_id,
                        crate::execution_log::ItemStatus::Skipped,
                        None,
                        Some("no changes generated"),
                    );
                }
            }

            // Wave boundary: flush when wave is full or this is the last file
            let wave_ready = !current_wave.is_empty()
                && (current_wave.len() >= parallel_size || (is_last && !current_wave.is_empty()));

            if wave_ready {
                // Safety: ensure working tree is clean before wave commit
                // (stashes from the current wave are preserved — they'll be popped by commit_boost_batch)
                let _ = git::ensure_clean_state(&self.config.path);

                if commit_immediately {
                    // commit_size <= parallel_size: one real commit per wave
                    let committed = self.commit_boost_batch(&current_wave, test_framework, stash_prefix, skip_test_run, &framework_context_base)?;
                    files_boosted += committed;
                    current_wave.clear();
                    if committed > 0 {
                        consecutive_wave_failures = 0;
                        if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                            total_measurements += 1;
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
                            // Re-measure so overall_met early-exit works and progress is accurate
                            if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                                total_measurements += 1;
                                color_info!(
                                    "Project-wide coverage after wave commit: {} (was {})",
                                    cov_vs(pct, self.config.min_coverage), cov_prev(current_pct)
                                );
                                current_pct = pct;
                            }
                        } else {
                            // Per-file fallback created real commits — re-measure coverage
                            if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                                total_measurements += 1;
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
                        // US-036: Squashing is a git refactoring — the code state
                        // and therefore coverage are identical to what we measured
                        // after the wave commit(s). Skip the redundant re-measurement.
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
                total_measurements += 1;
                current_pct = pct;
            }
        }
        // US-036: final squash before cleanup passes — no need to re-measure,
        // squashing doesn't change code state. Any pending measurements happen
        // in the cleanup/retry passes below.
        if temp_commit_count > 0 {
            let _ = self.squash_boost_commits(temp_commit_count, &temp_commit_files);
        }

        // US-040: Track files that already consumed their coverage_rounds quota
        // in a force_individual pass, so subsequent passes skip them.  A file
        // entering both cleanup and retry would otherwise burn up to 2×rounds
        // worth of AI calls for zero benefit.
        let mut exhausted_files: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Cleanup pass: files still below min_file_coverage get a targeted individual-mode retry.
        //
        // In wave mode (coverage_wave_size > 1), the main loop does only ONE round of test
        // generation per file (coverage_rounds is skipped — no per-file test run between rounds).
        // Files whose wave tests failed and were reverted end up never getting their full
        // coverage_rounds. This cleanup pass processes those files one-by-one with full
        // coverage_rounds support (force_individual = true).
        if has_file_threshold {
            let still_below: Vec<_> = runner::find_lcov_report_with_hint(
                    &self.config.path, self.config.commands.coverage_report.as_deref())
                .map(|p| runner::per_file_lcov_coverage(&p))
                .unwrap_or_default()
                .into_iter()
                .filter(|fc| {
                    !is_test_file(&fc.file)
                        && fc.coverage_pct < self.config.min_file_coverage
                        && fc.total_lines > fc.covered_lines
                        && !self.config.coverage_exclude.iter().any(|pat| {
                            glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
                        })
                })
                .collect();

            if !still_below.is_empty() {
                info!(
                    "Cleanup pass: {} file(s) still below per-file threshold {:.0}% — retrying individually with up to {} round(s) each",
                    still_below.len(), self.config.min_file_coverage, self.config.coverage_rounds
                );
                let total_cleanup = still_below.len();
                for (i, fc) in still_below.iter().enumerate() {
                    // Every file entering the cleanup pass consumes its full
                    // coverage_rounds quota, regardless of whether tests were
                    // generated successfully — mark as exhausted.
                    exhausted_files.insert(fc.file.clone());
                    match self.generate_tests_for_file(
                        fc, test_framework, &test_examples_str,
                        stash_prefix, false, &framework_context_base, true,
                    )? {
                        Some(_) => {
                            files_boosted += 1;
                            // US-036: measure every N files (or on the last one)
                            // instead of after every file.
                            let is_last = i + 1 == total_cleanup;
                            let should_measure = is_last || (i + 1) % MEASURE_EVERY_N == 0;
                            if should_measure {
                                if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                                    total_measurements += 1;
                                    color_info!(
                                        "Project-wide coverage after cleanup commit: {} (was {}) [measured after {}/{} files]",
                                        cov_vs(pct, self.config.min_coverage),
                                        cov_prev(current_pct),
                                        i + 1,
                                        total_cleanup
                                    );
                                    current_pct = pct;
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
        }

        // Overall coverage retry pass: if overall coverage is still below min_coverage,
        // re-scan for files with room for improvement and retry them individually with
        // full coverage_rounds support.  The main wave loop only does one round per file,
        // so this pass compensates when the target hasn't been reached.
        if current_pct < self.config.min_coverage {
            let all_candidates: Vec<_> = runner::find_lcov_report_with_hint(
                    &self.config.path, self.config.commands.coverage_report.as_deref())
                .map(|p| runner::per_file_lcov_coverage(&p))
                .unwrap_or_default()
                .into_iter()
                .filter(|fc| {
                    !is_test_file(&fc.file)
                        && fc.coverage_pct < 100.0
                        && fc.total_lines > fc.covered_lines
                        && !self.config.coverage_exclude.iter().any(|pat| {
                            glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
                        })
                        && resolve_source_file(&self.config.path, &fc.file).exists()
                })
                .collect();

            // US-040: Exclude files already processed by the cleanup pass — they've
            // already used their coverage_rounds quota, re-processing them would burn
            // AI calls for no new benefit.
            let exhausted_count = all_candidates.iter()
                .filter(|fc| exhausted_files.contains(&fc.file))
                .count();
            let retry_candidates: Vec<_> = all_candidates.into_iter()
                .filter(|fc| !exhausted_files.contains(&fc.file))
                .collect();

            if exhausted_count > 0 {
                info!(
                    "Overall coverage retry: skipping {} file(s) already exhausted in cleanup pass",
                    exhausted_count
                );
            }

            if !retry_candidates.is_empty() {
                info!(
                    "Overall coverage retry: {:.1}% < {:.0}% target — retrying {} file(s) individually with up to {} round(s) each",
                    current_pct, self.config.min_coverage, retry_candidates.len(), self.config.coverage_rounds
                );
                let total_retry = retry_candidates.len();
                for (i, fc) in retry_candidates.iter().enumerate() {
                    if current_pct >= self.config.min_coverage {
                        break;
                    }
                    // Mark each file as exhausted before we touch it (idempotent).
                    exhausted_files.insert(fc.file.clone());
                    match self.generate_tests_for_file(
                        &fc, test_framework, &test_examples_str,
                        stash_prefix, false, &framework_context_base, true,
                    )? {
                        Some(_) => {
                            files_boosted += 1;
                            // US-036: measure every N files OR on the last one.
                            // Additionally, when we're close to the target, measure
                            // every file to detect early termination.
                            let is_last = i + 1 == total_retry;
                            let near_target = (self.config.min_coverage - current_pct) < 5.0;
                            let should_measure = is_last
                                || near_target
                                || (i + 1) % MEASURE_EVERY_N == 0;
                            if should_measure {
                                if let Some(pct) = self.run_coverage_and_measure(&cov_cmd) {
                                    total_measurements += 1;
                                    color_info!(
                                        "Project-wide coverage after overall retry: {} (was {}) [measured after {}/{} files]",
                                        cov_vs(pct, self.config.min_coverage),
                                        cov_prev(current_pct),
                                        i + 1,
                                        total_retry
                                    );
                                    current_pct = pct;
                                }
                            }
                        }
                        None => {}
                    }
                }
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
        // US-064: also report branch coverage when available
        if total_br > 0 {
            // Re-compute from the latest lcov so we reflect the boost effect
            let final_branch = runner::find_lcov_report_with_hint(
                &self.config.path, self.config.commands.coverage_report.as_deref(),
            )
                .map(|p| runner::per_file_lcov_coverage(&p))
                .map(|v| {
                    let (t, c) = v.iter().fold((0u64, 0u64), |(t, c), fc| {
                        (t + fc.total_branches, c + fc.covered_branches)
                    });
                    if t == 0 { 0.0 } else { (c as f64 / t as f64) * 100.0 }
                })
                .unwrap_or(overall_branch_pct);
            color_info!(
                "Branch coverage: {:.1}% → {:.1}% (target: {:.0}%)",
                overall_branch_pct, final_branch, self.config.min_branch_coverage
            );
        }
        // US-036: report total coverage measurements so the user can see how many
        // test-suite runs were triggered (primary cost driver for slow suites).
        info!(
            "Coverage measured {} times during boost (US-036: reduced from ~{} with pre-optimization)",
            total_measurements,
            // Rough estimate of pre-optimization count: 1 init + 1/wave + 1/file in cleanup + 1/file in retry
            total_measurements.saturating_add(files_processed as u32),
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
        // When true: ignore coverage_commit_batch and wave mode — commit each round immediately
        // and run tests after each round (enables full coverage_rounds support).
        // Used by the cleanup pass for files still below min_file_coverage after the main wave loop.
        force_individual: bool,
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

        let batch_mode = if force_individual {
            false
        } else {
            self.config.coverage_commit_batch > 1 || skip_test_run
        };
        let skip_test_run = if force_individual { false } else { skip_test_run };
        let max_rounds = self.config.coverage_rounds;
        let unlimited = max_rounds == 0;
        // Derive explicit test file path when test_dir is configured.
        let expected_test_path: Option<String> = self.config.test_generation.test_dir
            .as_deref()
            .map(|td| derive_test_file_path_with_segments(
                &fc.file, td,
                self.config.test_generation.test_source_root.as_deref(),
                self.config.test_generation.test_spec_segments,
            ));
        let mut round: u32 = 0;
        let mut any_success = false;
        let mut previous_coverage_pct = fc.coverage_pct;
        let mut current_uncovered_count = fc.total_lines.saturating_sub(fc.covered_lines);
        // Track specific uncovered line numbers across rounds for targeted prompts.
        let mut current_uncovered_lines: Vec<u32> = fc.uncovered_lines.clone();
        let mut last_test_output = String::new();
        let mut accumulated_test_files: Vec<String> = Vec::new();
        let mut accumulated_artifacts: Vec<String> = Vec::new();
        // Last overall project coverage measured during the round loop (individual mode).
        // Carried back to the caller so it can skip a redundant run_coverage_and_measure call.
        let mut last_overall_pct: Option<f64> = None;

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

            // Decide generation strategy for this round:
            //
            //  General (whole-file) — round 1 only, when the file is far below the
            //    threshold (gap > 20 pp) AND small enough to embed in full (≤ 600 source
            //    lines).  Sends the annotated source in one call: broad coverage, low cost.
            //
            //  Chunks — file has many uncovered lines (> 30).  Split into method-level
            //    batches so each AI call has focused context.
            //
            //  Single-prompt — ≤ 30 uncovered lines.  One focused call with just the
            //    uncovered snippets (fine-tune pass or already-close-to-threshold file).
            let source_line_count = source_content.lines().count();
            // General strategy: coverage is below half the threshold (e.g. < 40 % when target
            // is 80 %).  The whole annotated source is sent in one call — efficient even for
            // large files because one comprehensive call beats many small batched calls.
            // Fine-tune strategy: coverage is already ≥ half the threshold — only targeted
            // tests are needed to close the remaining gap.
            let use_general = round == 1
                && previous_coverage_pct < self.config.min_coverage * 0.5;
            let use_chunks = !use_general && current_uncovered_lines.len() > 30;
            let strategy = if use_general { "general" } else if use_chunks { "chunks" } else { "single-prompt" };
            info!(
                "Coverage boost {} for {} — {:.1}% coverage, {} uncovered lines, strategy: {}",
                round_label, fc.file, previous_coverage_pct, current_uncovered_count, strategy
            );

            let file_class = self.classify_source_file_cached(&fc.file);
            let pkg_hint = runner::derive_test_package(&fc.file)
                .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
                .unwrap_or_default();
            let per_file_ctx = if round == 1 {
                build_per_file_context(framework_context, &file_class, &pkg_hint)
            } else {
                // US-056: retries only need critical flags (avoid_spring_context, custom_instructions)
                // — the AI already knows the framework from round 1.
                let slim = build_slim_framework_context(&self.config.test_generation);
                build_per_file_context(&slim, &file_class, "")
            };

            if use_general {
                // --- General whole-file strategy: far below threshold, small file ---
                // Embed the entire source (annotated) in one prompt.  Establishes broad
                // baseline coverage efficiently; subsequent rounds fine-tune if needed.
                let annotated = claude::annotate_source_with_coverage(
                    &source_content,
                    &current_uncovered_lines,
                );
                let compliance_ctx = if self.config.compliance_enabled {
                    let c = claude::ComplianceTraceContext::new(
                        self.exec_log.run_id(),
                        format!("COVERAGE:{}", fc.file),
                    );
                    Some(if self.config.health_mode { c.with_risk_class("A") } else { c })
                } else { None };
                let prompt = claude::build_whole_file_coverage_prompt(
                    &fc.file,
                    previous_coverage_pct,
                    self.config.min_coverage,
                    &annotated,
                    test_framework,
                    test_examples_str,
                    &per_file_ctx,
                    compliance_ctx.as_ref(),
                    expected_test_path.as_deref(),
                );
                if self.config.show_prompts {
                    info!("Coverage boost prompt (general, {}):\n{}", round_label, prompt);
                }
                let test_tier = claude::classify_test_gen_tier(
                    current_uncovered_count as usize,
                    source_line_count,
                    &self.config.test_generation.tiers,
                );
                info!(
                    "Generating tests for {} [general whole-file] [{}] ({})...",
                    fc.file, test_tier, round_label
                );
                match self.run_ai_keyed("coverage_boost", &prompt, &test_tier, Some(fc.file.as_str())) {
                    Ok(_) => {
                        info!("AI completed general whole-file generation for {} ({})", fc.file, round_label);
                    }
                    Err(e) => {
                        warn!("Failed general whole-file generation for {} ({}): {} — stopping rounds", fc.file, round_label, e);
                        let _ = git::revert_changes(&self.config.path);
                        break;
                    }
                }
            } else if use_chunks {
                // --- Chunked strategy: batch small methods together, large methods solo ---
                let chunks = split_into_method_chunks(&source_content, &current_uncovered_lines, &fc.file);
                let batches = group_chunks_into_batches(chunks);
                let total_batches = batches.len();
                info!(
                    "Splitting {} uncovered lines into {} batch(es) for {} ({})",
                    current_uncovered_lines.len(), total_batches, fc.file, round_label
                );

                for (bi, batch) in batches.iter().enumerate() {
                    let batch_idx = bi + 1;

                    // US-053: compact each chunk's snippet to save tokens
                    let effective_snippets: Vec<(String, String)> = batch.iter()
                        .map(|c| (
                            c.label.clone(),
                            compact_method_snippet(&c.snippet, self.config.chunk_snippet_max_lines),
                        ))
                        .collect();

                    // Tier is based on the aggregate complexity of the whole batch
                    let batch_uncovered: usize = batch.iter().map(|c| c.uncovered_count).sum();
                    let batch_snippet_lines: usize = effective_snippets.iter().map(|(_, s)| s.lines().count()).sum();
                    let batch_tier = claude::classify_chunk_test_gen_tier(
                        batch_uncovered,
                        batch_snippet_lines,
                        &self.config.test_generation.tiers,
                    );

                    // US-067: boundary hints across all snippets in the batch
                    let all_snippets = effective_snippets.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n");
                    let boundary_hints = detect_boundary_hints(&all_snippets);

                    // US-066/US-069/US-073: compliance trace context
                    let compliance_ctx = if self.config.compliance_enabled {
                        let risk_class = compliance::resolve_effective_risk_class(
                            &fc.file, &self.config.compliance, self.config.health_mode,
                        );
                        let ctx = claude::ComplianceTraceContext::new(
                            self.exec_log.run_id(),
                            format!("COVERAGE:{}", fc.file),
                        );
                        Some(if self.config.health_mode {
                            ctx.with_risk_class(risk_class.as_str())
                        } else { ctx })
                    } else { None };

                    let chunk_refs: Vec<(&str, &str)> = effective_snippets.iter()
                        .map(|(l, s)| (l.as_str(), s.as_str()))
                        .collect();
                    let batch_label: String = batch.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ");
                    let prompt = claude::build_batched_chunk_test_prompt(
                        &fc.file,
                        &chunk_refs,
                        batch_idx,
                        total_batches,
                        test_framework,
                        &per_file_ctx,
                        &boundary_hints,
                        compliance_ctx.as_ref(),
                        expected_test_path.as_deref(),
                    );

                    if self.config.show_prompts {
                        info!("Batch {}/{} prompt ({}):\n{}", batch_idx, total_batches, round_label, prompt);
                    }

                    info!(
                        "  Batch {}/{}: {} — {} uncovered lines [{}]",
                        batch_idx, total_batches, batch_label, batch_uncovered, batch_tier
                    );
                    match self.run_ai_keyed("coverage_boost_chunk", &prompt, &batch_tier, Some(fc.file.as_str())) {
                        Ok(_) => {
                            info!("  Batch {}/{} completed", batch_idx, total_batches);
                            // Validate: no source files modified by this batch
                            let changed = git::changed_files(&self.config.path).unwrap_or_default();
                            let src_modified: Vec<&String> = changed.iter()
                                .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f))
                                .collect();
                            if !src_modified.is_empty() {
                                warn!("  Batch {}/{} modified source files: {:?} — reverting", batch_idx, total_batches, src_modified);
                                let _ = git::revert_changes(&self.config.path);
                                // Continue with next batch — don't abort the whole file
                            } else {
                                // Stage test files from this batch to preserve them across subsequent batches
                                let test_files: Vec<String> = changed.iter()
                                    .filter(|f| is_test_file(f) || is_generated_artifact(f))
                                    .cloned()
                                    .collect();
                                if !test_files.is_empty() {
                                    let refs: Vec<&str> = test_files.iter().map(|s| s.as_str()).collect();
                                    let _ = git::add_files(&self.config.path, &refs);
                                    // Revert non-test changes
                                    let _ = git::revert_changes(&self.config.path);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "  Batch {}/{} ({}) failed: {} — skipping batch",
                                batch_idx, total_batches, batch_label, e
                            );
                            let _ = git::revert_changes(&self.config.path);
                            // Continue with remaining batches
                        }
                    }
                }

                // Check if any test files were generated across all chunks
                let has_staged = git::has_staged_changes(&self.config.path).unwrap_or(false);
                let all_changed = git::changed_files(&self.config.path).unwrap_or_default();
                if all_changed.is_empty() && !has_staged {
                    warn!("No test files generated across all chunks for {} ({})", fc.file, round_label);
                    break;
                }
            } else {
                // --- Single-prompt strategy: ≤15 uncovered lines ---
                let covered_count = fc.total_lines.saturating_sub(current_uncovered_count as u64);
                // US-064: enrich the summary with branch coverage info when available
                let branch_info = if fc.total_branches > 0 {
                    let uncovered_branches = fc.total_branches.saturating_sub(fc.covered_branches);
                    let branch_lines_hint = if !fc.uncovered_branch_lines.is_empty() {
                        let head: Vec<String> = fc.uncovered_branch_lines.iter().take(10)
                            .map(|l| l.to_string()).collect();
                        format!(" (lines: {}{})",
                            head.join(", "),
                            if fc.uncovered_branch_lines.len() > 10 { ", ..." } else { "" },
                        )
                    } else {
                        String::new()
                    };
                    format!(
                        "\nBranch coverage: {:.1}% — {} of {} branches taken, {} uncovered{}. \
                         Target branches that are NOT taken to also cover those paths.",
                        fc.branch_coverage_pct,
                        fc.covered_branches,
                        fc.total_branches,
                        uncovered_branches,
                        branch_lines_hint,
                    )
                } else {
                    String::new()
                };
                let uncovered_summary = format!(
                    "{:.1}% coverage — {} of {} coverable lines hit, {} uncovered{}",
                    previous_coverage_pct, covered_count, fc.total_lines, current_uncovered_count, branch_info
                );
                let uncovered_snippets = extract_uncovered_snippets(
                    &source_content,
                    &current_uncovered_lines,
                    80,
                );
                // US-067: detect boundary/negative testing opportunities in the snippet
                let boundary_hints = detect_boundary_hints(&uncovered_snippets);
                // US-066/US-069/US-073: compliance trace context with risk class + requirements
                let (compliance_ctx, safety_section, reqs_section) = if self.config.compliance_enabled {
                    let risk_class = compliance::resolve_effective_risk_class(
                        &fc.file, &self.config.compliance, self.config.health_mode,
                    );
                    let ctx = claude::ComplianceTraceContext::new(
                        self.exec_log.run_id(),
                        format!("COVERAGE:{}", fc.file),
                    );
                    let ctx = if self.config.health_mode {
                        ctx.with_risk_class(risk_class.as_str())
                    } else { ctx };
                    let safety = compliance::build_safety_classification_section(risk_class, self.config.health_mode);
                    let reqs = compliance::requirements_for_file(&fc.file, &self.config.compliance);
                    let reqs_text = compliance::build_requirements_section(&reqs, self.config.health_mode);
                    (Some(ctx), safety, reqs_text)
                } else {
                    (None, String::new(), String::new())
                };

                // Combine extra context sections (safety + requirements) into the framework context
                let extended_per_file_ctx = if safety_section.is_empty() && reqs_section.is_empty() {
                    per_file_ctx.clone()
                } else {
                    format!("{}{}{}", per_file_ctx, safety_section, reqs_section)
                };

                let prompt = if round == 1 {
                    claude::build_test_generation_prompt(
                        &fc.file,
                        &uncovered_summary,
                        &uncovered_snippets,
                        test_framework,
                        test_examples_str,
                        &extended_per_file_ctx,
                        &boundary_hints,
                        compliance_ctx.as_ref(),
                        expected_test_path.as_deref(),
                    )
                } else {
                    claude::build_test_generation_retry_prompt(
                        &fc.file,
                        &uncovered_summary,
                        &uncovered_snippets,
                        test_framework,
                        round,
                        &truncate_tail(&last_test_output, 500),
                        &per_file_ctx,
                    )
                };

                if self.config.show_prompts {
                    info!("Coverage boost prompt ({}):\n{}", round_label, prompt);
                }

                let uncovered = current_uncovered_count as usize;
                let test_tier = claude::classify_test_gen_tier(uncovered, fc.total_lines as usize, &self.config.test_generation.tiers);
                info!("Generating tests for {} [{}] ({})...", fc.file, test_tier, round_label);
                match self.run_ai_keyed("coverage_boost", &prompt, &test_tier, Some(fc.file.as_str())) {
                    Ok(_) => {
                        info!("AI completed test generation for {} ({})", fc.file, round_label);
                    }
                    Err(e) => {
                        warn!("Failed to generate tests for {} ({}): {} — stopping rounds", fc.file, round_label, e);
                        let _ = git::revert_changes(&self.config.path);
                        break;
                    }
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

            // Run tests (skipped when skip_test_run=true — validation happens at wave commit time).
            //
            // US-059: when `tests_embedded_in_coverage=true`, the coverage command below
            // (later in the round for re-measurement) already runs the test suite.
            // We skip the separate `run_tests` call here and defer validation to the
            // coverage re-measurement, parsing its output for test failures.
            if !skip_test_run && !self.config.commands.tests_embedded_in_coverage {
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
            } else if self.config.commands.tests_embedded_in_coverage {
                info!("US-059: Skipping separate test run for {} ({}) — coverage command runs tests", fc.file, round_label);
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
                    // Capture overall project coverage so the caller can skip a redundant
                    // run_coverage_and_measure call after generate_tests_for_file returns.
                    last_overall_pct = runner::overall_lcov_coverage(lcov);
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
            // Only carry through if tests actually ran (individual mode).
            // In wave/batch mode skip_test_run=true so no measurement was taken here.
            measured_overall_pct: if skip_test_run { None } else { last_overall_pct },
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

            // US-059: when the coverage command runs tests internally, skip the
            // separate test invocation and validate by parsing the coverage output.
            // This halves the test-suite execution time for Maven/Gradle/pytest/etc.
            if self.config.commands.tests_embedded_in_coverage {
                let cov_cmd = self
                    .config
                    .coverage_command
                    .clone()
                    .or_else(|| self.config.commands.coverage.clone());
                if let Some(cmd) = cov_cmd {
                    info!(
                        "US-059: Running coverage command (runs tests + produces report) for wave of {} files",
                        batch.len()
                    );
                    match runner::run_shell_command(&self.config.path, &cmd, "coverage (embedded tests)") {
                        Ok((true, output)) => {
                            if let Some(reason) = runner::detect_test_failures_in_output(&output) {
                                warn!(
                                    "US-059: Coverage command exit=0 but detected failures ({}) — falling back to per-file:\n{}",
                                    reason,
                                    runner::extract_error_summary(&output, 800)
                                );
                                return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                            }
                            info!(
                                "US-059: Coverage command passed (no test failures detected) — proceeding with batch commit ({} files)",
                                batch.len()
                            );
                        }
                        Ok((false, output)) => {
                            warn!(
                                "US-059: Coverage command FAILED for batch of {} files — falling back to per-file:\n{}",
                                batch.len(),
                                runner::extract_error_summary(&output, 800)
                            );
                            return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                        }
                        Err(e) => {
                            warn!("US-059: Coverage command error during batch commit: {} — falling back to per-file", e);
                            return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                        }
                    }
                } else {
                    warn!(
                        "US-059: tests_embedded_in_coverage=true but no coverage command configured — falling back to run_tests"
                    );
                    // Fall through to run_tests below
                    match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                        Ok((true, _)) => {
                            info!("Wave tests pass — proceeding with batch commit ({} files)", batch.len());
                        }
                        Ok((false, output)) => {
                            warn!(
                                "Batch tests failed for {} files — falling back to per-file validation:\n{}",
                                batch.len(), runner::extract_error_summary(&output, 800)
                            );
                            return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                        }
                        Err(e) => {
                            warn!("Test execution error during batch commit: {} — falling back to per-file validation", e);
                            return Ok(self.fallback_per_file_commit(batch, test_framework, framework_context));
                        }
                    }
                }
            } else {
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

        let uncovered_summary = format!(
            "{:.0}% coverage — previous test generation attempt failed. \
             Fix the errors and regenerate working tests.",
            result.coverage_before
        );
        // Repair retry: the AI already generated tests that failed to compile.
        // No snippets needed — the error output tells it what to fix.
        let uncovered_snippets = String::new();
        // US-040: Include framework context in retry
        let file_class = self.classify_source_file_cached(&result.file);
        let per_file_ctx = build_per_file_context(framework_context, &file_class, "");
        let prompt = claude::build_test_generation_retry_prompt(
            &result.file,
            &uncovered_summary,
            &uncovered_snippets,
            test_framework,
            2, // retry attempt
            &truncate(error_output, 2000),
            &per_file_ctx,
        );
        let tier = claude::classify_repair_tier();

        if let Err(e) = self.run_ai_keyed("coverage_boost_repair", &prompt, &tier, Some(result.file.as_str())) {
            warn!("AI retry failed for {}: {} — discarding definitively", result.file, e);
            self.evict_session(result.file.as_str());
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
        // Delete the old coverage report so we never read stale data.
        // If the coverage command fails to produce a new report, find_lcov_report_with_hint
        // will return None — which is the correct behaviour.
        if let Some(old_report) = runner::find_lcov_report_with_hint(
            &self.config.path,
            self.config.commands.coverage_report.as_deref(),
        ) {
            let _ = std::fs::remove_file(&old_report);
        }

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

    /// Parallel coverage boost: process files concurrently in git worktrees.
    ///
    /// Creates a worktree pool, dispatches files in batches of `coverage_parallel`,
    /// copies results back to the main tree, validates with tests, and commits.
    async fn boost_coverage_parallel(
        &self,
        files: &[&runner::FileCoverage],
        test_framework: &str,
        test_examples_str: &str,
        framework_context: &str,
        initial_pct: f64,
        cov_cmd: &str,
    ) -> Result<()> {
        let parallelism = self.config.coverage_parallel as usize;
        info!(
            "=== Parallel coverage boost: {} files, parallelism={} ===",
            files.len(),
            parallelism
        );

        // Create worktree pool. Fall back to sequential on failure.
        let pool = match super::worktree_pool::WorktreePool::new(&self.config.path, parallelism) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn!(
                    "Failed to create worktree pool: {} — falling back to sequential mode",
                    e
                );
                // Recursive call with parallel=1 would be complex; just return an error
                // that the caller can handle. In practice, worktree creation failures are
                // rare (permission issues, old git version).
                anyhow::bail!("Worktree pool creation failed: {}", e);
            }
        };

        // Build the shared context for parallel tasks
        let ctx = Arc::new(ParallelGenContext {
            engine_routing: self.engine_routing.clone(),
            test_generation: self.config.test_generation.tiers.clone(),
            coverage_exclude: self.config.coverage_exclude.clone(),
            claude_timeout: self.config.claude_timeout,
            skip_permissions: self.config.dangerously_skip_permissions,
            show_prompts: self.config.show_prompts,
            test_framework: test_framework.to_string(),
            test_examples: test_examples_str.to_string(),
            framework_context: framework_context.to_string(),
            exec_log: self.exec_log.clone(),
            parent_phase_id: *self.current_phase_id.lock().unwrap(),
            chunk_snippet_max_lines: self.config.chunk_snippet_max_lines,
            compliance_enabled: self.config.compliance_enabled,
            health_mode: self.config.health_mode,
            min_coverage: self.config.min_coverage,
            test_dir: self.config.test_generation.test_dir.clone(),
            test_source_root: self.config.test_generation.test_source_root.clone(),
            test_spec_segments: self.config.test_generation.test_spec_segments,
            session_map: Arc::clone(&self.session_map),
        });

        let start_pct = initial_pct;
        let mut current_pct = initial_pct;
        let mut _files_boosted = 0usize;
        let mut consecutive_failures = 0usize;
        let max_failures = self.config.max_boost_failures;

        // Process in batches of `parallelism`
        for batch_start in (0..files.len()).step_by(parallelism) {
            if max_failures > 0 && consecutive_failures >= max_failures {
                warn!(
                    "Stopping parallel coverage boost: {} consecutive batch failures",
                    consecutive_failures
                );
                break;
            }

            // Check if overall threshold already met
            if current_pct >= self.config.min_coverage {
                info!(
                    "Overall coverage {:.1}% meets threshold {:.0}% — stopping",
                    current_pct, self.config.min_coverage
                );
                break;
            }

            let batch_end = (batch_start + parallelism).min(files.len());
            let batch = &files[batch_start..batch_end];
            info!(
                "--- Parallel batch [{}-{}] of {} files ---",
                batch_start + 1,
                batch_end,
                files.len()
            );

            // Spawn one blocking task per file in the batch.
            // Each task: generate tests → copy to main tree → clean worktree.
            let main_path = self.config.path.clone();
            let mut handles = Vec::with_capacity(batch.len());
            for &fc in batch {
                let pool = Arc::clone(&pool);
                let ctx = Arc::clone(&ctx);
                let fc = fc.clone();
                let main_path = main_path.clone();

                let handle = tokio::task::spawn_blocking(move || {
                    let wt_root = pool.acquire();
                    let wt_path = pool.project_dir(&wt_root);
                    let result = generate_tests_in_worktree(&fc, &wt_path, &ctx);

                    // Copy test files to main tree BEFORE cleaning the worktree
                    let copied = match &result {
                        Ok(Some(pr)) if !pr.test_files.is_empty() => {
                            match super::worktree_pool::copy_worktree_results(
                                &wt_path,
                                &main_path,
                                &pr.test_files,
                            ) {
                                Ok(c) => c,
                                Err(e) => {
                                    warn!("[wt] Failed to copy test files for {}: {}", fc.file, e);
                                    Vec::new()
                                }
                            }
                        }
                        _ => Vec::new(),
                    };

                    // Clean worktree for reuse (operate on worktree root, not subdir)
                    let _ = git::revert_changes(&wt_root);
                    let _ = git::ensure_clean_state(&wt_root);
                    pool.release(wt_root);

                    // Return the list of files successfully copied to main tree
                    (result, copied)
                });
                handles.push(handle);
            }

            // Await all tasks and collect results (per-file for fallback support)
            let mut per_file_results: Vec<(ParallelFileResult, Vec<String>)> = Vec::new();
            let mut all_copied_files: Vec<String> = Vec::new();
            let mut batch_had_results = false;

            for handle in handles {
                match handle.await {
                    Ok((Ok(Some(result)), copied)) => {
                        info!(
                            "Parallel: {} → {} test file(s) copied to main tree",
                            result.file,
                            copied.len()
                        );
                        all_copied_files.extend(copied.clone());
                        batch_had_results = true;
                        per_file_results.push((result, copied));
                    }
                    Ok((Ok(None), _)) => {
                        // File skipped
                    }
                    Ok((Err(e), _)) => {
                        warn!("Parallel: file generation failed: {}", e);
                    }
                    Err(e) => {
                        warn!("Parallel: task panicked: {}", e);
                    }
                }
            }

            if !batch_had_results || all_copied_files.is_empty() {
                consecutive_failures += 1;
                continue;
            }

            // Stage all copied test files in the main tree
            let stage_refs: Vec<&str> = all_copied_files.iter().map(|s| s.as_str()).collect();
            if let Err(e) = git::add_files(&self.config.path, &stage_refs) {
                warn!("Failed to stage parallel test files: {} — reverting", e);
                let _ = git::revert_changes(&self.config.path);
                consecutive_failures += 1;
                continue;
            }

            // Validate: run tests on the main tree
            info!(
                "Running tests to validate {} parallel-generated test file(s)...",
                all_copied_files.len()
            );
            let batch_passed = match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                Ok((true, _)) => {
                    info!("Tests pass after parallel batch — committing");
                    true
                }
                Ok((false, output)) => {
                    warn!(
                        "Tests FAIL after parallel batch — falling back to per-file validation:\n{}",
                        runner::extract_error_summary(&output, 500)
                    );
                    let _ = git::revert_changes(&self.config.path);
                    let _ = git::ensure_clean_state(&self.config.path);
                    false
                }
                Err(e) => {
                    warn!("Test execution error: {} — falling back to per-file validation", e);
                    let _ = git::revert_changes(&self.config.path);
                    let _ = git::ensure_clean_state(&self.config.path);
                    false
                }
            };

            if !batch_passed {
                // Per-file fallback: test each file individually, retry failures up to coverage_attempts times
                let max_attempts = self.config.coverage_attempts.max(1) as usize;
                let mut any_committed = false;

                for (result, copied) in &per_file_results {
                    if copied.is_empty() {
                        continue;
                    }

                    let mut committed = false;
                    for attempt in 1..=max_attempts {
                        // Stage this file's test files
                        let refs: Vec<&str> = copied.iter().map(|s| s.as_str()).collect();
                        if git::add_files(&self.config.path, &refs).is_err() {
                            warn!("Failed to stage test files for {} — skipping", result.file);
                            let _ = git::revert_changes(&self.config.path);
                            break;
                        }

                        match runner::run_tests(&self.config.path, test_framework, self.config.test_timeout) {
                            Ok((true, _)) => {
                                info!("Per-file tests pass for {} (attempt {}) — committing", result.file, attempt);
                                let _ = git::revert_changes(&self.config.path); // clean non-test leftovers
                                // Re-stage test files after revert
                                let _ = git::add_files(&self.config.path, &refs);
                                let msg = format_commit_message(
                                    &self.config, "test", "coverage",
                                    &format!("add tests for {} ({:.0}% → boost)", result.file, result.coverage_before),
                                    "", "", &result.file,
                                );
                                if git::commit(&self.config.path, &msg).is_ok() {
                                    committed = true;
                                    any_committed = true;
                                } else {
                                    warn!("Commit failed for {} — reverting", result.file);
                                    let _ = git::revert_changes(&self.config.path);
                                }
                                break;
                            }
                            Ok((false, output)) => {
                                warn!(
                                    "Per-file tests FAIL for {} (attempt {}/{}):\n{}",
                                    result.file, attempt, max_attempts,
                                    runner::extract_error_summary(&output, 500)
                                );
                                // Revert the failing tests
                                let _ = git::revert_changes(&self.config.path);
                                let _ = git::ensure_clean_state(&self.config.path);

                                if attempt < max_attempts {
                                    // Retry: regenerate tests with the error as context
                                    info!(
                                        "Retrying test generation for {} with error context (attempt {}/{})",
                                        result.file, attempt + 1, max_attempts
                                    );
                                    let file_class = self.classify_source_file_cached(&result.file);
                                    let per_file_ctx = build_per_file_context(&ctx.framework_context, &file_class, "");
                                    let prompt = claude::build_test_generation_retry_prompt(
                                        &result.file,
                                        &format!(
                                            "{:.0}% coverage — previous test generation attempt failed. \
                                             Fix the errors and regenerate working tests.",
                                            result.coverage_before
                                        ),
                                        "",
                                        test_framework,
                                        attempt as u32 + 1,
                                        &truncate_tail(&output, 1000),
                                        &per_file_ctx,
                                    );
                                    let tier = claude::classify_repair_tier();
                                    if let Err(e) = self.run_ai_keyed("coverage_boost_repair", &prompt, &tier, Some(result.file.as_str())) {
                                        warn!("AI retry failed for {}: {} — giving up", result.file, e);
                                        self.evict_session(result.file.as_str());
                                        let _ = git::revert_changes(&self.config.path);
                                        break;
                                    }
                                    // The AI wrote new test files — loop back to test them
                                    // Update copied to reflect the new files
                                    // (we use changed_files instead since the AI may have written different files)
                                    let changed = git::changed_files(&self.config.path).unwrap_or_default();
                                    let new_test_files: Vec<String> = changed.iter()
                                        .filter(|f| is_test_file(f))
                                        .cloned()
                                        .collect();
                                    if new_test_files.is_empty() {
                                        warn!("No test files generated during retry for {} — giving up", result.file);
                                        let _ = git::revert_changes(&self.config.path);
                                        break;
                                    }
                                    // Check no source files were modified
                                    let source_modified: Vec<&String> = changed.iter()
                                        .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f))
                                        .collect();
                                    if !source_modified.is_empty() {
                                        warn!(
                                            "Source files modified during retry for {}: {:?} — giving up",
                                            result.file, source_modified
                                        );
                                        let _ = git::revert_changes(&self.config.path);
                                        break;
                                    }
                                    // Continue to next attempt iteration (test the new files)
                                    continue;
                                }
                                // Exhausted all attempts
                            }
                            Err(e) => {
                                warn!("Test execution error for {}: {} — skipping", result.file, e);
                                let _ = git::revert_changes(&self.config.path);
                                let _ = git::ensure_clean_state(&self.config.path);
                                break;
                            }
                        }
                    }
                    if !committed {
                        warn!("Could not produce passing tests for {} after {} attempt(s) — discarding", result.file, max_attempts);
                        let _ = git::revert_changes(&self.config.path);
                        let _ = git::ensure_clean_state(&self.config.path);
                    }
                }

                if any_committed {
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                }
            } else {
                // Batch passed — commit all together
                let file_list: Vec<&str> = batch.iter()
                    .map(|&fc| fc.file.as_str())
                    .take(5)
                    .collect();
                let msg = format_commit_message(
                    &self.config,
                    "test",
                    "coverage",
                    &format!(
                        "add tests for {} file(s): {}{}",
                        batch.len(),
                        file_list.join(", "),
                        if batch.len() > 5 { " ..." } else { "" }
                    ),
                    "",
                    "",
                    "",
                );
                match git::commit(&self.config.path, &msg) {
                    Ok(()) => {
                        _files_boosted += batch.len();
                        consecutive_failures = 0;
                        info!("Committed parallel batch ({} files)", batch.len());
                    }
                    Err(e) => {
                        warn!("Commit failed: {} — reverting", e);
                        let _ = git::revert_changes(&self.config.path);
                        consecutive_failures += 1;
                        continue;
                    }
                }
            }

            // Re-measure coverage
            if let Some(pct) = self.run_coverage_and_measure(cov_cmd) {
                color_info!(
                    "Project-wide coverage after parallel batch: {} (was {})",
                    cov_vs(pct, self.config.min_coverage),
                    cov_prev(start_pct)
                );
                current_pct = pct;
            }
        }

        // Worker AI calls already write directly to the shared execution log
        // via `ctx.exec_log.log_ai_call(...)`, so no merge step is needed here.

        // pool is dropped here — worktrees cleaned up
        drop(pool);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Parallel coverage boost: standalone context + generation function
// ---------------------------------------------------------------------------

/// Read-only config snapshot for parallel test generation.
///
/// Extracted from `Orchestrator` so it can be moved into `spawn_blocking`
/// closures (`Send + Sync + 'static`).
#[derive(Clone)]
pub(crate) struct ParallelGenContext {
    pub engine_routing: crate::engine::EngineRoutingConfig,
    pub test_generation: crate::config::TestGenTiers,
    pub coverage_exclude: Vec<String>,
    pub claude_timeout: u64,
    pub skip_permissions: bool,
    pub show_prompts: bool,
    pub test_framework: String,
    pub test_examples: String,
    pub framework_context: String,
    /// Shared execution log — workers record AI calls directly here.
    pub exec_log: Arc<crate::execution_log::ExecutionLog>,
    /// Phase id of the enclosing `coverage_boost` phase, so worker AI calls
    /// are attributed to it in the SQLite database.
    pub parent_phase_id: Option<i64>,
    /// Max method body lines to embed in chunk snippet (0 = always full). US-053.
    pub chunk_snippet_max_lines: usize,
    /// US-066: compliance mode flag — activates trace block injection in prompts.
    pub compliance_enabled: bool,
    /// US-066/069: health mode flag — activates MDR/IEC 62304 risk class in trace blocks.
    pub health_mode: bool,
    /// Overall coverage threshold — used to decide general vs. fine-tune strategy.
    pub min_coverage: f64,
    /// Where to write test files (mirrors source structure under this dir).
    /// None = colocated next to source file (default).
    pub test_dir: Option<String>,
    /// Source prefix to strip before mirroring into `test_dir`.
    pub test_source_root: Option<String>,
    /// When set, truncate the filename stem to this many dot-segments when
    /// deriving the spec file name (consolidates sub-module files).
    pub test_spec_segments: Option<usize>,
    /// US-086: shared Claude session map (file_path → live session). Cloned
    /// from `Orchestrator::session_map` at dispatch time so worker-mode test
    /// generation can resume conversations across the chunked-batch loop.
    pub session_map: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, super::SessionEntry>,
        >,
    >,
}

/// Result of parallel test generation for a single file.
#[allow(dead_code)]
pub(crate) struct ParallelFileResult {
    pub file: String,
    pub test_files: Vec<String>,
    pub coverage_before: f64,
}

/// Generate tests for a single file inside an isolated worktree.
///
/// Derive the expected test file path for a source file given the configured
/// `test_dir` and optional `test_source_root`.
///
/// Rules:
/// - `test_dir` is the directory under which test files live (e.g. `"projects/lib/test/unit"`).
/// - `test_source_root` is the prefix to strip from the source file path before
///   mirroring it under `test_dir`.  When absent it is auto-detected as the longest
///   common directory prefix between `test_dir` and `source_file`.
/// - The source extension is replaced by `.spec.<ext>` (e.g. `.ts` → `.spec.ts`).
#[allow(dead_code)]
pub(crate) fn derive_test_file_path(
    source_file: &str,
    test_dir: &str,
    test_source_root: Option<&str>,
) -> String {
    derive_test_file_path_with_segments(source_file, test_dir, test_source_root, None)
}

/// Like `derive_test_file_path` but also accepts an optional segment limit.
/// When `test_spec_segments` is `Some(n)`, the filename stem is truncated to
/// its first `n` dot-separated segments before appending `.spec.<ext>`.
///
/// Example (`test_spec_segments = Some(2)`):
///   `calendar.component.datesRender.ts` → `calendar.component.spec.ts`
pub(crate) fn derive_test_file_path_with_segments(
    source_file: &str,
    test_dir: &str,
    test_source_root: Option<&str>,
    test_spec_segments: Option<usize>,
) -> String {
    // Determine the prefix to strip.
    let source_root: &str = if let Some(r) = test_source_root {
        r
    } else {
        // Auto-detect: find the longest common directory prefix between test_dir and source_file.
        // Walk component by component.
        let td: Vec<&str> = test_dir.split('/').collect();
        let sf: Vec<&str> = source_file.split('/').collect();
        let common_len = td.iter().zip(sf.iter()).take_while(|(a, b)| a == b).count();
        // Return a static-lifetime empty string or a prefix derived from sf.
        // We need to own the result — build it separately and leak it if necessary.
        // Instead, fall through with a known common prefix length.
        let common: String = sf[..common_len].join("/");
        // We cannot easily return a &str here from a local String, so we compute the
        // result in-place and return early.
        let stripped = if common.is_empty() {
            source_file.to_string()
        } else {
            source_file
                .strip_prefix(&format!("{}/", common))
                .unwrap_or(source_file)
                .to_string()
        };
        let test_file = append_spec_extension(&stripped, test_spec_segments);
        return format!("{}/{}", test_dir, test_file);
    };

    // Strip configured source root.
    let stripped = if source_root.is_empty() {
        source_file.to_string()
    } else {
        source_file
            .strip_prefix(&format!("{}/", source_root))
            .unwrap_or(source_file)
            .to_string()
    };
    let test_file = append_spec_extension(&stripped, test_spec_segments);
    format!("{}/{}", test_dir, test_file)
}

/// Replace the last file extension with `.spec.<ext>`.
/// `foo.component.ts` → `foo.component.spec.ts`
/// `FooService.java`  → `FooService.spec.java`
///
/// When `spec_segments` is `Some(n)`, the stem is truncated to its first `n`
/// dot-separated segments before the `.spec.` suffix is appended:
///   `calendar.component.datesRender.ts` with `n=2` → `calendar.component.spec.ts`
fn append_spec_extension(filename: &str, spec_segments: Option<usize>) -> String {
    let (stem, ext) = if let Some(dot_pos) = filename.rfind('.') {
        (&filename[..dot_pos], Some(&filename[dot_pos + 1..]))
    } else {
        (filename, None)
    };

    let effective_stem = if let Some(n) = spec_segments {
        let parts: Vec<&str> = stem.split('.').collect();
        if parts.len() > n {
            parts[..n].join(".")
        } else {
            stem.to_string()
        }
    } else {
        stem.to_string()
    };

    match ext {
        Some(e) => format!("{}.spec.{}", effective_stem, e),
        None => format!("{}.spec", effective_stem),
    }
}

/// This is the parallel equivalent of the AI-generation portion of
/// `generate_tests_for_file`.  It does NOT run tests, commit, stash,
/// or measure coverage — those happen after results are copied back
/// to the main tree.  Only one round of generation is performed
/// (retries are handled at the wave level).
pub(crate) fn generate_tests_in_worktree(
    fc: &runner::FileCoverage,
    wt_path: &std::path::Path,
    ctx: &ParallelGenContext,
) -> Result<Option<ParallelFileResult>> {
    // Skip excluded files
    if !ctx.coverage_exclude.is_empty() {
        if ctx.coverage_exclude.iter().any(|pat| {
            glob::Pattern::new(pat).map(|p| p.matches(&fc.file)).unwrap_or(false)
        }) {
            info!("[wt] Skipping excluded file: {}", fc.file);
            return Ok(None);
        }
    }

    if fc.total_lines <= fc.covered_lines {
        return Ok(None);
    }

    // Read source from the worktree
    let full_path = resolve_source_file(wt_path, &fc.file);
    let source_content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("[wt] Cannot read {} ({}): {} — skipping", fc.file, full_path.display(), e);
            return Ok(None);
        }
    };

    let uncovered_lines: Vec<u32> = fc.uncovered_lines.clone();
    let source_line_count = source_content.lines().count();
    // Derive explicit test file path when test_dir is configured.
    let expected_test_path: Option<String> = ctx.test_dir
        .as_deref()
        .map(|td| derive_test_file_path_with_segments(
            &fc.file, td,
            ctx.test_source_root.as_deref(),
            ctx.test_spec_segments,
        ));
    // General strategy: coverage is below half the threshold (e.g. < 40 % when target is 80 %).
    // Fine-tune strategy (chunks / single-prompt): coverage is already ≥ half the threshold.
    let use_general = fc.coverage_pct < ctx.min_coverage * 0.5;
    let use_chunks = !use_general && uncovered_lines.len() > 30;
    let strategy = if use_general { "general" } else if use_chunks { "chunks" } else { "single-prompt" };
    info!(
        "[wt] {} — {:.1}% coverage, {} uncovered lines, strategy: {}",
        fc.file, fc.coverage_pct, uncovered_lines.len(), strategy
    );

    let file_class = runner::classify_source_file(&fc.file, wt_path);
    let pkg_hint = runner::derive_test_package(&fc.file)
        .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
        .unwrap_or_default();
    let per_file_ctx = build_per_file_context(&ctx.framework_context, &file_class, &pkg_hint);

    // Helper closure: run AI via engine, record usage directly in the execution log.
    // US-086: this closure participates in the shared per-file Claude session
    // map. Each call passes the source `fc.file` as session_key; consecutive
    // calls within the same boost (chunks + repair) resume the conversation
    // and avoid re-loading the source + framework context for every batch.
    let run_ai = |step: &str, prompt: &str, tier: &claude::ClaudeTier| -> Result<String> {
        let mut invocation = crate::engine::resolve_engine_for_tier(tier, &ctx.engine_routing)?;

        // Look up live session for this source file.
        let session_key = fc.file.as_str();
        if matches!(invocation.engine_kind, crate::engine::EngineKind::Claude) {
            let entry = {
                let map = ctx.session_map.lock().expect("session_map poisoned");
                map.get(session_key).cloned()
            };
            if let Some(e) = entry {
                let age = e.opened_at.elapsed().as_secs();
                if e.turns < super::SESSION_MAX_TURNS && age < super::SESSION_MAX_AGE_SECS {
                    invocation.session_id = Some(e.id.clone());
                    tracing::info!(
                        "[wt] Reusing Claude session {} for {} (turn {}/{}, age {}s)",
                        e.id, session_key, e.turns + 1, super::SESSION_MAX_TURNS, age
                    );
                } else {
                    let mut map = ctx.session_map.lock().expect("session_map poisoned");
                    map.remove(session_key);
                    tracing::info!(
                        "[wt] Evicting Claude session {} for {} (turns={} age={}s)",
                        e.id, session_key, e.turns, age
                    );
                }
            }
        }

        let tier_timeout = tier.effective_timeout(ctx.claude_timeout);
        let prompt_floor = ((prompt.len() as u64) / 10 + 120)
            .min(ctx.claude_timeout.saturating_mul(3));
        let timeout = tier_timeout.max(prompt_floor);

        let model_label = invocation.model.clone().unwrap_or_else(|| "default".to_string());
        let engine_kind = invocation.engine_kind.clone();
        let effort_label = invocation.effort.clone();

        let call_started = std::time::Instant::now();
        let result = crate::engine::run_engine_full(
            wt_path,
            prompt,
            timeout,
            ctx.skip_permissions,
            ctx.show_prompts,
            &invocation,
        );
        let call_duration_ms = call_started.elapsed().as_millis() as u64;

        if let Ok(ref out) = result {
            // Persist returned session id for the next turn.
            if let Some(ref new_sid) = out.session_id {
                let mut map = ctx.session_map.lock().expect("session_map poisoned");
                let entry = map.entry(session_key.to_string())
                    .or_insert_with(|| super::SessionEntry {
                        id: new_sid.clone(),
                        turns: 0,
                        opened_at: std::time::Instant::now(),
                    });
                if entry.id != *new_sid {
                    entry.id = new_sid.clone();
                    entry.opened_at = std::time::Instant::now();
                    entry.turns = 0;
                }
                entry.turns = entry.turns.saturating_add(1);
            }

            let (usage, unknown) = match out.usage {
                Some(u) => (u, false),
                None => (crate::usage::TokenUsage::default(), true),
            };
            tracing::info!(
                "[wt] usage logged: step={} in={} out={} cache_read={} cache_create={} unknown={}",
                step, usage.input, usage.output, usage.cache_read, usage.cache_creation, unknown
            );
            let entry = crate::usage::UsageEntry {
                step: step.to_string(),
                engine: engine_kind,
                model: model_label,
                usage,
                unknown,
            };
            if let Err(e) = ctx.exec_log.log_ai_call(
                ctx.parent_phase_id,
                None,
                &entry,
                effort_label.as_deref(),
                Some(call_duration_ms),
            ) {
                tracing::warn!("execution_log: log_ai_call failed (parallel worker): {}", e);
            }
        }

        result.map(|o| o.stdout)
    };

    if use_general {
        // --- General whole-file strategy: far below threshold, embed full source ---
        let annotated = claude::annotate_source_with_coverage(&source_content, &uncovered_lines);
        let compliance_ctx = if ctx.compliance_enabled {
            let c = claude::ComplianceTraceContext::new(
                ctx.exec_log.run_id(),
                format!("COVERAGE:{}", fc.file),
            );
            Some(if ctx.health_mode { c.with_risk_class("A") } else { c })
        } else { None };
        let prompt = claude::build_whole_file_coverage_prompt(
            &fc.file,
            fc.coverage_pct,
            ctx.min_coverage,
            &annotated,
            &ctx.test_framework,
            &ctx.test_examples,
            &per_file_ctx,
            compliance_ctx.as_ref(),
            expected_test_path.as_deref(),
        );
        let test_tier = claude::classify_test_gen_tier(
            uncovered_lines.len(),
            source_line_count,
            &ctx.test_generation,
        );
        info!(
            "[wt] {} → general whole-file, {} uncovered [{}] ({:.1}% < {:.0}% threshold midpoint)",
            fc.file, uncovered_lines.len(), test_tier, fc.coverage_pct, ctx.min_coverage * 0.5
        );
        match run_ai("coverage_boost", &prompt, &test_tier) {
            Ok(_) => info!("[wt] General whole-file AI completed for {}", fc.file),
            Err(e) => {
                warn!("[wt] General whole-file AI failed for {}: {}", fc.file, e);
                let _ = git::revert_changes(wt_path);
                return Ok(None);
            }
        }
    } else if use_chunks {
        // --- Chunked strategy: batch small methods together, large methods solo ---
        let chunks = split_into_method_chunks(&source_content, &uncovered_lines, &fc.file);
        let batches = group_chunks_into_batches(chunks);
        let total_batches = batches.len();
        info!(
            "[wt] {} → {} batch(es), {} uncovered lines",
            fc.file, total_batches, uncovered_lines.len()
        );

        for (bi, batch) in batches.iter().enumerate() {
            let batch_idx = bi + 1;

            // US-053: compact each chunk's snippet to save tokens
            let effective_snippets: Vec<(String, String)> = batch.iter()
                .map(|c| (
                    c.label.clone(),
                    compact_method_snippet(&c.snippet, ctx.chunk_snippet_max_lines),
                ))
                .collect();

            // Tier based on aggregate complexity of the whole batch
            let batch_uncovered: usize = batch.iter().map(|c| c.uncovered_count).sum();
            let batch_snippet_lines: usize = effective_snippets.iter().map(|(_, s)| s.lines().count()).sum();
            let batch_tier = claude::classify_chunk_test_gen_tier(
                batch_uncovered,
                batch_snippet_lines,
                &ctx.test_generation,
            );

            // US-067: boundary hints across all snippets in the batch
            let all_snippets = effective_snippets.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n");
            let boundary_hints = detect_boundary_hints(&all_snippets);

            // US-066: compliance trace context (from ParallelGenContext)
            let compliance_ctx = if ctx.compliance_enabled {
                let c = claude::ComplianceTraceContext::new(
                    ctx.exec_log.run_id(),
                    format!("COVERAGE:{}", fc.file),
                );
                Some(if ctx.health_mode { c.with_risk_class("A") } else { c })
            } else { None };

            let chunk_refs: Vec<(&str, &str)> = effective_snippets.iter()
                .map(|(l, s)| (l.as_str(), s.as_str()))
                .collect();
            let batch_label: String = batch.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ");
            let prompt = claude::build_batched_chunk_test_prompt(
                &fc.file,
                &chunk_refs,
                batch_idx,
                total_batches,
                &ctx.test_framework,
                &per_file_ctx,
                &boundary_hints,
                compliance_ctx.as_ref(),
                expected_test_path.as_deref(),
            );

            info!(
                "[wt]   Batch {}/{}: {} — {} uncovered [{}]",
                batch_idx, total_batches, batch_label, batch_uncovered, batch_tier
            );
            match run_ai("coverage_boost_chunk", &prompt, &batch_tier) {
                Ok(_) => {
                    // Validate: no source files modified
                    let changed = git::changed_files(wt_path).unwrap_or_default();
                    let src_modified: Vec<&String> = changed.iter()
                        .filter(|f| !is_test_file(f) && !is_generated_artifact(f) && !is_internal_file(f))
                        .collect();
                    if !src_modified.is_empty() {
                        warn!("[wt]   Batch {}/{} modified source: {:?} — reverting", batch_idx, total_batches, src_modified);
                        let _ = git::revert_changes(wt_path);
                    } else {
                        let test_files: Vec<String> = changed.iter()
                            .filter(|f| is_test_file(f) || is_generated_artifact(f))
                            .cloned()
                            .collect();
                        if !test_files.is_empty() {
                            let refs: Vec<&str> = test_files.iter().map(|s| s.as_str()).collect();
                            let _ = git::add_files(wt_path, &refs);
                            let _ = git::revert_changes(wt_path);
                        }
                    }
                }
                Err(e) => {
                    warn!("[wt]   Batch {}/{} ({}) failed: {} — skipping", batch_idx, total_batches, batch_label, e);
                    let _ = git::revert_changes(wt_path);
                }
            }
        }
    } else {
        // --- Single-prompt strategy ---
        let uncovered_count = uncovered_lines.len();
        let covered_count = fc.total_lines.saturating_sub(fc.total_lines - fc.covered_lines);
        let uncovered_summary = format!(
            "{:.1}% coverage — {} of {} coverable lines hit, {} uncovered",
            fc.coverage_pct, covered_count, fc.total_lines, uncovered_count
        );
        let uncovered_snippets = extract_uncovered_snippets(&source_content, &uncovered_lines, 80);
        // US-067: boundary/negative hints for worktree single-prompt path
        let boundary_hints = detect_boundary_hints(&uncovered_snippets);
        // US-066: compliance trace context
        let compliance_ctx = if ctx.compliance_enabled {
            let c = claude::ComplianceTraceContext::new(
                ctx.exec_log.run_id(),
                format!("COVERAGE:{}", fc.file),
            );
            Some(if ctx.health_mode { c.with_risk_class("A") } else { c })
        } else { None };

        let prompt = claude::build_test_generation_prompt(
            &fc.file,
            &uncovered_summary,
            &uncovered_snippets,
            &ctx.test_framework,
            &ctx.test_examples,
            &per_file_ctx,
            &boundary_hints,
            compliance_ctx.as_ref(),
            expected_test_path.as_deref(),
        );

        let test_tier = claude::classify_test_gen_tier(
            uncovered_count,
            fc.total_lines as usize,
            &ctx.test_generation,
        );

        info!("[wt] {} → single prompt, {} uncovered [{}]", fc.file, uncovered_count, test_tier);
        match run_ai("coverage_boost", &prompt, &test_tier) {
            Ok(_) => {
                info!("[wt] AI completed for {}", fc.file);
            }
            Err(e) => {
                warn!("[wt] AI failed for {}: {}", fc.file, e);
                let _ = git::revert_changes(wt_path);
                return Ok(None);
            }
        }
    }

    // Collect test files produced in the worktree
    // (staged + unstaged — we want everything the AI wrote)
    let changed = git::changed_files(wt_path).unwrap_or_default();
    let test_files: Vec<String> = changed.into_iter()
        .filter(|f| is_test_file(f) || is_generated_artifact(f))
        .collect();

    // Also check staged files
    let staged = git::has_staged_changes(wt_path).unwrap_or(false);

    if test_files.is_empty() && !staged {
        info!("[wt] No test files produced for {}", fc.file);
        return Ok(None);
    }

    // Stage everything so `changed_files` picks it up for the copy step
    if !test_files.is_empty() {
        let refs: Vec<&str> = test_files.iter().map(|s| s.as_str()).collect();
        let _ = git::add_files(wt_path, &refs);
    }

    info!("[wt] {} → {} test file(s) generated", fc.file, test_files.len());

    Ok(Some(ParallelFileResult {
        file: fc.file.clone(),
        test_files,
        coverage_before: fc.coverage_pct,
    }))
}

/// Freeze the current lcov/coverage report as the baseline used by every
/// per-issue coverage check during the fix loop.
///
/// Rationale: the fix loop runs with parallel worktrees, and each worker
/// may regenerate its own lcov during per-fix validation. If `check_coverage`
/// reads from the worktree's lcov, a fix that happens to land in a file
/// still being tested elsewhere gets a moving-target answer. Freezing the
/// lcov once — right after the last "complete" test run (preflight + coverage
/// boost) — gives every worker the same, immutable reference.
///
/// Returns the absolute path to the copied file, or `None` when no coverage
/// report can be located (e.g. the project hasn't generated one yet and has
/// no coverage command). The returned path lives in the MAIN repo's
/// `.reparo/baseline.lcov` — never inside a worktree, so concurrent workers
/// can all read it without contention.
pub fn snapshot_baseline_lcov(
    project_path: &std::path::Path,
    coverage_report_hint: Option<&str>,
) -> Option<std::path::PathBuf> {
    let source = runner::find_lcov_report_quietly(project_path, coverage_report_hint)?;

    let reparo_dir = project_path.join(".reparo");
    if let Err(e) = std::fs::create_dir_all(&reparo_dir) {
        warn!(
            "Could not create .reparo directory for baseline snapshot: {} — skipping snapshot",
            e
        );
        return None;
    }

    // Preserve the original extension so downstream parsers that switch on
    // `.xml` vs `.info` still work.
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("info");
    let dest = reparo_dir.join(format!("baseline.{}", ext));

    match std::fs::copy(&source, &dest) {
        Ok(_) => {
            info!(
                "Frozen baseline coverage report: {} → {}",
                source.display(),
                dest.display()
            );
            ensure_gitignore_contains(project_path, ".reparo/");
            Some(dest)
        }
        Err(e) => {
            warn!(
                "Could not copy {} to baseline snapshot: {} — per-issue coverage will fall back to SonarQube",
                source.display(),
                e
            );
            None
        }
    }
}

/// Append `entry` to `.gitignore` if the file exists and does not already
/// contain it. No-op on any error — `.reparo/` contents are harmless if
/// accidentally committed, and a misread .gitignore shouldn't fail the run.
fn ensure_gitignore_contains(project_path: &std::path::Path, entry: &str) {
    let gitignore = project_path.join(".gitignore");
    let existing = match std::fs::read_to_string(&gitignore) {
        Ok(s) => s,
        Err(_) => return,
    };
    let target = entry.trim_end_matches('\n');
    let already = existing
        .lines()
        .any(|l| l.trim() == target || l.trim() == entry);
    if already {
        return;
    }
    let mut new_content = existing;
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(target);
    new_content.push('\n');
    if let Err(e) = std::fs::write(&gitignore, new_content) {
        warn!(".gitignore update skipped: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_test_file_path_with_explicit_source_root() {
        let result = derive_test_file_path(
            "projects/example/angular-base/tables/basetable/basetable.component.ts",
            "projects/example/angular-base/test/unit",
            Some("projects/example/angular-base"),
        );
        assert_eq!(
            result,
            "projects/example/angular-base/test/unit/tables/basetable/basetable.component.spec.ts"
        );
    }

    #[test]
    fn test_derive_test_file_path_auto_detect_source_root() {
        // No explicit source root — auto-detect from common prefix.
        let result = derive_test_file_path(
            "projects/example/angular-base/common/services/locale-imports.ts",
            "projects/example/angular-base/test/unit",
            None,
        );
        assert_eq!(
            result,
            "projects/example/angular-base/test/unit/common/services/locale-imports.spec.ts"
        );
    }

    #[test]
    fn test_derive_test_file_path_java() {
        let result = derive_test_file_path(
            "src/main/java/com/example/FooService.java",
            "src/test/java/com/example",
            Some("src/main/java/com/example"),
        );
        assert_eq!(result, "src/test/java/com/example/FooService.spec.java");
    }

    #[test]
    fn test_append_spec_extension() {
        assert_eq!(append_spec_extension("foo.component.ts", None), "foo.component.spec.ts");
        assert_eq!(append_spec_extension("bar.service.js", None), "bar.service.spec.js");
        assert_eq!(append_spec_extension("no_extension", None), "no_extension.spec");
    }

    #[test]
    fn test_append_spec_extension_with_segments() {
        // Consolidate sub-module files into parent component spec
        assert_eq!(
            append_spec_extension("calendar.component.datesRender.ts", Some(2)),
            "calendar.component.spec.ts"
        );
        assert_eq!(
            append_spec_extension("calendar.component.eventDrop.ts", Some(2)),
            "calendar.component.spec.ts"
        );
        // When stem already has ≤ N segments, no truncation
        assert_eq!(
            append_spec_extension("foo.component.ts", Some(2)),
            "foo.component.spec.ts"
        );
        // Single segment file
        assert_eq!(
            append_spec_extension("foo.ts", Some(2)),
            "foo.spec.ts"
        );
    }

    #[test]
    fn test_derive_test_file_path_with_spec_segments() {
        // Sub-module Angular-style files consolidated into parent spec
        let result = derive_test_file_path_with_segments(
            "src/app/calendar.component.datesRender.ts",
            "src/app",
            Some("src/app"),
            Some(2),
        );
        assert_eq!(result, "src/app/calendar.component.spec.ts");

        let result2 = derive_test_file_path_with_segments(
            "src/app/calendar.component.eventDrop.ts",
            "src/app",
            Some("src/app"),
            Some(2),
        );
        assert_eq!(result2, "src/app/calendar.component.spec.ts");
    }

    #[test]
    fn baseline_snapshot_copies_lcov_into_reparo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();

        // Produce a minimal lcov file at a conventional location so
        // find_lcov_report_quietly locates it.
        let cov_dir = project.join("coverage");
        std::fs::create_dir_all(&cov_dir).unwrap();
        let lcov = cov_dir.join("lcov.info");
        std::fs::write(&lcov, "TN:\nSF:src/a.rs\nend_of_record\n").unwrap();

        let out = super::snapshot_baseline_lcov(project, None);
        let out = out.expect("snapshot should succeed when lcov exists");
        assert!(out.starts_with(project.join(".reparo")));
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "TN:\nSF:src/a.rs\nend_of_record\n"
        );
    }

    #[test]
    fn baseline_snapshot_returns_none_without_report() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(super::snapshot_baseline_lcov(tmp.path(), None).is_none());
    }

    #[test]
    fn ensure_gitignore_adds_entry_once() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = tmp.path().join(".gitignore");
        std::fs::write(&gi, "target/\n").unwrap();

        super::ensure_gitignore_contains(tmp.path(), ".reparo/");
        super::ensure_gitignore_contains(tmp.path(), ".reparo/"); // idempotent

        let content = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(content.matches(".reparo/").count(), 1);
        assert!(content.contains("target/"));
    }
}
