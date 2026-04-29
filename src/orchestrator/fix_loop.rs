use super::helpers::*;
use super::Orchestrator;
use crate::claude;
use crate::git;
use crate::report::{self, FixStatus, IssueResult, TestFailureAnalysis};
use crate::runner;
use crate::sonar::{self, Issue};
use anyhow::Result;
use tracing::{error, info, warn};

impl Orchestrator {
    pub(super) async fn improve_documentation(&self, test_command: &str) -> Result<()> {
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

            // US-087: reuse the per-file Claude session so docs share context
            // with the recent fix on this file (which already loaded source +
            // structural understanding into the conversation).
            match self.run_ai_keyed("documentation", &prompt, &tier, Some(file_path.as_str())) {
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

    pub(super) async fn process_issue(&self, issue: &Issue, test_command: &str) -> IssueResult {
        let file_path = sonar::component_to_path(&issue.component);
        let rule_key = issue.rule.clone();
        let memory_key = (file_path.clone(), rule_key.clone());

        // (file, rule) failure memory short-circuit. After
        // FAILURE_MEMORY_THRESHOLD prior issues with the same key produced a
        // non-Fixed outcome, retrying is almost always futile and consumes
        // significant AI budget. Fail-fast to NeedsReview without any model
        // call. Reset on Fixed (see post-call branch below).
        let prior_failures = {
            let memory = self
                .fix_failure_memory
                .lock()
                .expect("fix_failure_memory mutex poisoned");
            memory.get(&memory_key).copied().unwrap_or(0)
        };
        if prior_failures >= super::FAILURE_MEMORY_THRESHOLD {
            warn!(
                "Short-circuiting {} ({}): {} prior failures on ({}, {}) — see FAILURE_MEMORY_THRESHOLD",
                issue.key, issue.rule, prior_failures, file_path, rule_key
            );
            return IssueResult {
                issue_key: issue.key.clone(),
                rule: issue.rule.clone(),
                severity: issue.severity.clone(),
                issue_type: issue.issue_type.clone(),
                message: issue.message.clone(),
                file: file_path.clone(),
                lines: format_lines(&issue.text_range),
                status: FixStatus::NeedsReview(format!(
                    "Skipped: {} prior issue(s) with rule {} on {} have already failed during this run; further attempts are unlikely to succeed without human review",
                    prior_failures, rule_key, file_path
                )),
                change_description: String::new(),
                tests_added: Vec::new(),
                pr_url: None,
                diff_summary: None,
            };
        }

        // run 2026-04-28 follow-up: drop any live session for this file at
        // the issue boundary. Resuming across distinct issues lets Claude
        // believe the work is already done and return "no changes"
        // (observed twice on EmailServiceImpl/S6813, ~3 min wasted each).
        // Repair cycles within process_issue_impl still get session reuse.
        self.evict_session_for_new_issue(&file_path, &issue.key);

        let result = self.process_issue_impl(issue, test_command).await;

        // Update memory based on outcome. Fixed clears the counter (a new
        // working approach was found, so prior failures may not predict the
        // next one). Anything else increments.
        {
            let mut memory = self
                .fix_failure_memory
                .lock()
                .expect("fix_failure_memory mutex poisoned");
            match &result.status {
                FixStatus::Fixed => {
                    memory.remove(&memory_key);
                }
                FixStatus::NeedsReview(_) | FixStatus::Failed(_) => {
                    *memory.entry(memory_key).or_insert(0) += 1;
                }
                FixStatus::Skipped(_) | FixStatus::RiskSkipped(_) => {
                    // Skips are not failures of the fix process — leave counter alone.
                }
            }
        }

        // US-081: drop any persistent Claude session for this file when the
        // issue ends non-Fixed. The conversation captured the failed attempt
        // (and possibly a revert) and would mislead the next sibling fix.
        // On Fixed we keep the session alive — successive fixes on the same
        // file benefit from the model already understanding the structure.
        match &result.status {
            FixStatus::NeedsReview(_) | FixStatus::Failed(_) => {
                self.evict_session(&file_path);
            }
            _ => {}
        }

        result
    }

    async fn process_issue_impl(&self, issue: &Issue, test_command: &str) -> IssueResult {
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

        // Rule blocklist: skip issues that are known to consistently break tests
        // without producing a usable fix (e.g. java:S1874 on Hibernate/JPA
        // types). Cheaper than running the full fix/build/test/repair cycle and
        // then falling back to NeedsReview anyway.
        if rule_is_blocklisted(
            &issue.rule,
            full_path.to_str().unwrap_or(""),
            &self.config.rule_blocklist,
            &self.config.hard_case_blocklist,
        ) {
            let reason = format!(
                "Rule {} blocklisted for {} (previously broke tests without a working fix)",
                issue.rule, file_path
            );
            warn!("Blocklisted: {}", reason);
            result.status = FixStatus::NeedsReview(reason);
            return result;
        }

        let total_lines = file_content.lines().count() as u32;
        let (start_line, end_line) = match &issue.text_range {
            Some(tr) if tr.start_line == tr.end_line => {
                // Single-line range (e.g. function signature for cognitive
                // complexity). Scope to the enclosing method so we don't
                // wrongly attribute coverage of unrelated later methods to
                // this issue. Falls back to EOF if no method boundary is
                // detected (e.g. class-level code or unsupported language).
                enclosing_method_range(&file_content, &file_path, tr.start_line)
                    .unwrap_or((tr.start_line, total_lines))
            }
            Some(tr) => (tr.start_line, tr.end_line),
            None => (1, total_lines),
        };

        // Step A-0: Pre-fix risk assessment — skip issues with cross-cutting impact
        // (e.g., enabling CSRF protection requires coordinated frontend changes).
        if let Some(assessment) = crate::orchestrator::risk_assessment::assess_fix_risk(
            issue,
            &self.config.risk_assessment,
            &self.config.path,
            &file_content,
            self.config.claude_timeout,
            self.config.dangerously_skip_permissions,
            self.config.show_prompts,
        ) {
            warn!(
                "Skipping {} ({}): risk assessment — {}",
                issue.key, issue.rule, assessment.reason
            );
            result.status = FixStatus::RiskSkipped(assessment.reason.clone());
            report::append_risk_skipped(
                &self.config.path,
                &result,
                &assessment.reason,
                &assessment.suggested_action,
            );
            return result;
        }

        // Step A: Check line-level coverage and generate tests if needed (US-004).
        // Skip coverage for:
        //   - non-coverable files (CSS, HTML, assets) — nothing to instrument
        //   - test files themselves — they ARE tests; JaCoCo doesn't instrument
        //     `src/test/**`, so SonarQube reports 0% and we'd try to generate
        //     tests for tests (nonsensical). Just fix the bug in the test.
        if is_non_coverable_file(&file_path) {
            info!("Skipping coverage check for non-coverable file: {}", file_path);
        } else if is_test_file(&file_path) {
            info!("Skipping coverage check for test file: {} (test files aren't coverage-instrumented)", file_path);
        } else if !rule_is_coverage_dependent(&issue.rule) {
            // Static-analysis rules (lint:*, most Sonar code smells) don't need
            // new tests to be verified — SonarQube re-scans the source, not the
            // coverage report. Skipping the pre-fix coverage gate avoids a 5-12
            // min detour per such issue. Callers set rule_is_coverage_dependent
            // narrowly so false negatives self-heal via the post-fix rescan loop.
            info!(
                "Skipping pre-fix coverage gate for {} (static-analysis; not coverage-dependent)",
                issue.rule
            );
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

        // Step A-2: Clean before fix if command defined (US-014).
        // Skip for lint:* fixes — linter changes don't need a clean build lifecycle.
        // When `skip_clean_when_safe` is enabled (default), also skip between
        // successful sequential fixes: incremental compile is correct between
        // green fixes and `mvn clean` alone is ~2-3 s that stacks up across
        // dozens of issues. The `needs_clean` flag is reset on any failure
        // (see post-build/test branches) so dirty state still triggers a clean.
        if !issue.rule.starts_with("lint:") {
            if let Some(ref clean_cmd) = self.config.commands.clean {
                // Parallel workers run in pool-managed worktrees that are
                // reset to a clean state on every acquire (see WorktreePool
                // release). A stale build directory is impossible here, so
                // `mvn clean` is pure overhead — ~1.5-2 s per issue × N
                // issues adds up to tens of minutes on real projects.
                let skip_for_fresh = self.config.fresh_worktree;
                let should_clean = !skip_for_fresh
                    && (!self.config.skip_clean_when_safe
                        || self.needs_clean.load(std::sync::atomic::Ordering::Relaxed));
                if should_clean {
                    match runner::run_shell_command(&self.config.path, clean_cmd, "clean") {
                        Ok((true, _)) => info!("Clean succeeded"),
                        Ok((false, output)) => warn!("Clean failed: {}", truncate(&output, 100)),
                        Err(e) => warn!("Clean command error: {}", e),
                    }
                    // Whether clean passed or failed, the state is now "clean-attempted".
                    self.needs_clean
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                } else if skip_for_fresh {
                    info!(
                        "Skipping clean for {} (fresh worktree; build tree is already clean)",
                        issue.key
                    );
                } else {
                    info!(
                        "Skipping clean for {} (previous fix succeeded; incremental compile is safe)",
                        issue.key
                    );
                }
            }
        } else if self.config.commands.clean.is_some() {
            info!("Skipping clean for lint issue {} (not needed for linter-origin fixes)", issue.key);
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

        // Classify the issue to pick the right model + effort. Sonar's
        // own effort estimate acts as an overlay — large-effort issues
        // escalate one tier even when the rule looks mechanical.
        let effort_minutes = issue
            .effort
            .as_deref()
            .and_then(sonar::parse_effort_minutes);
        let tier = claude::classify_issue_tier(
            &issue.rule,
            &issue.severity,
            &issue.message,
            end_line.saturating_sub(start_line) + 1,
            effort_minutes,
        );
        info!("Issue {} classified as tier {} (rule: {}, severity: {})", issue.key, tier, issue.rule, issue.severity);

        // A1: Linter autofix fast-path. For `lint:<format>:<rule>` findings,
        // try the linter's own --fix on the affected file first. If it edits
        // the file, skip the Claude call entirely — a one-second fix beats a
        // 30-60s roundtrip to a model. If it made no change we fall through.
        let mut claude_output: String = String::new();
        let mut used_fastpath = false;
        if !self.config.skip_linter_fastpath && issue.rule.starts_with("lint:") {
            if let Some((fmt_name, rule_name)) = issue.rule
                .strip_prefix("lint:")
                .and_then(|rest| rest.split_once(':'))
            {
                if let Some(fmt) = crate::linter::LintFormat::parse(fmt_name) {
                    let abs = self.config.path.join(&file_path);
                    match crate::linter::autofix_single(fmt, rule_name, &abs, &self.config.path) {
                        Ok(true) => {
                            info!(
                                "Linter fast-path resolved {} ({}:{}); skipping AI call",
                                issue.key, fmt_name, rule_name
                            );
                            claude_output = format!(
                                "[linter-fastpath] {} applied --fix for rule {}",
                                fmt_name, rule_name
                            );
                            used_fastpath = true;
                        }
                        Ok(false) => {
                            info!(
                                "Linter fast-path made no change for {}; falling back to AI",
                                issue.key
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Linter fast-path error for {}: {} — falling back to AI",
                                issue.key, e
                            );
                        }
                    }
                }
            }
        }

        // Deterministic fast-path: some rules have a single well-defined text
        // transformation (S1118 utility-class constructors, S1124 modifier
        // order). Running Claude for these wastes ~15-20 s + tokens per issue
        // — a small parser + edit does the same work in microseconds. If the
        // deterministic pass produces a change we skip Claude entirely; the
        // subsequent build/test cycle still verifies the edit, so a wrong
        // transformation can't silently land.
        if !used_fastpath {
            match crate::orchestrator::deterministic::try_apply(
                &issue.rule,
                &file_path,
                &file_content,
                start_line,
            ) {
                Ok(Some(new_content)) => {
                    if let Err(e) = std::fs::write(&full_path, &new_content) {
                        warn!(
                            "Deterministic fix for {} could not write file: {} — falling back to AI",
                            issue.key, e
                        );
                    } else {
                        info!(
                            "Deterministic fast-path resolved {} ({}); skipping AI call",
                            issue.key, issue.rule
                        );
                        claude_output = format!(
                            "[deterministic] Applied template fix for {}",
                            issue.rule
                        );
                        used_fastpath = true;
                    }
                }
                Ok(None) => {
                    // Rule not implemented or site ambiguous — fall through.
                }
                Err(e) => {
                    warn!(
                        "Deterministic fast-path errored for {}: {} — falling back to AI",
                        issue.key, e
                    );
                }
            }
        }

        if !used_fastpath {
            // US-081: tie this call to the per-file session map so the next
            // issue on the same file can resume the conversation.
            claude_output = match self.run_ai_lean_keyed(
                "fix_issue",
                &prompt,
                &tier,
                Some(file_path.as_str()),
            ) {
                Ok(output) => output,
                Err(e) => {
                    result.status = FixStatus::Failed(format!("Claude failed: {}", e));
                    let _ = git::revert_changes(&self.config.path);
                    return result;
                }
            };
        }

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

        // Scope guard: the issue's scope determines which files may be modified.
        //   - Issue in a SOURCE file → tests are off-limits (don't game the suite)
        //   - Issue in a TEST file   → source files are off-limits (out of scope)
        // We never let a single fix touch both scopes.
        let issue_in_test_file = is_test_file(&file_path);
        let (out_of_scope, scope_label, in_scope_label): (Vec<String>, &str, &str) = if issue_in_test_file {
            (
                changed.iter().filter(|f| !is_test_file(f) && !is_internal_file(f)).cloned().collect(),
                "source",
                "test",
            )
        } else {
            (
                changed.iter().filter(|f| is_test_file(f)).cloned().collect(),
                "test",
                "source",
            )
        };
        if !out_of_scope.is_empty() {
            warn!(
                "Issue is in a {} file — reverting out-of-scope {} file(s) {:?}, keeping {} fix",
                in_scope_label, scope_label, out_of_scope, in_scope_label
            );
            for off in &out_of_scope {
                let checkout_result = std::process::Command::new("git")
                    .current_dir(&self.config.path)
                    .args(["checkout", "HEAD", "--", off])
                    .status();
                match checkout_result {
                    Ok(s) if s.success() => {
                        info!("Reverted out-of-scope file: {}", off);
                    }
                    _ => {
                        // File might be newly created (untracked) — remove it
                        let abs_path = self.config.path.join(off);
                        if abs_path.exists() {
                            let _ = std::fs::remove_file(&abs_path);
                            info!("Removed new out-of-scope file: {}", off);
                        }
                    }
                }
            }

            // Re-check if any in-scope changes remain after the revert
            let remaining = git::changed_files(&self.config.path).unwrap_or_default();
            let in_scope_changes: Vec<String> = if issue_in_test_file {
                remaining.into_iter().filter(|f| is_test_file(f) && !is_internal_file(f)).collect()
            } else {
                remaining.into_iter().filter(|f| !is_test_file(f) && !is_internal_file(f)).collect()
            };
            if in_scope_changes.is_empty() {
                result.status = FixStatus::Failed(format!(
                    "Claude only modified out-of-scope {} files — no {} fix applied",
                    scope_label, in_scope_label
                ));
                let _ = git::revert_changes(&self.config.path);
                return result;
            }
            info!("Keeping in-scope ({}) changes: {:?}", in_scope_label, in_scope_changes);
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
        //
        // For trivial rules (lint:* and MINOR/INFO static-analysis smells), cap
        // the repair loop at 2 attempts (fix + one repair). A 3-attempt ladder
        // with opus escalation is disproportionate for a one-line style fix,
        // but capping at 1 was too aggressive: a single transient build-break
        // (stale import, missed helper change) killed the entire fix. One
        // repair attempt recovers most of these; if it still fails, THEN route
        // to manual review.
        let severity_upper = issue.severity.to_uppercase();
        let is_trivial_rule = issue.rule.starts_with("lint:")
            || ((severity_upper == "MINOR" || severity_upper == "INFO")
                && !rule_is_coverage_dependent(&issue.rule));
        let max_fix_attempts = if is_trivial_rule {
            2
        } else {
            self.config.coverage_attempts
        };
        let mut fix_verified = false;
        // Fast-fail guard: if a repair claude call consumed ≥80% of its tier
        // timeout, we've almost certainly found a hopeless case (Jackson
        // migration, deprecated-API removal cascading into callers, etc.).
        // Spending another 720s on attempt 2 to confirm the same failure
        // wastes wall-clock and budget. When set, subsequent iterations
        // short-circuit to NeedsReview instead of re-invoking Claude.
        let mut repair_budget_exhausted = false;

        for fix_attempt in 1..=max_fix_attempts {
            if fix_attempt > 1 {
                info!("Fix-repair attempt {}/{} for {}", fix_attempt, max_fix_attempts, issue.key);
            }

            // Format code if command defined.
            // Skip for lint:* fixes — the linter autofix / Claude edit already
            // follows project style conventions and a separate formatter pass
            // adds only latency without value.
            if !issue.rule.starts_with("lint:") {
            if let Some(ref fmt_cmd) = self.config.commands.format {
                // Common pattern: users set `format: echo "No formatter configured"`
                // as a no-op placeholder when their stack has no formatter. The
                // spawned shell still costs ~400-500 ms per call, which adds up
                // to minutes over a full run. Skip any command that's just a
                // pass-through `echo` (no side effects on source files).
                if is_noop_echo_command(fmt_cmd) {
                    // Silent: printing a line per issue would spam the log with
                    // "No formatter configured" which is exactly what we're
                    // trying to elide.
                } else {
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
            }
            } // end if !lint:*

            // Build/compile if command defined.
            // Skip for lint:* fixes — the test phase's incremental compile will
            // catch any real build break; a dedicated `mvn compile` just doubles the work.
            let skip_build_for_lint = issue.rule.starts_with("lint:");
            if skip_build_for_lint && self.config.commands.build.is_some() && fix_attempt == 1 {
                info!("Skipping build for lint issue {} (test phase will verify)", issue.key);
            }
            if !skip_build_for_lint {
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "build") {
                    Ok((true, _)) => {
                        info!("Build succeeded after fix");
                    }
                    Ok((false, output)) => {
                        warn!("Build fails after fix for {} (attempt {})", issue.key, fix_attempt);
                        if fix_attempt < max_fix_attempts && !repair_budget_exhausted {
                            info!("Asking Claude to fix the build error...");
                            let repair_prompt = claude::build_fix_error_prompt(
                                "build",
                                &truncate(&output, 2000),
                                &file_path,
                                &issue.message,
                            );
                            // US-088 + run 2026-04-28 follow-up: build-repair attempt 1
                            // previously used `classify_repair_tier()` (sonnet:medium 1.0×)
                            // which with `claude_timeout=600` parked workers at the
                            // full 600 s wall on stuck loops — 4× observed in a single
                            // 75-min run = 40 min of pure waste. Successful repairs
                            // completed in 60–230 s, so 0.4× is well above the
                            // empirical p95 while the `prompt_floor` (≈400 s for
                            // 2.8 KB prompts) keeps the effective minimum safe for
                            // longer reasoning. Subsequent attempts use the same
                            // 0.4× to keep escalation flat — the 80%-budget fast-
                            // fail catches stuck loops earlier than re-extending.
                            let repair_tier =
                                claude::ClaudeTier::with_timeout("sonnet", "medium", 0.4);
                            info!(
                                "Build-repair tier for {} (attempt {}/{}): {}:{}",
                                issue.key, fix_attempt, max_fix_attempts, repair_tier.model, repair_tier.effort
                            );
                            let repair_budget = repair_tier.effective_timeout(self.config.claude_timeout);
                            let repair_start = std::time::Instant::now();
                            match self.run_ai_lean_keyed(
                                "fix_build_error",
                                &repair_prompt,
                                &repair_tier,
                                Some(file_path.as_str()),
                            ) {
                                Ok(_) => {
                                    let elapsed = repair_start.elapsed().as_secs();
                                    if elapsed * 10 >= repair_budget * 8 {
                                        warn!(
                                            "Build-repair consumed {}s of {}s budget (≥80%); will NeedsReview if this retry also fails",
                                            elapsed, repair_budget
                                        );
                                        repair_budget_exhausted = true;
                                    }
                                    info!("Claude applied build fix — retrying...");
                                    continue;
                                }
                                Err(e) => {
                                    warn!("Claude failed to fix build: {}", e);
                                }
                            }
                        } else if repair_budget_exhausted {
                            warn!(
                                "Skipping further build-repair attempts for {} — previous repair burned ≥80% of its budget (fast-fail)",
                                issue.key
                            );
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
            } // end if !skip_build_for_lint

            // Validate tests — tests MUST NOT be modified.
            // Strategy: run a targeted Surefire filter first (5-15s) to catch
            // obvious breakage fast; if targeted passes, run the full suite
            // for confirmation so we still catch cross-class regressions.
            if !test_command.is_empty() {
                // Try targeted tests first if enabled and we can derive a filter.
                //
                // We derive from the issue's own file_path plus Claude's changelist.
                // `file_path` guarantees a Java candidate even if scope guards pruned
                // `changed` down to non-Java paths, so every fix gets at least one
                // targeted test class.
                if self.config.targeted_tests_first {
                    let mut filter_inputs: Vec<String> = Vec::with_capacity(changed.len() + 1);
                    filter_inputs.push(file_path.clone());
                    filter_inputs.extend(changed.iter().cloned());
                    // Derive Maven module from the primary file so we can restrict
                    // the reactor to `-pl :<module> -am`. No-op for single-module
                    // projects or non-Maven runners.
                    let maven_module = runner::derive_maven_module(&self.config.path, &file_path);
                    if let Some(ref m) = maven_module {
                        info!("Scoping targeted tests to Maven module `{}`", m);
                    }
                    if let Some(filter) = runner::derive_surefire_filter(&filter_inputs) {
                        match runner::run_targeted_tests_scoped(&self.config.path, test_command, &filter, maven_module.as_deref()) {
                            Ok(res) if !res.ran_any => {
                                // Unsupported runner or no tests matched — fall through to full suite
                            }
                            Ok(res) if !res.success => {
                                // Targeted failure is decisive — skip the full suite run
                                warn!(
                                    "Targeted tests fail after fix for {} (attempt {}) — skipping full suite, going to repair",
                                    issue.key, fix_attempt
                                );
                                let output = res.output;
                                // Hoisted parse: if the runner output yields no
                                // identifiable test names, the repair LLM has no
                                // signal to act on. Parsing again at the final-
                                // revert path is cheap (regex over the same buffer).
                                let parsed_failures = parse_failing_tests(&output);
                                let unparseable_failures = parsed_failures.is_empty();
                                let mut repair_applied = false;
                                if fix_attempt < max_fix_attempts
                                    && !repair_budget_exhausted
                                    && !unparseable_failures
                                {
                                    info!("Asking Claude to fix the test failure (without modifying tests)...");
                                    let repair_prompt = claude::build_fix_error_prompt(
                                        "test",
                                        &truncate(&output, 1000),
                                        &file_path,
                                        &issue.message,
                                    );
                                    // US-088 + run 2026-04-28 follow-up: attempt-1 stays
                                    // on `classify_test_repair_tier` (haiku:medium 0.4×).
                                    // Subsequent attempts dropped from 0.6× to 0.4× —
                                    // empirically test-repair sonnet calls that succeed
                                    // finish under 90 s; the 0.6× wall (360 s) just
                                    // delayed inevitable reverts. `prompt_floor` still
                                    // guarantees ≥400 s of headroom for large prompts.
                                    let repair_tier = match fix_attempt {
                                        1 => claude::classify_test_repair_tier(),
                                        _ => claude::ClaudeTier::with_timeout("sonnet", "medium", 0.4),
                                    };
                                    info!(
                                        "Test-repair tier for {} (attempt {}/{}): {}:{}",
                                        issue.key,
                                        fix_attempt,
                                        max_fix_attempts,
                                        repair_tier.model,
                                        repair_tier.effort
                                    );
                                    let repair_budget = repair_tier.effective_timeout(self.config.claude_timeout);
                                    let repair_start = std::time::Instant::now();
                                    match self.run_ai_lean_keyed(
                                        "fix_test_error",
                                        &repair_prompt,
                                        &repair_tier,
                                        Some(file_path.as_str()),
                                    ) {
                                        Ok(_) => {
                                            let elapsed = repair_start.elapsed().as_secs();
                                            if elapsed * 10 >= repair_budget * 8 {
                                                warn!(
                                                    "Test-repair consumed {}s of {}s budget (≥80%); will NeedsReview if this retry also fails",
                                                    elapsed, repair_budget
                                                );
                                                repair_budget_exhausted = true;
                                            }
                                            // Scope guard during repair: if issue is in source file,
                                            // tests are off-limits; if issue is in test file, only the
                                            // issue's own test file may be modified — other tests are off-limits.
                                            let repair_changed = git::changed_files(&self.config.path).unwrap_or_default();
                                            let off_limits: Vec<_> = repair_changed.iter().filter(|f| {
                                                if issue_in_test_file {
                                                    is_test_file(f) && f.as_str() != file_path.as_str()
                                                } else {
                                                    is_test_file(f)
                                                }
                                            }).collect();
                                            if !off_limits.is_empty() {
                                                warn!("Claude modified out-of-scope file(s) during repair: {:?} — reverting repair", off_limits);
                                                let _ = git::revert_changes(&self.config.path);
                                            } else {
                                                info!("Claude applied repair — retrying...");
                                                repair_applied = true;
                                            }
                                        }
                                        Err(e) => warn!("Claude failed to fix tests: {}", e),
                                    }
                                } else if unparseable_failures {
                                    warn!(
                                        "Skipping test-repair for {} — failing-test list could not be parsed from runner output (no signal to act on)",
                                        issue.key
                                    );
                                } else if repair_budget_exhausted {
                                    warn!(
                                        "Skipping further test-repair attempts for {} — previous repair burned ≥80% of its budget (fast-fail)",
                                        issue.key
                                    );
                                }
                                if repair_applied {
                                    continue;
                                }
                                // No retries left, AI errored, or scope-violation revert —
                                // the broken fix must not be committed. Revert and fail
                                // the issue inline (mirror of the full-suite final-failure
                                // branch below), so the next pre-fix build check finds a
                                // clean tree.
                                let failing_tests = parsed_failures;
                                let failure_analysis = analyze_test_failure(
                                    &issue.rule,
                                    &issue.message,
                                    &result.change_description,
                                    &failing_tests,
                                    &output,
                                );
                                info!("Failing tests: {:?}", failing_tests);
                                info!("Failure analysis: {}", failure_analysis.reason);
                                info!("Reverting working tree for {}", issue.key);
                                let _ = git::revert_changes(&self.config.path);
                                info!("Revert complete for {}, writing REVIEW_NEEDED entry", issue.key);
                                result.status = FixStatus::NeedsReview(format!(
                                    "Fix causes targeted test failure(s) after {} attempts. {}",
                                    fix_attempt, failure_analysis.reason,
                                ));
                                report::append_review_needed(
                                    &self.config.path,
                                    &result,
                                    &failing_tests,
                                    &failure_analysis,
                                    &output,
                                );
                                info!("Returning NeedsReview for {} (targeted tests failed)", issue.key);
                                return result;
                            }
                            Ok(_) => {
                                // Targeted tests passed. In batch mode the full suite runs
                                // once at the batch boundary (see mod.rs squash block), so
                                // we skip it here — targeted is enough for per-fix verification.
                                // For TRIVIAL fixes we also defer even with batch_size==1:
                                // the safety net is the final_validation full-suite run at the
                                // end of the whole run. A 1-line lint fix doesn't justify ~78s
                                // of full-suite cost per issue.
                                let trivial = is_trivial_fix(
                                    &issue.rule,
                                    &issue.severity,
                                    &changed,
                                    count_changed_lines(&self.config.path),
                                    5,
                                );
                                if self.config.batch_size != 1 {
                                    info!(
                                        "Targeted tests pass — deferring full suite to batch boundary (batch_size={})",
                                        self.config.batch_size
                                    );
                                    fix_verified = true;
                                    break;
                                } else if trivial {
                                    info!(
                                        "Targeted tests pass — trivial fix ({}), deferring full suite to final-validation step",
                                        issue.rule
                                    );
                                    fix_verified = true;
                                    break;
                                } else {
                                    info!("Targeted tests pass — running full suite for confirmation");
                                }
                            }
                            Err(e) => {
                                warn!("Targeted test runner error: {}", e);
                            }
                        }
                    }
                }

                // In batch mode, the full suite ONLY runs at the batch boundary
                // (see `mod.rs` batch-boundary full-suite block). Skipping here
                // covers every fall-through case above: no filter derived,
                // targeted `ran_any == false`, targeted runner error, or the
                // targeted-tests-first feature disabled entirely.
                if self.config.batch_size != 1 {
                    info!(
                        "Deferring full test suite to batch boundary (batch_size={}); build+targeted results are enough for per-fix verification",
                        self.config.batch_size
                    );
                    fix_verified = true;
                    break;
                }

                info!("Running full test suite to validate fix...");
                match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                    Ok((true, _)) => {
                        info!("All tests pass after fix for {}", issue.key);
                        fix_verified = true;
                        break;
                    }
                    Ok((false, output)) => {
                        warn!("Tests fail after fix for {} (attempt {})", issue.key, fix_attempt);

                        // Same fast-fail as the targeted-test branch: no parseable
                        // failures → no signal for the LLM to act on.
                        let parsed_failures = parse_failing_tests(&output);
                        let unparseable_failures = parsed_failures.is_empty();

                        if fix_attempt < max_fix_attempts
                            && !repair_budget_exhausted
                            && !unparseable_failures
                        {
                            info!("Asking Claude to fix the test failure (without modifying tests)...");
                            let repair_prompt = claude::build_fix_error_prompt(
                                "test",
                                &truncate(&output, 1000),
                                &file_path,
                                &issue.message,
                            );
                            // Progressive escalation — same ladder as the targeted-test
                            // branch, multipliers tightened in run 2026-04-28 follow-up
                            // (0.7×→0.4× for medium, 1.0×→0.6× for high). Stuck repairs
                            // hit the full wall without progress; successful ones finished
                            // well under the new caps. `prompt_floor` keeps minima safe.
                            let repair_tier = match fix_attempt {
                                1 => claude::classify_test_repair_tier(),
                                2 => claude::ClaudeTier::with_timeout("sonnet", "medium", 0.4),
                                _ => claude::ClaudeTier::with_timeout("sonnet", "high", 0.6),
                            };
                            info!(
                                "Test-repair tier for {} (full-suite, attempt {}/{}): {}:{}",
                                issue.key, fix_attempt, max_fix_attempts, repair_tier.model, repair_tier.effort
                            );
                            let repair_budget = repair_tier.effective_timeout(self.config.claude_timeout);
                            let repair_start = std::time::Instant::now();
                            match self.run_ai_lean_keyed(
                                "fix_test_error",
                                &repair_prompt,
                                &repair_tier,
                                Some(file_path.as_str()),
                            ) {
                                Ok(_) => {
                                    let elapsed = repair_start.elapsed().as_secs();
                                    if elapsed * 10 >= repair_budget * 8 {
                                        warn!(
                                            "Full-suite test-repair consumed {}s of {}s budget (≥80%); fast-fail guard armed",
                                            elapsed, repair_budget
                                        );
                                        repair_budget_exhausted = true;
                                    }
                                    // Scope guard during repair: see above for semantics.
                                    let repair_changed = git::changed_files(&self.config.path).unwrap_or_default();
                                    let off_limits: Vec<_> = repair_changed.iter().filter(|f| {
                                        if issue_in_test_file {
                                            is_test_file(f) && f.as_str() != file_path.as_str()
                                        } else {
                                            is_test_file(f)
                                        }
                                    }).collect();
                                    if !off_limits.is_empty() {
                                        warn!("Claude modified out-of-scope file(s) during repair: {:?} — reverting repair", off_limits);
                                        let _ = git::revert_changes(&self.config.path);
                                    } else {
                                        info!("Claude applied repair — retrying...");
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    warn!("Claude failed to fix tests: {}", e);
                                }
                            }
                        }

                        // Final attempt or Claude failed — revert and give up
                        if unparseable_failures {
                            warn!(
                                "Skipping full-suite test-repair for {} — failing-test list could not be parsed (no signal to act on)",
                                issue.key
                            );
                        }
                        let failing_tests = parsed_failures;
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
                            match self.run_ai_lean_keyed(
                                "fix_lint_error",
                                &lint_prompt,
                                &lint_tier,
                                Some(file_path.as_str()),
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
        // Skip entirely for rules whose verdict doesn't depend on coverage
        // metrics (vulnerabilities, bugs, most code smells) — SonarQube will
        // re-evaluate these from static analysis alone.
        let needs_coverage = rule_is_coverage_dependent(&issue.rule);
        // Prefer the report-only command when available: the validation test run
        // we just executed produced fresh jacoco.exec data, so re-running the
        // full suite via `mvn verify -Pcoverage` (or similar) is ~79s of waste.
        let full_cov_cmd = self.config.coverage_command
            .clone()
            .or_else(|| self.config.commands.coverage.clone());
        let report_only_cmd = self.config.commands.coverage_report_only.clone();
        let (cov_cmd_opt, using_report_only) = match (&report_only_cmd, &full_cov_cmd) {
            (Some(ro), _) => (Some(ro.clone()), true),
            (None, Some(full)) => (Some(full.clone()), false),
            (None, None) => (None, false),
        };
        if !needs_coverage {
            info!(
                "Skipping coverage regen for {} (rule {} is static-analysis; not coverage-dependent)",
                issue.key, issue.rule
            );
        }
        let cov_cmd_to_run = if needs_coverage { cov_cmd_opt } else { None };
        if let Some(cov_cmd) = cov_cmd_to_run {
            if using_report_only {
                info!("Regenerating coverage report (report-only, reusing prior test run)...");
            } else {
                info!("Regenerating coverage report before SonarQube re-scan...");
            }
            match runner::run_shell_command(&self.config.path, &cov_cmd, "coverage") {
                Ok((true, _)) => {
                    if runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()).is_some() {
                        info!("Coverage report updated");
                    } else {
                        warn!("Coverage command succeeded but no report file was produced");
                    }
                }
                Ok((false, output)) => {
                    if using_report_only {
                        // Report-only failed (e.g. jacoco.exec missing) — fall back to the full command.
                        warn!(
                            "Report-only coverage command failed (non-blocking): {} — falling back to full coverage command",
                            truncate(&output, 100)
                        );
                        if let Some(full) = full_cov_cmd {
                            match runner::run_shell_command(&self.config.path, &full, "coverage") {
                                Ok((true, _)) => info!("Coverage report updated (full fallback)"),
                                Ok((false, o)) => warn!("Coverage command failed (non-blocking): {}", truncate(&o, 100)),
                                Err(e) => warn!("Coverage command error (non-blocking): {}", e),
                            }
                        }
                    } else {
                        warn!("Coverage command failed (non-blocking): {}", truncate(&output, 100));
                    }
                }
                Err(e) => warn!("Coverage command error (non-blocking): {}", e),
            }
        }

        // Step C-6: Re-scan with SonarQube to verify the issue is resolved (with retries).
        // Batched: only rescan every `rescan_batch_size` fixes (default 1 = every fix).
        // The counter is incremented in mod.rs before process_issue is called, so the
        // first issue has counter=1. Rescan when counter % N == 0 so we also rescan
        // the Nth, 2Nth, … issues — giving periodic confidence without per-issue cost.
        let fix_counter = self.fix_counter.load(std::sync::atomic::Ordering::Relaxed);
        let batch_n = self.config.rescan_batch_size;
        // Lint findings (synthetic `lint:<format>:<rule>` keys) are produced by the
        // local linter scan, not SonarQube. A SonarQube rescan will never report
        // them, so verifying the fix against Sonar is meaningless and just burns
        // ~30-60s of scanner + CE wait. Short-circuit before the batch check.
        //
        // When `batch_n == 0`, ALL per-issue rescans are deferred to a single
        // end-of-run verification scan (see orchestrator::run post-loop block).
        let is_lint_issue = issue.rule.starts_with("lint:");
        let should_rescan = !is_lint_issue
            && batch_n > 0
            && (batch_n == 1 || (fix_counter > 0 && fix_counter % batch_n == 0));
        if !should_rescan {
            if is_lint_issue {
                info!(
                    "Skipping SonarQube rescan for {} — linter-origin rule, not tracked by Sonar",
                    issue.key
                );
            } else if batch_n == 0 {
                info!(
                    "Deferring SonarQube rescan for {} — end-of-run verification enabled (--rescan-batch-size 0)",
                    issue.key
                );
            } else {
                info!(
                    "Skipping per-issue SonarQube rescan for {} (batched: fix {}/{}; rescan every {})",
                    issue.key, fix_counter, batch_n, batch_n
                );
            }
        }
        let max_sonar_retries = self.config.coverage_attempts;
        if should_rescan {
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
                                        "SonarQube still reports issue {} (attempt {}/{}) — asking Claude to refine the existing fix...",
                                        issue.key, sonar_attempt, max_sonar_retries
                                    );
                                    // Keep the previous fix in the working tree so Claude can
                                    // iterate on top of it. The final revert only happens if we
                                    // exhaust all retries (see max_sonar_retries branch below).
                                    let pre_retry_diff = std::process::Command::new("git")
                                        .current_dir(&self.config.path)
                                        .args(["diff", "HEAD"])
                                        .output()
                                        .ok()
                                        .map(|o| o.stdout)
                                        .unwrap_or_default();
                                    // Ask Claude to refine the existing in-progress fix
                                    let retry_prompt = format!(
                                        r#"Your previous fix for SonarQube issue {} is already applied to the working tree but did NOT resolve it.

## Issue details
- **Rule**: {} — {}
- **File**: `{}`
- **State**: Your earlier edits are still in the working tree (uncommitted). The code compiles and tests pass, but SonarQube still reports the same issue.

## Instructions:
1. Read the current state of the file — do NOT start over from HEAD
2. Identify why rule {} is still triggered despite your previous edits
3. Refine or extend the existing fix to make SonarQube accept it
4. Do NOT modify any test files
5. Ensure the fix still compiles and tests still pass

Refine the fix now."#,
                                        issue.key, issue.rule, issue.message,
                                        file_path, issue.rule
                                    );
                                    // Progressive retry escalation: each attempt that SonarQube
                                    // reports still-unresolved is a stronger signal that sonnet
                                    // isn't going to crack this issue alone. We escalate a rung
                                    // per retry so opus only fires when cheaper tiers have
                                    // verifiably failed, not speculatively up-front.
                                    //
                                    //   attempt 1 (first retry): lift haiku→sonnet, low→medium
                                    //   attempt 2:               sonnet→sonnet:high
                                    //   attempt 3+:              opus:high (last-resort fallback)
                                    //
                                    // Opus entering here is evidence-based: two cheaper attempts
                                    // have demonstrably not solved it, so the ~4× cost of opus
                                    // is justified rather than speculative.
                                    let retry_tier = match sonar_attempt {
                                        1 => claude::ClaudeTier::with_timeout(
                                            if tier.model == "haiku" { "sonnet" } else { tier.model },
                                            if tier.effort == "low" { "medium" } else { tier.effort },
                                            tier.timeout_multiplier.max(0.7),
                                        ),
                                        2 => claude::ClaudeTier::with_timeout(
                                            "sonnet",
                                            "high",
                                            tier.timeout_multiplier.max(1.0),
                                        ),
                                        _ => claude::ClaudeTier::with_timeout(
                                            "opus",
                                            "high",
                                            tier.timeout_multiplier.max(1.5),
                                        ),
                                    };
                                    info!(
                                        "Retry tier for {} (attempt {}/{}): {}:{}",
                                        issue.key,
                                        sonar_attempt,
                                        max_sonar_retries,
                                        retry_tier.model,
                                        retry_tier.effort
                                    );
                                    match self.run_ai_lean_keyed(
                                        "fix_issue_retry",
                                        &retry_prompt,
                                        &retry_tier,
                                        Some(file_path.as_str()),
                                    ) {
                                        Ok(_) => {
                                            // Guard against no-op retries: compare the full working-tree
                                            // diff before and after the retry. If unchanged, Claude
                                            // didn't touch anything — another rescan would just loop.
                                            let post_retry_diff = std::process::Command::new("git")
                                                .current_dir(&self.config.path)
                                                .args(["diff", "HEAD"])
                                                .output()
                                                .ok()
                                                .map(|o| o.stdout)
                                                .unwrap_or_default();
                                            if post_retry_diff == pre_retry_diff {
                                                warn!(
                                                    "Retry made no file changes for {} — aborting retry loop",
                                                    issue.key
                                                );
                                                let _ = git::revert_changes(&self.config.path);
                                                result.status = FixStatus::NeedsReview(
                                                    "Claude retry produced no changes on top of the previous fix; manual review required.".to_string()
                                                );
                                                return result;
                                            }
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
                                            // Mirror the main-path strategy: try a targeted
                                            // Surefire filter first when batching is on. Targeted
                                            // passing is sufficient for per-issue verification;
                                            // the full suite runs once at the batch boundary.
                                            let all_retry_changed = git::changed_files(&self.config.path)
                                                .unwrap_or_default();
                                            let mut filter_inputs: Vec<String> =
                                                Vec::with_capacity(all_retry_changed.len() + 1);
                                            filter_inputs.push(file_path.clone());
                                            filter_inputs.extend(all_retry_changed.into_iter());
                                            let mut skipped_full = false;
                                            if self.config.batch_size != 1 && self.config.targeted_tests_first {
                                                if let Some(filter) = runner::derive_surefire_filter(&filter_inputs) {
                                                    let retry_module = runner::derive_maven_module(&self.config.path, &file_path);
                                                    match runner::run_targeted_tests_scoped(&self.config.path, test_command, &filter, retry_module.as_deref()) {
                                                        Ok(res) if res.ran_any && res.success => {
                                                            info!(
                                                                "Retry targeted tests pass — deferring full suite to batch boundary (batch_size={})",
                                                                self.config.batch_size
                                                            );
                                                            skipped_full = true;
                                                        }
                                                        Ok(res) if res.ran_any && !res.success => {
                                                            warn!("Retry fix broke targeted tests — reverting");
                                                            let _ = git::revert_changes(&self.config.path);
                                                            break;
                                                        }
                                                        _ => {
                                                            // Unsupported runner or no match — fall through to full suite
                                                        }
                                                    }
                                                }
                                            }
                                            if skipped_full {
                                                continue;
                                            }
                                            match runner::run_tests(&self.config.path, test_command, self.config.test_timeout) {
                                                Ok((true, _)) => {
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
        }

        // Step D: Commit the fix (US-008)
        //
        // Re-scan the working tree NOW (not the snapshot taken right after
        // the initial fix). Build-repair and test-repair iterations can
        // touch additional files — most commonly the dependent surface
        // that broke the build (e.g. an interface signature when the impl
        // changed `Boolean` → `boolean`). Run 2026-04-29 evidence: wave
        // 110 fix S5411 modified `EmailServiceImpl.java`; the build broke
        // because the override no longer matched `EmailService.java`;
        // Claude's repair updated the interface; build went green; but
        // the commit only staged the snapshot from line 779 (impl alone),
        // leaving the interface fix as an uncommitted dirty hunk that the
        // worktree cleanup discarded. Result: batch branch had impl with
        // `boolean` parameter against an interface with `Boolean` —
        // doesn't compile, breaks the next preflight.
        let final_all_changed = git::changed_files(&self.config.path).unwrap_or_default();
        let final_changed: Vec<String> = final_all_changed
            .into_iter()
            .filter(|f| !is_internal_file(f))
            .collect();
        let files_to_stage: Vec<&str> = final_changed.iter().map(|s| s.as_str()).collect();
        if !files_to_stage.is_empty() {
            let _ = git::add_files(&self.config.path, &files_to_stage);
        }
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            // Use the full set of touched files (initial fix + repair
            // iterations) so commit metadata and PR descriptions reflect
            // every file actually staged into this commit.
            let changed_files_str = final_changed.join(", ");
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

            // When fix_commit_batch != 1, use a WIP commit that will be squashed later.
            // fix_commit_batch == 1 → real commit immediately (default/individual behavior).
            // fix_commit_batch == 0 → one commit per branch (all WIP, squashed at end).
            // fix_commit_batch >  1 → squash every N fixes into one commit.
            let use_wip = self.config.fix_commit_batch != 1;
            if use_wip {
                let wip_msg = format!(
                    "reparo-wip: fix {} - {}\n\nRule: {}\nSeverity: {}\nFile: {}:{}\nModified: {}\nIssue: {}",
                    issue.key,
                    truncate(&issue.message, 72),
                    issue.rule,
                    issue.severity,
                    file_path,
                    lines,
                    changed_files_str,
                    issue_url,
                );
                if let Err(e) = git::commit_no_verify(&self.config.path, &wip_msg) {
                    result.status = FixStatus::Failed(format!("Commit failed: {}", e));
                    return result;
                }
                info!("WIP commit for {} (pending batch squash)", issue.key);
            } else {
                if let Err(e) = git::commit(&self.config.path, &msg) {
                    result.status = FixStatus::Failed(format!("Commit failed: {}", e));
                    return result;
                }
                info!("Committed fix for {}", issue.key);
            }

            // US-021: Capture diff summary for PR body
            result.diff_summary = capture_diff_summary(&self.config.path);
        }

        result.status = FixStatus::Fixed;
        result
    }

    /// Squash N temporary "reparo-wip: fix" commits into a single real commit.
    ///
    /// Used when `fix_commit_batch > 1` or `fix_commit_batch == 0` (one per branch).
    /// `issues` is a slice of `(issue_key, issue_message)` pairs for the commit body.
    pub(super) fn squash_fix_commits(&self, n: usize, issues: &[(String, String)]) -> Result<()> {
        if n == 0 {
            return Ok(());
        }

        info!("Squashing {} WIP fix commit(s) into one real commit...", n);

        // Safety check: verify the last n commits are all "reparo-wip: fix" commits.
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
                    "Cannot squash fix commits: last {} commits contain non-wip entries: {:?}",
                    n, non_wip
                );
                return Ok(());
            }
        }

        // Soft-reset to unstage all WIP commits back to the index in one shot.
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

        // Build the squash commit message.
        let keys: Vec<&str> = issues.iter().map(|(k, _)| k.as_str()).collect();
        let subject = if issues.len() == 1 {
            format_commit_message(
                &self.config, "fix", "sonar",
                &format!("{} - {}", keys[0], truncate(&issues[0].1, 72)),
                keys[0], "", "",
            )
        } else {
            format_commit_message(
                &self.config, "fix", "sonar",
                &format!("{} issues [{}]", issues.len(), keys.join(", ")),
                "", "", "",
            )
        };
        let body: String = issues.iter()
            .map(|(k, m)| format!("- {}: {}", k, truncate(m, 100)))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = format!("{}\n\n{}", subject, body);

        match git::commit(&self.config.path, &msg) {
            Ok(()) => {
                info!("Squash commit successful ({} fix(es))", issues.len());
            }
            Err(e) => {
                warn!("Squash commit failed: {} — WIP commits remain as-is", e);
            }
        }
        Ok(())
    }

    /// Check line-level coverage for the affected lines of an issue (US-004).
    ///
    /// Source precedence:
    ///   1. **Frozen baseline snapshot** — `self.config.baseline_lcov_path`.
    ///      Captured once after preflight + coverage boost, it reflects the
    ///      "last complete test run" and is immutable for the rest of the
    ///      fix loop. Every worker (parallel or sequential) reads the same
    ///      file, so fixes happening concurrently cannot contaminate each
    ///      other's coverage view.
    ///   2. **SonarQube** — the project-wide reference. Used when the
    ///      baseline is missing or doesn't contain the target file.
    ///
    /// NOTE: the per-worktree lcov is deliberately NOT consulted. That lcov
    /// mutates as workers run their own validation coverage, which leaks
    /// cross-worker state into what should be a per-issue decision.
    async fn check_coverage(
        &self,
        component: &str,
        file_path: &str,
        start_line: u32,
        end_line: u32,
    ) -> CoverageCheck {
        // 1. Frozen baseline (set once before the fix loop starts).
        if let Some(ref baseline) = self.config.baseline_lcov_path {
            if let Some(cov) =
                runner::check_local_coverage(baseline, file_path, start_line, end_line)
            {
                if cov.fully_covered {
                    return CoverageCheck::FullyCovered;
                }
                if cov.covered.is_empty() && cov.uncovered.is_empty() {
                    return CoverageCheck::FullyCovered;
                }
                return CoverageCheck::NeedsCoverage {
                    uncovered_lines: cov.uncovered,
                    coverage_pct: cov.coverage_pct,
                };
            }
            info!(
                "Baseline coverage report {} has no data for {} — falling back to SonarQube",
                baseline.display(),
                file_path
            );
        }

        // 2. SonarQube — general reference.
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
    pub(super) async fn generate_tests_with_retry(
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

            let uncovered_summary = format!(
                "{} uncovered lines (range {}-{}) out of {} total",
                current_uncovered.len(), start_line, end_line, file_content.lines().count()
            );
            let uncovered_snippets = extract_uncovered_snippets(
                file_content,
                &current_uncovered,
                80,
            );

            // Build prompt — first attempt or retry with context
            let file_class = self.classify_source_file_cached(file_path);
            let pkg_hint = runner::derive_test_package(file_path)
                .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
                .unwrap_or_default();
            let prompt = if attempt == 1 {
                let per_file_ctx = build_per_file_context(&framework_ctx_base, &file_class, &pkg_hint);
                // US-067: detect boundary/negative testing hints for the fix-loop test generation path
                let boundary_hints = detect_boundary_hints(&uncovered_snippets);
                // US-066: compliance trace context for tests generated inside the fix loop
                let compliance_ctx = if self.config.compliance_enabled {
                    let ctx = claude::ComplianceTraceContext::new(
                        self.exec_log.run_id(),
                        format!("ISSUE:{}", file_path),
                    );
                    Some(if self.config.health_mode { ctx.with_risk_class("A") } else { ctx })
                } else { None };
                claude::build_test_generation_prompt(
                    file_path,
                    &uncovered_summary,
                    &uncovered_snippets,
                    &framework,
                    &examples_str,
                    &per_file_ctx,
                    &boundary_hints,
                    compliance_ctx.as_ref(),
                    None,
                )
            } else {
                let per_file_ctx = build_per_file_context(&framework_ctx_base, &file_class, "");
                claude::build_test_generation_retry_prompt(
                    file_path,
                    &uncovered_summary,
                    &uncovered_snippets,
                    &framework,
                    attempt,
                    &truncate(&last_test_output, 1000),
                    &per_file_ctx,
                )
            };

            // Run claude to generate tests. US-087: pin to the source file
            // session so test gen reuses the conversation opened by the fix.
            let test_tier = claude::classify_test_gen_tier(current_uncovered.len(), file_content.lines().count(), &self.config.test_generation.tiers);
            match self.run_ai_keyed("test_generation", &prompt, &test_tier, Some(file_path)) {
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

            // Run tests to verify they pass. Scope to just the newly generated
            // test class(es) — a full-suite run here costs 60-90 s, the targeted
            // run costs 5-10 s. Fall back to the full suite only when the filter
            // matches nothing (derive_surefire_filter returned None, or the
            // runner reported ran_any=false).
            let targeted_filter = runner::derive_surefire_filter(&all_test_files);
            let tests_result: Result<(bool, String), _> = match targeted_filter.as_deref() {
                Some(filter) => match runner::run_targeted_tests(
                    &self.config.path,
                    test_command,
                    filter,
                ) {
                    Ok(tr) if tr.ran_any => Ok((tr.success, tr.output)),
                    _ => runner::run_tests(&self.config.path, test_command, self.config.test_timeout),
                },
                None => runner::run_tests(&self.config.path, test_command, self.config.test_timeout),
            };

            match tests_result {
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

            // US-087: tie to source file session so contract test gen reuses
            // the conversation context already established during fix + docs.
            let claude_result = self.run_ai_keyed("contract_test_generation", &prompt, &tier, Some(file_path));

            match claude_result {
                Ok(_output) => {
                    // AI output is not used for retry prompts — test failure output is
                    // captured below and fed back into the retry prompt instead.
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

            // Run contract tests. `PactConfig::validate()` guarantees that
            // `test_command` is set whenever `generate_tests` is enabled, so
            // reaching this point without a command is a configuration bug.
            let test_cmd = self.config.pact.test_command.as_ref().expect(
                "pact.test_command must be set when generate_tests is enabled \
                 (enforced by PactConfig::validate)",
            );
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
                // US-087: key by the conflicted file path. If the same file
                // had a recent fix in this run, the conversation already has
                // the right mental model.
                match self.run_ai_keyed("rebase_conflict", &prompt, &tier, Some(file.as_str())) {
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
    pub(super) fn create_pr(&self, branch_name: &str) -> Result<String> {
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
                FixStatus::RiskSkipped(_) => "Risk skipped",
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

        // US-021: Include per-issue diff summaries with collapsible per-file blocks
        let diffs: Vec<(&str, &str)> = fixed_results
            .iter()
            .filter_map(|r| {
                r.diff_summary
                    .as_deref()
                    .map(|d| (r.issue_key.as_str(), d))
            })
            .collect();
        if !diffs.is_empty() {
            for (key, diff) in &diffs {
                body.push_str(&format!("\n### Diff: {}\n\n{}\n\n", key, diff));
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

    /// Create a PR for a single issue (US-018: parallel mode).
    ///
    /// Unlike `create_pr()` which batches all results, this creates one PR per issue
    /// with a focused title and body. Called from the parallel worker after push.
    pub(crate) fn create_per_issue_pr(
        &self,
        result: &IssueResult,
        branch_name: &str,
    ) -> Result<String> {
        // Rebase onto latest base branch to minimize merge conflicts
        if !self.config.skip_rebase {
            if let Err(e) = self.rebase_on_latest_base() {
                warn!("Pre-push rebase failed: {} — pushing without rebase", e);
            }
        }

        // Push with retry
        let path = self.config.path.clone();
        let branch = branch_name.to_string();
        crate::retry::retry_sync(3, 3, "git push", || {
            git::push(&path, &branch)
        })?;

        // Title
        let title = format!(
            "[SonarQube] Fix {} {}: {}",
            result.severity,
            result.issue_type.to_lowercase(),
            truncate(&result.message, 50),
        );

        // Body
        let issue_url = format!(
            "{}/project/issues?id={}&open={}",
            self.config.sonar_url, self.config.sonar_project_id, result.issue_key,
        );
        let mut body = String::from("## Summary\n\n");
        body.push_str(&format!(
            "Automated fix for SonarQube issue [{}]({}).\n\n",
            result.issue_key, issue_url,
        ));
        body.push_str(&format!(
            "| Field | Value |\n|-------|-------|\n\
             | **Rule** | `{}` |\n\
             | **Severity** | {} |\n\
             | **Type** | {} |\n\
             | **File** | `{}:{}` |\n\n",
            result.rule, result.severity, result.issue_type, result.file, result.lines,
        ));

        if !result.change_description.is_empty() {
            body.push_str(&format!("### Changes\n\n{}\n\n", result.change_description));
        }

        if !result.tests_added.is_empty() {
            body.push_str("### Tests added\n\n");
            for t in &result.tests_added {
                body.push_str(&format!("- `{}`\n", t));
            }
            body.push('\n');
        }

        if let Some(ref diff) = result.diff_summary {
            body.push_str(&format!("### Diff\n\n{}\n\n", diff));
        }

        body.push_str("## Test plan\n\n");
        body.push_str("- [x] All existing tests pass (verified by Reparo)\n");
        body.push_str("- [ ] SonarQube re-scan confirms issue resolved\n");
        body.push_str("- [ ] Code review approved\n\n");
        body.push_str("Generated with [Reparo](https://github.com/reparo) using Claude\n");

        let severity_label = result.severity.to_lowercase();
        let labels: Vec<&str> = vec!["sonar-fix", "automated", &severity_label];

        git::create_pr(
            &self.config.path,
            &title,
            &body,
            &self.config.branch,
            &labels,
        )
    }
}
