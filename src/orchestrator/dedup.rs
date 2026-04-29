use super::helpers::*;
use super::Orchestrator;
use crate::claude;
use crate::config::ScannerKind;
use crate::git;
use crate::runner;
use crate::sonar;
use anyhow::Result;
use tracing::{info, warn};

impl Orchestrator {
    /// Reduce code duplication in project files by:
    /// 1. Ensuring 100% coverage of duplicated ranges
    /// 2. Asking Claude to refactor and eliminate duplication
    /// 3. Formatting if configured
    /// 4. Verifying build passes
    /// 5. Commit if verified, revert if not
    pub(super) async fn reduce_duplications(
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
                    effort: None,
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
            // US-087: dedup operates on a single file — key the session by
            // file_path so previous fix/test/doc context on it is reused.
            match self.run_ai_keyed("dedup", &prompt, &dedup_tier, Some(dup_file.file_path.as_str())) {
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
}
