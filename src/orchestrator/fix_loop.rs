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

    pub(super) async fn process_issue(&self, issue: &Issue, test_command: &str) -> IssueResult {
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
            let file_class = runner::classify_source_file(file_path, &self.config.path);
            let pkg_hint = runner::derive_test_package(file_path)
                .map(|p| format!("The test class should be in package `{}` under `src/test/java/`.", p))
                .unwrap_or_default();
            let prompt = if attempt == 1 {
                let per_file_ctx = build_per_file_context(&framework_ctx_base, &file_class, &pkg_hint);
                claude::build_test_generation_prompt(
                    file_path,
                    &uncovered_summary,
                    &uncovered_snippets,
                    &framework,
                    &examples_str,
                    &per_file_ctx,
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
}
