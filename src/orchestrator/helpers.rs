use std::path::{Path, PathBuf};

use crate::config::ValidatedConfig;
use crate::report::TestFailureAnalysis;
use crate::sonar;

/// Result of the coverage check for an issue's affected lines (US-004).
pub(crate) enum CoverageCheck {
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
pub(crate) enum TestGenResult {
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
pub(crate) struct BoostFileResult {
    /// Source file that was boosted
    pub file: String,
    /// Test files created (relative paths)
    pub test_files: Vec<String>,
    /// Generated artifacts to stage alongside tests (coverage reports, etc.)
    pub artifacts: Vec<String>,
    /// Number of rounds that produced passing tests
    pub rounds_completed: u32,
    /// File coverage percentage before boost
    pub coverage_before: f64,
    /// Overall project coverage already measured during the boost (individual mode only).
    /// When `Some`, the caller can skip a redundant `run_coverage_and_measure` call.
    pub measured_overall_pct: Option<f64>,
}

/// Returns true when the SonarQube rule's verdict depends on code coverage
/// metrics (e.g. `common-java:InsufficientLineCoverage`). For these rules
/// we must regenerate the coverage report before the rescan, otherwise
/// SonarQube will still see the stale numbers.
///
/// For every other rule (vulnerabilities, bugs, most code smells) the
/// verdict comes from static analysis and the coverage report is irrelevant,
/// so we can skip the ~79s coverage regeneration.
pub(crate) fn rule_is_coverage_dependent(rule_key: &str) -> bool {
    let k = rule_key.to_lowercase();
    // Linter-origin rules (synthetic `lint:<format>:<rule>` keys) are
    // static-analysis findings. Coverage regen is never needed for them.
    if k.starts_with("lint:") {
        return false;
    }
    // Known SonarQube coverage rule keywords. Kept narrow on purpose —
    // a false negative (skipping regen when we shouldn't) triggers a rescan
    // retry in fix_loop, so the loop is self-healing.
    k.contains("coverage") || k.contains("uncovered") || k.contains("linecoverage") || k.contains("branchcoverage")
}

/// Merge linter-derived and SonarQube-derived issues into a single severity-
/// interleaved queue. Within a severity bucket, linter findings are placed
/// before sonar findings so the new phase's work gets a fair chance.
///
/// When `reverse_severity` is true, the ordering is flipped (INFO → BLOCKER).
pub(crate) fn merge_lint_and_sonar_issues(
    lint: Vec<sonar::Issue>,
    sonar: Vec<sonar::Issue>,
    reverse_severity: bool,
) -> Vec<sonar::Issue> {
    fn rank(sev: &str) -> u8 {
        match sev.to_uppercase().as_str() {
            "BLOCKER" => 0,
            "CRITICAL" => 1,
            "MAJOR" => 2,
            "MINOR" => 3,
            "INFO" => 4,
            _ => 5,
        }
    }

    // Bucket each list by severity rank; preserve input order within a bucket.
    let mut buckets: Vec<Vec<sonar::Issue>> = (0..=5).map(|_| Vec::new()).collect();
    for i in lint {
        let r = rank(&i.severity) as usize;
        buckets[r].push(i);
    }
    // Mark the split point per bucket so lint items stay ahead of sonar.
    let lint_lens: Vec<usize> = buckets.iter().map(|b| b.len()).collect();
    for i in sonar {
        let r = rank(&i.severity) as usize;
        buckets[r].push(i);
    }

    // Re-order each bucket: [lint..., sonar...] is already the layout we have.
    // Nothing else to do — we pushed lint first then sonar.
    let _ = lint_lens;

    let mut out: Vec<sonar::Issue> = Vec::new();
    if reverse_severity {
        for b in buckets.into_iter().rev() {
            out.extend(b);
        }
    } else {
        for b in buckets.into_iter() {
            out.extend(b);
        }
    }
    out
}

/// Parse test output to extract names of failing tests (US-007).
///
/// Handles common test runner output formats:
/// - pytest: `FAILED tests/test_foo.py::test_bar`
/// - JUnit/Maven: `Tests run: X, Failures: Y` + `testMethodName(ClassName)`
/// - Jest: `FAIL src/foo.test.js` + `✕ test name`
/// - Go: `--- FAIL: TestFoo`
/// - Rust: `test module::test_name ... FAILED`
pub(crate) fn parse_failing_tests(output: &str) -> Vec<String> {
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
pub(crate) fn analyze_test_failure(
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

/// Capture a per-file diff summary of the last commit for PR body (US-021).
///
/// Produces collapsible `<details>/<summary>` blocks per file with:
/// - File name and `(+N, -M)` line counts in the summary
/// - Unified diff with 3 lines of context, truncated to 50 lines per file
/// - Total output capped at 200 lines across all files
/// - Test files excluded (new test files are mostly full-file diffs)
pub(crate) fn capture_diff_summary(project_path: &Path) -> Option<String> {
    // Get the list of changed files with numstat (+added, -removed)
    let numstat_output = std::process::Command::new("git")
        .current_dir(project_path)
        .args(["diff", "HEAD~1", "--numstat"])
        .output()
        .ok()?;
    if !numstat_output.status.success() {
        return None;
    }

    let numstat = String::from_utf8_lossy(&numstat_output.stdout);
    let file_stats: Vec<(&str, &str, &str)> = numstat
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                Some((parts[0], parts[1], parts[2]))
            } else {
                None
            }
        })
        .collect();

    if file_stats.is_empty() {
        return None;
    }

    let mut result = String::new();
    let mut total_lines = 0usize;
    const MAX_LINES_PER_FILE: usize = 50;
    const MAX_LINES_TOTAL: usize = 200;

    for (added, removed, file) in &file_stats {
        // Skip test files — they're typically full-file additions
        if is_test_file(file) {
            continue;
        }

        if total_lines >= MAX_LINES_TOTAL {
            result.push_str("\n*Full diff available in the Files tab*\n");
            break;
        }

        // Get per-file diff
        let diff_output = std::process::Command::new("git")
            .current_dir(project_path)
            .args(["diff", "HEAD~1", "-U3", "--", file])
            .output()
            .ok();

        let diff_text = match diff_output {
            Some(ref out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).to_string()
            }
            _ => continue,
        };

        // Truncate per-file diff to MAX_LINES_PER_FILE
        let diff_lines: Vec<&str> = diff_text.lines().collect();
        let remaining_budget = MAX_LINES_TOTAL.saturating_sub(total_lines);
        let file_limit = MAX_LINES_PER_FILE.min(remaining_budget);
        let (file_diff, was_truncated) = if diff_lines.len() > file_limit {
            (
                diff_lines[..file_limit].join("\n"),
                true,
            )
        } else {
            (diff_text.clone(), false)
        };

        let lines_used = diff_lines.len().min(file_limit);
        total_lines += lines_used;

        let truncation_note = if was_truncated {
            format!(
                "\n... ({} more lines, see Files tab)",
                diff_lines.len() - file_limit
            )
        } else {
            String::new()
        };

        result.push_str(&format!(
            "<details>\n<summary>{} (+{}, -{})</summary>\n\n```diff\n{}{}\n```\n\n</details>\n\n",
            file, added, removed, file_diff, truncation_note
        ));
    }

    if result.is_empty() {
        return None;
    }

    Some(result.trim_end().to_string())
}

#[allow(dead_code)]
pub(crate) fn sanitize_branch(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

pub(crate) fn format_lines(text_range: &Option<sonar::TextRange>) -> String {
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
pub(crate) fn build_change_description(claude_output: &str, changed_files: &[String]) -> String {
    let summary = claude_output
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .unwrap_or("Automated fix applied");

    let files_str = changed_files.join(", ");
    format!("{} [files: {}]", summary, files_str)
}

pub(crate) fn detect_test_framework(project_path: &Path) -> String {
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
pub(crate) fn build_framework_context(
    detected_deps: &str,
    tg: &crate::config::TestGenerationConfig,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Start with auto-detected dependencies
    if !detected_deps.is_empty() {
        parts.push(format!("Detected test dependencies: {}", detected_deps));
    }

    // Framework-specific project-level rules — injected once per run, not per file.
    // Per-file guidance (component type, decorators, etc.) comes from classify_source_file.
    if detected_deps.contains("Angular") {
        parts.push("Always import { TestBed, ComponentFixture } from '@angular/core/testing' — never from '@angular/core'.".to_string());
        parts.push("Never instantiate a @Component with new — always go through TestBed.createComponent().".to_string());
        parts.push("Declare every component/directive/pipe used in the template inside the same TestBed.configureTestingModule({ declarations: [...] }).".to_string());
        if detected_deps.contains("Karma") {
            parts.push("Test runner is Karma (browser Zone.js). Use fakeAsync/tick for timer-based async; waitForAsync/fixture.whenStable() for promise-based async. Plain async in it() blocks is unreliable with Karma.".to_string());
        } else {
            parts.push("Test runner is Jest (Node). async/await works natively; fakeAsync/tick is also available.".to_string());
        }
    } else if detected_deps.contains("NestJS") {
        parts.push("Use @nestjs/testing: Test.createTestingModule({ providers: [...] }).compile() — never new Service() directly.".to_string());
        parts.push("Always provide mocks as { provide: RealDep, useValue: { method: jest.fn() } } in the providers array.".to_string());
        parts.push("Retrieve instances via moduleRef.get(ServiceClass) after compile().".to_string());
    } else if detected_deps.contains("Next.js") {
        parts.push("Use @testing-library/react for component tests. Always mock routing before rendering.".to_string());
        parts.push("For App Router: jest.mock('next/navigation', () => ({ useRouter: () => ({ push: jest.fn() }), usePathname: () => '/', useSearchParams: () => new URLSearchParams() })).".to_string());
        parts.push("For Pages Router: jest.mock('next/router', () => ({ useRouter: () => ({ push: jest.fn(), pathname: '/', query: {} }) })).".to_string());
    } else if detected_deps.starts_with("React") {
        parts.push("Use @testing-library/react. Prefer query priority: getByRole > getByLabelText > getByText > getByTestId.".to_string());
        parts.push("Use userEvent from @testing-library/user-event for interactions — it wraps act() automatically.".to_string());
        if detected_deps.contains("MSW") {
            parts.push("Use MSW (Mock Service Worker) to intercept HTTP requests in tests — do not mock fetch/axios directly.".to_string());
        }
    } else if detected_deps.starts_with("Vue") {
        parts.push("Use @vue/test-utils. Use shallowMount to isolate the component under test (stubs children); use mount when child behaviour matters.".to_string());
        parts.push("Trigger events asynchronously: await wrapper.trigger('click'); call await wrapper.vm.$nextTick() after data mutations before asserting DOM.".to_string());
    } else if detected_deps.contains("Laravel") {
        parts.push("Extend Tests\\TestCase (not bare PHPUnit TestCase) to get Laravel test helpers.".to_string());
        parts.push("Use RefreshDatabase trait whenever the test touches the database.".to_string());
        parts.push("Use model factories (ModelClass::factory()->create()) instead of raw DB inserts.".to_string());
    } else if detected_deps.contains("Symfony") {
        parts.push("Extend WebTestCase for HTTP tests, KernelTestCase for service/repository tests, plain TestCase for value objects and utilities.".to_string());
        parts.push("Access services in KernelTestCase via static::getContainer()->get(ServiceClass::class).".to_string());
    } else if detected_deps.contains("RSpec") {
        parts.push("Use described_class to refer to the class under test — avoids hard-coding the name.".to_string());
        parts.push("Use let for lazy-evaluated setup; let! for eager setup. Use subject for the primary object under test.".to_string());
        if detected_deps.contains("FactoryBot") {
            parts.push("Use FactoryBot.create(:factory_name) for persisted records, build for in-memory only.".to_string());
        }
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

/// Build a slim framework context for retry rounds (US-056).
///
/// On round > 1 the AI already knows the framework from its previous attempt.
/// Only flags that directly affect *how* to write tests (not which deps exist) are included.
pub(crate) fn build_slim_framework_context(tg: &crate::config::TestGenerationConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if tg.avoid_spring_context {
        parts.push("IMPORTANT: Do NOT use @SpringBootTest for unit tests. Use @ExtendWith(MockitoExtension.class) or plain JUnit 5 instead.".to_string());
    }
    if let Some(ref custom) = tg.custom_instructions {
        parts.push(custom.clone());
    }
    parts.join("\n")
}

/// Build per-file context combining base framework context with file-specific classification (US-040).
pub(crate) fn build_per_file_context(base: &str, file_classification: &str, package_hint: &str) -> String {
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

/// Extract annotated code snippets for uncovered lines, grouping consecutive
/// lines into contiguous blocks with ±1 line of context.  Returns a compact
/// representation that the AI can act on directly without reading the file.
///
/// Output format (one block per group):
/// ```text
/// // Lines 45-52 (UNCOVERED):
///  44: previousLine          ← context
/// >45: uncoveredLine         ← needs coverage
/// >46: uncoveredLine
///  53: nextLine              ← context
/// ```
/// US-067: Scan a source snippet for boundary/negative testing opportunities
/// and return a list of hints that will be injected into the generation prompt.
///
/// The hints are heuristic — regex-based, not semantic — so they only cover
/// the most obvious patterns:
/// - Numeric comparisons against literals (`x > 100`, `len >= MAX`) → test at boundary
/// - String/collection emptiness (`isEmpty`, `is_empty`, `== ""`) → test with empty
/// - Exception throws (`throw`, `raise`, `return Err`) → test the error path
/// - Nullability (`null`, `None`, `undefined`, `.orElse`, `Optional`) → test with null
/// - Array/list access (`arr[i]`, `list.get(i)`) → test index=0, index=last, OOB
///
/// Returns an empty string when no hints are detected (keeps the prompt lean
/// for trivial files). The caller decides whether to include the "Mandatory
/// categories" section based on emptiness.
pub(crate) fn detect_boundary_hints(source_snippet: &str) -> String {
    let mut hints: Vec<String> = Vec::new();

    // Precompile regexes once per call; regex crate caches internally.
    let num_cmp = regex::Regex::new(r"([<>]=?|==|!=)\s*(-?\d+|MIN|MAX|MAX_VALUE|MIN_VALUE|Integer\.MAX_VALUE|Integer\.MIN_VALUE|\w+\.MAX|\w+\.MIN)").unwrap();
    let empty_check = regex::Regex::new(r#"(isEmpty|is_empty|\.len\(\)\s*==\s*0|==\s*"")"#).unwrap();
    let throw_re = regex::Regex::new(r"\b(throw|raise)\s+\w").unwrap();
    let err_return = regex::Regex::new(r"\breturn\s+(Err|Error|None|null|false)\b").unwrap();
    let null_usage = regex::Regex::new(r"\b(null|None|undefined|Optional\.|\.orElse|\.unwrap|\.unwrap_or|\?\?)").unwrap();
    let index_access = regex::Regex::new(r"\w+\s*\[\s*\w+\s*\]|\.get\(\s*\w+\s*\)").unwrap();

    for (i, raw_line) in source_snippet.lines().enumerate() {
        // Skip comments and annotation-only lines in the snippet
        let trimmed = raw_line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '#');
        if trimmed.trim_start().starts_with("//") || trimmed.trim_start().starts_with("#") {
            continue;
        }
        // The snippet lines are annotated like " 42:     if (x > 100) {"
        // Extract the actual code after the `:` for cleaner hint content.
        let code = match trimmed.find(':') {
            Some(idx) if idx < 6 => trimmed[idx + 1..].trim(),
            _ => trimmed.trim(),
        };
        if code.is_empty() { continue; }

        // Numeric comparison → boundary test
        if let Some(cap) = num_cmp.captures(code) {
            let op = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let val = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            hints.push(format!(
                "- Line {}: numeric comparison `{} {}` — test values at the boundary, one unit below, one unit above, plus 0 / MIN / MAX.",
                i + 1, op, val
            ));
            continue;
        }
        // Empty check
        if empty_check.is_match(code) {
            hints.push(format!(
                "- Line {}: empty-check pattern — test with empty, single-element, and populated inputs.",
                i + 1
            ));
            continue;
        }
        // Explicit throw / raise
        if throw_re.is_match(code) {
            hints.push(format!(
                "- Line {}: explicit throw/raise — generate a negative test that triggers this exception and verifies the exception type and message.",
                i + 1
            ));
            continue;
        }
        // Early error return
        if err_return.is_match(code) {
            hints.push(format!(
                "- Line {}: early-return error path — test that the function returns the error value under the triggering condition.",
                i + 1
            ));
            continue;
        }
        // Null/Option/undefined handling
        if null_usage.is_match(code) {
            hints.push(format!(
                "- Line {}: null/Optional/undefined usage — test with null (or None/Optional.empty) to verify defensive behaviour.",
                i + 1
            ));
            continue;
        }
        // Index access
        if index_access.is_match(code) {
            hints.push(format!(
                "- Line {}: index access — test index=0, index=last, and an out-of-bounds index (expect the documented exception or defensive return).",
                i + 1
            ));
            continue;
        }
    }

    // Deduplicate identical lines while keeping order
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    hints.retain(|h| seen.insert(h.clone()));

    // Cap at ~10 hints to keep the prompt bounded
    hints.truncate(10);
    hints.join("\n")
}

pub(crate) fn extract_uncovered_snippets(
    source_content: &str,
    uncovered_lines: &[u32],
    max_lines: usize,
) -> String {
    if uncovered_lines.is_empty() || source_content.is_empty() {
        return String::new();
    }

    let source_lines: Vec<&str> = source_content.lines().collect();
    let total = source_lines.len();

    // Group consecutive uncovered lines into ranges
    let mut groups: Vec<(u32, u32)> = Vec::new(); // (start, end) inclusive
    let mut lines_so_far = 0usize;
    for &line in uncovered_lines {
        if lines_so_far >= max_lines {
            break;
        }
        if let Some(last) = groups.last_mut() {
            if line <= last.1 + 3 {
                // Merge into current group (gap ≤ 2 lines)
                last.1 = line;
            } else {
                groups.push((line, line));
            }
        } else {
            groups.push((line, line));
        }
        lines_so_far += 1;
    }

    let mut out = String::new();
    for (start, end) in &groups {
        let s = (*start as usize).saturating_sub(1); // 0-indexed
        let e = (*end as usize).saturating_sub(1);

        // Context: 1 line before, 1 line after
        let ctx_start = s.saturating_sub(1);
        let ctx_end = (e + 1).min(total.saturating_sub(1));

        out.push_str(&format!("// Lines {}-{} (UNCOVERED):\n", start, end));
        for i in ctx_start..=ctx_end {
            if i >= total {
                break;
            }
            let lineno = i + 1; // 1-indexed
            let marker = if uncovered_lines.contains(&(lineno as u32)) { ">" } else { " " };
            out.push_str(&format!("{}{:>4}: {}\n", marker, lineno, source_lines[i]));
        }
        out.push('\n');
    }

    if lines_so_far < uncovered_lines.len() {
        out.push_str(&format!(
            "// … and {} more uncovered lines (will be addressed in subsequent rounds)\n",
            uncovered_lines.len() - lines_so_far
        ));
    }

    out
}

/// A chunk of uncovered code within a single method/function, ready for
/// targeted test generation.
#[derive(Debug, Clone)]
pub(crate) struct MethodChunk {
    /// Human-readable label (e.g. "processOrder (lines 45-78)")
    pub label: String,
    /// The uncovered line numbers in this chunk
    pub uncovered_lines: Vec<u32>,
    /// Annotated source snippet with `>` markers on uncovered lines
    pub snippet: String,
    /// Number of uncovered lines
    pub uncovered_count: usize,
}

/// Group method chunks into batches to reduce the number of AI calls.
///
/// Two cost drivers per chunk call:
///   - API round-trip overhead (fixed cost regardless of payload size)
///   - Output tokens (test code generated — roughly proportional to complexity)
///
/// Batching rules:
/// - A chunk is **solo** if it has >25 uncovered lines OR >100 snippet lines.
///   Large/complex methods need focused attention and produce enough output
///   that a dedicated call is justified.
/// - Small chunks are accumulated into a batch until adding the next one would:
///   - Push total uncovered lines over 30, OR
///   - Push total snippet lines over 120, OR
///   - Exceed 3 chunks in the batch.
/// - **Simple-method fast path**: when every chunk in the accumulating batch has
///   ≤5 uncovered lines AND ≤30 snippet lines (getters, trivial accessors, etc.),
///   the batch limits are raised to 8 chunks / 40 uncovered / 240 snippet lines.
///   This merges many tiny AI calls into one, sharing context and cutting cost.
///
/// This typically reduces 4-6 tiny-method calls to 1-2 calls while keeping
/// complex methods in dedicated calls where Claude can reason clearly.
pub(crate) fn group_chunks_into_batches(chunks: Vec<MethodChunk>) -> Vec<Vec<MethodChunk>> {
    const SOLO_UNCOVERED: usize = 25;
    const SOLO_SNIPPET_LINES: usize = 100;
    // Regular batch limits
    const MAX_BATCH_UNCOVERED: usize = 30;
    const MAX_BATCH_SNIPPET_LINES: usize = 120;
    const MAX_BATCH_SIZE: usize = 3;
    // Simple-method batch limits (getters / trivial accessors merged into one call)
    const SIMPLE_UNCOVERED: usize = 5;
    const SIMPLE_SNIPPET_LINES: usize = 30;
    const SIMPLE_MAX_BATCH_SIZE: usize = 8;
    const SIMPLE_MAX_BATCH_UNCOVERED: usize = 40; // 8 × 5
    const SIMPLE_MAX_BATCH_SNIPPET_LINES: usize = 240; // 8 × 30

    let mut batches: Vec<Vec<MethodChunk>> = Vec::new();
    let mut current_batch: Vec<MethodChunk> = Vec::new();
    let mut current_uncovered = 0usize;
    let mut current_snippet_lines = 0usize;
    // True while every chunk accumulated so far in the current batch is "simple"
    let mut batch_all_simple = true;

    for chunk in chunks {
        let chunk_snippet_lines = chunk.snippet.lines().count();
        let is_solo =
            chunk.uncovered_count > SOLO_UNCOVERED || chunk_snippet_lines > SOLO_SNIPPET_LINES;
        let chunk_is_simple =
            chunk.uncovered_count <= SIMPLE_UNCOVERED && chunk_snippet_lines <= SIMPLE_SNIPPET_LINES;

        if is_solo {
            // Flush any accumulated small chunks first, then push solo.
            if !current_batch.is_empty() {
                batches.push(std::mem::take(&mut current_batch));
                current_uncovered = 0;
                current_snippet_lines = 0;
                batch_all_simple = true;
            }
            batches.push(vec![chunk]);
        } else {
            // Use enlarged limits only when adding this chunk keeps the whole batch simple.
            let would_stay_simple = batch_all_simple && chunk_is_simple;
            let (max_size, max_uncov, max_snip) = if would_stay_simple {
                (SIMPLE_MAX_BATCH_SIZE, SIMPLE_MAX_BATCH_UNCOVERED, SIMPLE_MAX_BATCH_SNIPPET_LINES)
            } else {
                (MAX_BATCH_SIZE, MAX_BATCH_UNCOVERED, MAX_BATCH_SNIPPET_LINES)
            };

            let would_overflow = !current_batch.is_empty()
                && (current_uncovered + chunk.uncovered_count > max_uncov
                    || current_snippet_lines + chunk_snippet_lines > max_snip
                    || current_batch.len() >= max_size);

            if would_overflow {
                batches.push(std::mem::take(&mut current_batch));
                current_uncovered = 0;
                current_snippet_lines = 0;
                batch_all_simple = true;
            }

            batch_all_simple = batch_all_simple && chunk_is_simple;
            current_uncovered += chunk.uncovered_count;
            current_snippet_lines += chunk_snippet_lines;
            current_batch.push(chunk);
        }
    }

    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

/// Split uncovered lines into method-level chunks for targeted test generation.
///
/// Uses language-aware heuristics to find method/function boundaries, then groups
/// uncovered lines by the method they belong to.  Each chunk contains the full
/// method source (annotated) so the AI can write tests for one method at a time.
///
/// Falls back to contiguous-group splitting when method detection isn't applicable.
pub(crate) fn split_into_method_chunks(
    source_content: &str,
    uncovered_lines: &[u32],
    file_path: &str,
) -> Vec<MethodChunk> {
    if uncovered_lines.is_empty() || source_content.is_empty() {
        return Vec::new();
    }

    let source_lines: Vec<&str> = source_content.lines().collect();
    let total = source_lines.len();

    // Detect method boundaries based on file extension
    let methods = detect_method_boundaries(&source_lines, file_path);

    if methods.is_empty() {
        // US-058: log the fallback trigger so we can measure detector quality in the wild.
        tracing::debug!(
            "Method detection fell back to contiguous groups for {} — {} uncovered lines, {} source lines",
            file_path, uncovered_lines.len(), source_lines.len()
        );
        // Fallback: split into groups of ~20 contiguous uncovered lines
        return split_by_contiguous_groups(source_content, uncovered_lines, &source_lines, 20);
    }

    // Assign each uncovered line to its enclosing method
    let mut method_chunks: Vec<MethodChunk> = Vec::new();
    let mut unassigned: Vec<u32> = Vec::new();

    // Pre-build method lookup: for each uncovered line, find which method contains it
    for &line in uncovered_lines {
        let idx = line as usize;
        let mut found = false;
        for m in &methods {
            if idx >= m.start_line && idx <= m.end_line {
                // Find or create chunk for this method
                if let Some(chunk) = method_chunks.iter_mut().find(|c| c.label == m.name) {
                    chunk.uncovered_lines.push(line);
                    chunk.uncovered_count += 1;
                } else {
                    method_chunks.push(MethodChunk {
                        label: m.name.clone(),
                        uncovered_lines: vec![line],
                        snippet: String::new(), // filled below
                        uncovered_count: 1,
                    });
                }
                found = true;
                break;
            }
        }
        if !found {
            unassigned.push(line);
        }
    }

    // Build annotated snippets for each method chunk
    for chunk in &mut method_chunks {
        // Find the method boundaries again for context
        if let Some(m) = methods.iter().find(|m| m.name == chunk.label) {
            let start = m.start_line.saturating_sub(1); // 0-indexed
            let end = (m.end_line.saturating_sub(1)).min(total.saturating_sub(1));

            chunk.snippet.push_str(&format!("// Method: {} (lines {}-{})\n", m.name, m.start_line, m.end_line));
            for i in start..=end {
                if i >= total { break; }
                let lineno = (i + 1) as u32;
                let marker = if chunk.uncovered_lines.contains(&lineno) { ">" } else { " " };
                chunk.snippet.push_str(&format!("{}{:>4}: {}\n", marker, lineno, source_lines[i]));
            }
        }
    }

    // Handle lines outside any method (class-level code, field initializers)
    if !unassigned.is_empty() {
        let snippet = extract_uncovered_snippets(source_content, &unassigned, 40);
        method_chunks.push(MethodChunk {
            label: format!("class-level code ({} lines)", unassigned.len()),
            uncovered_lines: unassigned.clone(),
            snippet,
            uncovered_count: unassigned.len(),
        });
    }

    method_chunks
}

/// Compact an annotated method snippet when it exceeds `max_lines` (US-053).
///
/// Produces a focused view with:
/// - The method header comment ("// Method: name (lines X-Y)")
/// - The first 5 body lines (function signature and opening)
/// - For each uncovered line (marked `>`): a ±5 line context window
/// - The last body line (closing `}` or `end`)
/// - `// ... (N lines omitted — read file for full context)` gaps between sections
///
/// When `max_lines == 0` or the snippet is already within the limit, the original
/// snippet is returned unchanged.
pub(crate) fn compact_method_snippet(snippet: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return snippet.to_string();
    }

    let all_lines: Vec<&str> = snippet.lines().collect();
    if all_lines.len() <= max_lines {
        return snippet.to_string();
    }

    // Line 0 is the header "// Method: name (lines X-Y)"; body starts at index 1.
    let header = all_lines[0];
    let body = &all_lines[1..];
    let body_len = body.len();
    if body_len == 0 {
        return snippet.to_string();
    }

    // Collect indices of uncovered body lines (those starting with '>').
    let uncovered_body_indices: Vec<usize> = body.iter().enumerate()
        .filter(|(_, line)| line.starts_with('>'))
        .map(|(i, _)| i)
        .collect();

    // Build the set of body-line indices to include.
    let mut include: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

    // First 5 body lines (signature + opening brace)
    for i in 0..body_len.min(5) {
        include.insert(i);
    }
    // Last body line (closing brace / end)
    include.insert(body_len - 1);
    // ±5 context window around each uncovered line
    for &ui in &uncovered_body_indices {
        let lo = ui.saturating_sub(5);
        let hi = (ui + 5).min(body_len - 1);
        for j in lo..=hi {
            include.insert(j);
        }
    }

    // Build output with gap comments between non-contiguous sections.
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');

    let indices: Vec<usize> = include.into_iter().collect();
    let mut prev: Option<usize> = None;
    for &idx in &indices {
        if let Some(p) = prev {
            let gap = idx.saturating_sub(p + 1);
            if gap > 0 {
                out.push_str(&format!("// ... ({} lines omitted — read file for full context)\n", gap));
            }
        }
        out.push_str(body[idx]);
        out.push('\n');
        prev = Some(idx);
    }

    out
}

/// A detected method/function boundary in source code.
struct MethodBoundary {
    name: String,
    start_line: usize, // 1-indexed, inclusive
    end_line: usize,   // 1-indexed, inclusive
}

/// Find the enclosing method's (start_line, end_line) for a given 1-indexed line,
/// using language-aware method-boundary detection. Returns `None` if the line
/// is not inside any detected method (e.g. class-level code, or unsupported
/// language). The returned range is 1-indexed and inclusive, matching
/// SonarQube's line numbering.
pub(crate) fn enclosing_method_range(
    file_content: &str,
    file_path: &str,
    line: u32,
) -> Option<(u32, u32)> {
    let lines: Vec<&str> = file_content.lines().collect();
    let methods = detect_method_boundaries(&lines, file_path);
    let idx = line as usize;
    // Prefer the innermost (smallest) enclosing method in case of nested functions.
    methods
        .into_iter()
        .filter(|m| idx >= m.start_line && idx <= m.end_line)
        .min_by_key(|m| m.end_line - m.start_line)
        .map(|m| (m.start_line as u32, m.end_line as u32))
}

/// Detect method/function boundaries using language-aware heuristics.
fn detect_method_boundaries(lines: &[&str], file_path: &str) -> Vec<MethodBoundary> {
    let lower = file_path.to_lowercase();
    if lower.ends_with(".java") || lower.ends_with(".kt") || lower.ends_with(".scala") {
        detect_java_methods(lines)
    } else if lower.ends_with(".py") {
        detect_python_functions(lines)
    } else if lower.ends_with(".js") || lower.ends_with(".ts")
        || lower.ends_with(".jsx") || lower.ends_with(".tsx")
    {
        detect_js_functions(lines)
    } else if lower.ends_with(".go") {
        detect_go_functions(lines)
    } else if lower.ends_with(".rs") {
        detect_rust_functions(lines)
    } else {
        // Brace-based languages fallback
        detect_brace_functions(lines)
    }
}

/// US-058: Java/Kotlin/Scala method detection.
///
/// Detects methods (and Kotlin extension functions) with access modifiers, returning
/// their start/end line boundaries. Includes preceding annotation lines in the
/// boundary so the AI sees `@Transactional`, `@Override`, etc. in the snippet.
///
/// Handles:
/// - Java methods with nested generics: `Map<String, List<Integer>> foo()`
/// - Kotlin extension functions: `fun String.toSnakeCase()`
/// - Methods with multiple annotations on preceding lines
/// - Excludes class/interface/enum/record declarations
fn detect_java_methods(lines: &[&str]) -> Vec<MethodBoundary> {
    // Access modifier + permissive return type (generics with spaces OK) + name + `(`.
    // We allow anything up to the last `\w+\s*\(` on the line, which handles
    // `public Map<String, List<Order>> process()` correctly.
    let method_re = regex::Regex::new(
        r"^\s*(?:public|private|protected|static|final|abstract|synchronized|default|override)\b[^(;=]*?(\w+)\s*\("
    ).unwrap();
    // Kotlin: `fun [visibility?] [extensionReceiver.]methodName(` — no modifier required
    let kotlin_re = regex::Regex::new(
        r"^\s*(?:public\s+|private\s+|protected\s+|internal\s+)?(?:inline\s+|suspend\s+|operator\s+|override\s+|open\s+|tailrec\s+|external\s+|infix\s+)*fun\s+(?:<[^>]*>\s+)?(?:[\w.<>]+\.)?(\w+)\s*\("
    ).unwrap();
    let class_re = regex::Regex::new(
        r"\b(class|interface|enum|record|object|trait)\s+"
    ).unwrap();
    // Annotation line: `@Something` possibly with parens `@Something(x)`
    let annotation_re = regex::Regex::new(r"^\s*@\w").unwrap();

    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let caps = method_re.captures(lines[i]).or_else(|| kotlin_re.captures(lines[i]));
        if let Some(caps) = caps {
            // Skip class/interface/enum declarations
            if class_re.is_match(lines[i]) {
                i += 1;
                continue;
            }
            let name = caps.get(1).map(|m| m.as_str().to_string())
                .unwrap_or_else(|| format!("anonymous_{}", i + 1));

            // US-058: walk backwards to include preceding annotation/decorator lines.
            // Stops at the first non-annotation, non-blank line.
            let mut boundary_start = i;
            while boundary_start > 0 {
                let prev = lines[boundary_start - 1].trim();
                if prev.is_empty() || annotation_re.is_match(prev) {
                    boundary_start -= 1;
                } else {
                    break;
                }
            }
            let start = boundary_start + 1; // 1-indexed
            // Find balanced braces starting from the signature line
            let mut brace_depth = 0i32;
            let mut found_open = false;
            let mut j = i;
            while j < lines.len() {
                for ch in lines[j].chars() {
                    if ch == '{' { brace_depth += 1; found_open = true; }
                    else if ch == '}' { brace_depth -= 1; }
                }
                if found_open && brace_depth == 0 {
                    methods.push(MethodBoundary { name, start_line: start, end_line: j + 1 });
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if !found_open || brace_depth != 0 { i += 1; }
        } else {
            i += 1;
        }
    }
    methods
}

fn detect_go_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    let func_re = regex::Regex::new(r"^\s*func\s+(?:\([^)]+\)\s+)?(\w+)\s*\(").unwrap();
    detect_brace_delimited(lines, &func_re, 1)
}

fn detect_rust_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    let fn_re = regex::Regex::new(r"^\s*(pub\s+)?(async\s+)?fn\s+(\w+)").unwrap();
    detect_brace_delimited(lines, &fn_re, 3)
}

/// US-058: JS/TS function detection with async + arrow functions + method shorthand.
///
/// Handles:
/// - `function foo()` / `async function foo()` / `export function foo()`
/// - `const foo = () => { ... }` / `const foo = async (x) => { ... }`
/// - Class/object method shorthand: `foo() { ... }` / `async foo() { ... }`
/// - TypeScript decorators: `@Component` lines included in boundary
fn detect_js_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    // Three alternatives captured into 3 distinct groups so we can pick whichever matched:
    //   1) function foo / async function foo / export [async] function foo
    //   2) const/let/var foo = [async] ( ... ) => { OR = function ( ... ) {
    //   3) method shorthand foo() { ... } / async foo() { ... }
    let func_re = regex::Regex::new(
        r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*(\w+)|^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*(?::\s*[^=]+)?\s*=\s*(?:async\s+)?(?:function\s*\*?\s*\w*\s*)?\([^)]*\)\s*(?:=>|\{)|^\s*(?:public\s+|private\s+|protected\s+|readonly\s+|static\s+)*(?:async\s+|\*\s*)?(\w+)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::\s*[^{=]+)?\s*\{"
    ).unwrap();
    let decorator_re = regex::Regex::new(r"^\s*@\w").unwrap();

    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = func_re.captures(lines[i]) {
            let name = [1usize, 2, 3]
                .iter()
                .filter_map(|&g| caps.get(g).map(|m| m.as_str().to_string()))
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| format!("anonymous_{}", i + 1));

            // Exclude common keywords that might match group 3 (if, for, while, switch, catch)
            if matches!(name.as_str(), "if" | "for" | "while" | "switch" | "catch" | "return" | "throw" | "do") {
                i += 1;
                continue;
            }

            // Walk back to include decorators
            let mut boundary_start = i;
            while boundary_start > 0 {
                let prev = lines[boundary_start - 1].trim();
                if prev.is_empty() || decorator_re.is_match(prev) {
                    boundary_start -= 1;
                } else {
                    break;
                }
            }
            let start = boundary_start + 1;

            // Find balanced braces
            let mut brace_depth = 0i32;
            let mut found_open = false;
            let mut j = i;
            while j < lines.len() {
                for ch in lines[j].chars() {
                    if ch == '{' { brace_depth += 1; found_open = true; }
                    else if ch == '}' { brace_depth -= 1; }
                }
                if found_open && brace_depth == 0 {
                    methods.push(MethodBoundary {
                        name,
                        start_line: start,
                        end_line: j + 1,
                    });
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if !found_open || brace_depth != 0 { i += 1; }
        } else {
            i += 1;
        }
    }
    methods
}

/// Generic brace-delimited function detector.
fn detect_brace_delimited(lines: &[&str], signature_re: &regex::Regex, name_group: usize) -> Vec<MethodBoundary> {
    detect_brace_delimited_multi_capture(lines, signature_re, &[name_group])
}

fn detect_brace_delimited_multi_capture(
    lines: &[&str],
    signature_re: &regex::Regex,
    name_groups: &[usize],
) -> Vec<MethodBoundary> {
    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = signature_re.captures(lines[i]) {
            // Extract name from first non-empty capture group
            let name = name_groups.iter()
                .filter_map(|&g| caps.get(g).map(|m| m.as_str().to_string()))
                .next()
                .unwrap_or_else(|| format!("anonymous_{}", i + 1));

            let start = i + 1; // 1-indexed

            // Find the opening brace (might be on the same or next line)
            let mut brace_depth = 0i32;
            let mut found_open = false;
            let mut j = i;
            while j < lines.len() {
                for ch in lines[j].chars() {
                    if ch == '{' {
                        brace_depth += 1;
                        found_open = true;
                    } else if ch == '}' {
                        brace_depth -= 1;
                    }
                }
                if found_open && brace_depth == 0 {
                    methods.push(MethodBoundary {
                        name,
                        start_line: start,
                        end_line: j + 1, // 1-indexed
                    });
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if !found_open || brace_depth != 0 {
                // Couldn't find balanced braces, skip this match
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    methods
}

/// US-058: Python function/method detection with async + decorator support.
///
/// Handles:
/// - `def name(` and `async def name(`
/// - Decorators (`@pytest.fixture`, `@property`, etc.) — included in the boundary
/// - Multi-line docstrings (`"""..."""` / `'''...'''`) — indented code inside a
///   docstring no longer prematurely terminates the function body
/// - Inner functions — detected as independent boundaries with the outer function's name prefix
fn detect_python_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    let def_re = regex::Regex::new(r"^(\s*)(?:async\s+)?def\s+(\w+)\s*\(").unwrap();
    let decorator_re = regex::Regex::new(r"^\s*@\w").unwrap();

    // Helper: does this line contain triple-quote string delimiters that would
    // open/close a docstring? We only care about `"""` / `'''` at start-of-content.
    fn count_triple_quotes(line: &str) -> (u32, u32) {
        // (double_triple, single_triple)
        let d = line.matches("\"\"\"").count() as u32;
        let s = line.matches("'''").count() as u32;
        (d, s)
    }

    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = def_re.captures(lines[i]) {
            let indent = caps.get(1).map(|m| m.as_str().len()).unwrap_or(0);
            let name = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();

            // Walk backwards to include decorator lines
            let mut boundary_start = i;
            while boundary_start > 0 {
                let prev = lines[boundary_start - 1];
                if prev.trim().is_empty() || decorator_re.is_match(prev) {
                    boundary_start -= 1;
                } else {
                    break;
                }
            }
            let start = boundary_start + 1; // 1-indexed

            // Function body: lines with indent > function def indent, tracking
            // whether we're inside a triple-quoted docstring so dedented lines
            // there don't end the function prematurely.
            let mut end = i;
            let mut j = i + 1;
            let mut in_triple_double = false;
            let mut in_triple_single = false;
            while j < lines.len() {
                let line = lines[j];

                // Update docstring state based on triple-quote occurrences in this line.
                let (d, s) = count_triple_quotes(line);
                if d % 2 == 1 { in_triple_double = !in_triple_double; }
                if s % 2 == 1 { in_triple_single = !in_triple_single; }
                let in_docstring = in_triple_double || in_triple_single;

                if line.trim().is_empty() {
                    j += 1;
                    continue;
                }
                let line_indent = line.len() - line.trim_start().len();
                if in_docstring || line_indent > indent {
                    end = j;
                    j += 1;
                } else {
                    break;
                }
            }

            if end > i {
                methods.push(MethodBoundary {
                    name,
                    start_line: start,
                    end_line: end + 1, // 1-indexed
                });
            }
            // Advance by 1 (not `end + 1`) so nested functions inside this method
            // are discovered as separate boundaries.
            i += 1;
        } else {
            i += 1;
        }
    }
    methods
}

/// Fallback for unknown brace-based languages: detect top-level brace blocks.
fn detect_brace_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    // Very conservative: only detect if the line before `{` looks like a function
    let generic_re = regex::Regex::new(r"^\s*(?:\w+\s+)*(\w+)\s*\(").unwrap();
    detect_brace_delimited(lines, &generic_re, 1)
}

/// Fallback when no method boundaries are detected: split uncovered lines into
/// contiguous groups of up to `max_per_chunk` uncovered lines.
fn split_by_contiguous_groups(
    _source_content: &str,
    uncovered_lines: &[u32],
    source_lines: &[&str],
    max_per_chunk: usize,
) -> Vec<MethodChunk> {
    let total = source_lines.len();

    // Group consecutive lines (gap ≤ 3 merges into same group)
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for &line in uncovered_lines {
        if let Some(last) = groups.last_mut() {
            if line <= *last.last().unwrap() + 3 {
                last.push(line);
            } else {
                groups.push(vec![line]);
            }
        } else {
            groups.push(vec![line]);
        }
    }

    // Split groups that exceed max_per_chunk
    let mut final_groups: Vec<Vec<u32>> = Vec::new();
    for group in groups {
        if group.len() <= max_per_chunk {
            final_groups.push(group);
        } else {
            for chunk in group.chunks(max_per_chunk) {
                final_groups.push(chunk.to_vec());
            }
        }
    }

    // Build chunks with extended context (±3 lines)
    final_groups.into_iter().map(|lines| {
        let first = *lines.first().unwrap() as usize;
        let last_line = *lines.last().unwrap() as usize;
        let ctx_start = first.saturating_sub(1).saturating_sub(3); // 0-indexed, 3 lines before
        let ctx_end = (last_line.saturating_sub(1) + 3).min(total.saturating_sub(1));

        let mut snippet = format!("// Lines {}-{} ({} uncovered):\n", first, last_line, lines.len());
        for i in ctx_start..=ctx_end {
            if i >= total { break; }
            let lineno = (i + 1) as u32;
            let marker = if lines.contains(&lineno) { ">" } else { " " };
            snippet.push_str(&format!("{}{:>4}: {}\n", marker, lineno, source_lines[i]));
        }

        MethodChunk {
            label: format!("lines {}-{}", first, last_line),
            uncovered_lines: lines.clone(),
            snippet,
            uncovered_count: lines.len(),
        }
    }).collect()
}

/// Files that cannot have unit test coverage (style, templates, assets).
/// These should skip coverage checks and test generation.
pub(crate) fn is_non_coverable_file(path: &str) -> bool {
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
pub(crate) fn is_generated_artifact(path: &str) -> bool {
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

pub(crate) fn is_internal_file(path: &str) -> bool {
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
pub(crate) fn is_protected_file(path: &str, protected_files: &[String]) -> bool {
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
pub(crate) fn resolve_source_file(project_path: &Path, relative_file: &str) -> PathBuf {
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
    // e.g., example-app/src/main/java/com/... when --path points to parent
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

pub(crate) fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("test")
        || lower.contains("spec")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

/// Truncate keeping the **tail** of the string — useful for build/test output
/// where errors appear at the end.
pub(crate) fn truncate_tail(s: &str, max: usize) -> String {
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
pub(crate) fn format_commit_message(
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

// ANSI color helpers for terminal output
/// Check if stderr is a terminal (supports ANSI colors)
pub(crate) fn supports_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// ANSI color helpers — only emit escape codes when stderr is a real terminal.
pub(crate) fn colored(s: &str, code: &str) -> String {
    if supports_color() { format!("\x1b[{}m{}\x1b[0m", code, s) } else { s.to_string() }
}
pub(crate) fn green(s: &str) -> String { colored(s, "1;32") }
pub(crate) fn yellow(s: &str) -> String { colored(s, "1;33") }
pub(crate) fn red(s: &str) -> String { colored(s, "1;31") }
pub(crate) fn blue(s: &str) -> String { colored(s, "34") }

/// Color a coverage percentage based on how close it is to the threshold.
/// - Green + bold: at or above threshold
/// - Yellow + bold: within 10% of threshold
/// - Red + bold: more than 10% below threshold
pub(crate) fn cov_colored(pct: f64, threshold: f64) -> String {
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
pub(crate) fn cov_prev(pct: f64) -> String { blue(&format!("{:.1}%", pct)) }
/// Format a coverage percentage colored by distance to threshold.
/// Green if met, yellow if within 10%, red if > 10% below.
pub(crate) fn cov_vs(pct: f64, threshold: f64) -> String { cov_colored(pct, threshold) }

/// Print a colored info line directly to stderr, bypassing tracing's escaping.
/// Falls back to plain text when piped.
macro_rules! color_info {
    ($($arg:tt)*) => {
        eprintln!("{}", format!($($arg)*));
    };
}
pub(crate) use color_info;

#[cfg(test)]
mod tests {
    use super::*;

    // -- enclosing_method_range --

    #[test]
    fn enclosing_method_range_java_picks_method_not_file_end() {
        let src = "\
public class Foo {
    public void a() {
        int x = 1;
        int y = 2;
    }

    public void b() {
        int z = 3;
    }

    public void c() {
        int w = 4;
    }
}
";
        // Line 3 is inside a() — should return (2, 5), not (2, 15).
        let range = enclosing_method_range(src, "Foo.java", 3);
        assert_eq!(range, Some((2, 5)));
    }

    #[test]
    fn enclosing_method_range_returns_none_for_class_level_line() {
        let src = "\
public class Foo {
    public void a() {
        int x = 1;
    }
}
";
        // Line 1 is the class declaration — no enclosing method.
        assert_eq!(enclosing_method_range(src, "Foo.java", 1), None);
    }

    // -- rule_is_coverage_dependent --

    #[test]
    fn rule_coverage_dependent_sonar_common() {
        assert!(rule_is_coverage_dependent("common-java:InsufficientLineCoverage"));
        assert!(rule_is_coverage_dependent("common-java:InsufficientBranchCoverage"));
        assert!(rule_is_coverage_dependent("common-js:InsufficientLineCoverage"));
    }

    #[test]
    fn rule_coverage_dependent_generic_coverage_keyword() {
        assert!(rule_is_coverage_dependent("custom:UncoveredLines"));
        assert!(rule_is_coverage_dependent("some:new_coverage_rule"));
    }

    #[test]
    fn rule_not_coverage_dependent_vulnerability() {
        assert!(!rule_is_coverage_dependent("java:S5542"));        // weak crypto
        assert!(!rule_is_coverage_dependent("java:S2589"));        // dead code
        assert!(!rule_is_coverage_dependent("python:S1481"));      // unused local
        assert!(!rule_is_coverage_dependent("javascript:S1481"));
    }

    #[test]
    fn rule_not_coverage_dependent_duplication() {
        // Duplication is a metric but not coverage — regen not required for rescan
        assert!(!rule_is_coverage_dependent("common-java:DuplicatedBlocks"));
    }

    #[test]
    fn rule_linter_is_static_not_coverage_dependent() {
        assert!(!rule_is_coverage_dependent("lint:clippy:unused_imports"));
        assert!(!rule_is_coverage_dependent("lint:eslint:no-unused-vars"));
        assert!(!rule_is_coverage_dependent("lint:ruff:F401"));
    }

    // -- merge_lint_and_sonar_issues --

    fn fake_issue(key: &str, rule: &str, severity: &str) -> sonar::Issue {
        sonar::Issue {
            key: key.to_string(),
            rule: rule.to_string(),
            severity: severity.to_string(),
            component: "proj:src/x".to_string(),
            issue_type: "CODE_SMELL".to_string(),
            message: String::new(),
            text_range: None,
            status: "OPEN".to_string(),
            tags: vec![],
        }
    }

    #[test]
    fn merge_orders_by_severity_lint_first() {
        let lint = vec![
            fake_issue("L1", "lint:clippy:a", "MAJOR"),
            fake_issue("L2", "lint:clippy:b", "BLOCKER"),
        ];
        let sonar_is = vec![
            fake_issue("S1", "java:S1", "MAJOR"),
            fake_issue("S2", "java:S2", "BLOCKER"),
        ];
        let merged = merge_lint_and_sonar_issues(lint, sonar_is, false);
        let keys: Vec<&str> = merged.iter().map(|i| i.key.as_str()).collect();
        // BLOCKER bucket: L2 before S2; then MAJOR bucket: L1 before S1
        assert_eq!(keys, vec!["L2", "S2", "L1", "S1"]);
    }

    #[test]
    fn merge_handles_empty_lists() {
        let only_lint = vec![fake_issue("L1", "lint:r", "MAJOR")];
        let merged = merge_lint_and_sonar_issues(only_lint.clone(), vec![], false);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].key, "L1");

        let only_sonar = vec![fake_issue("S1", "java:r", "MAJOR")];
        let merged2 = merge_lint_and_sonar_issues(vec![], only_sonar, false);
        assert_eq!(merged2[0].key, "S1");
    }

    #[test]
    fn merge_reverse_severity_flips_order() {
        let lint = vec![fake_issue("L_INFO", "lint:x", "INFO")];
        let sonar_is = vec![fake_issue("S_BLOCKER", "java:y", "BLOCKER")];
        let merged = merge_lint_and_sonar_issues(lint, sonar_is, true);
        assert_eq!(merged[0].key, "L_INFO");
        assert_eq!(merged[1].key, "S_BLOCKER");
    }

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

    // -- build_slim_framework_context (US-056) --

    #[test]
    fn test_build_slim_framework_context_empty() {
        let tg = crate::config::TestGenerationConfig::default();
        assert!(build_slim_framework_context(&tg).is_empty());
    }

    #[test]
    fn test_build_slim_framework_context_avoids_spring() {
        let tg = crate::config::TestGenerationConfig {
            avoid_spring_context: true,
            ..Default::default()
        };
        let ctx = build_slim_framework_context(&tg);
        assert!(ctx.contains("MockitoExtension"));
        // Must NOT include auto-detected dep info (the "Detected test dependencies: ..." prefix)
        assert!(!ctx.contains("Detected test dependencies"));
        // Must NOT include library-specific declarations (framework/mock/assertion_library fields)
        assert!(!ctx.contains("Test framework:"));
        assert!(!ctx.contains("Mock framework:"));
    }

    #[test]
    fn test_build_slim_framework_context_custom_instructions_only() {
        let tg = crate::config::TestGenerationConfig {
            custom_instructions: Some("Use AssertJ fluent assertions.".to_string()),
            ..Default::default()
        };
        let ctx = build_slim_framework_context(&tg);
        assert!(ctx.contains("AssertJ fluent assertions"));
        assert!(!ctx.contains("Detected test dependencies"));
    }

    // -- capture_diff_summary (US-021) --

    #[test]
    fn test_capture_diff_summary_per_file_format() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Init a git repo with a source file
        std::process::Command::new("git").args(["init"]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["config", "user.name", "Test"]).current_dir(dir).output().unwrap();

        std::fs::write(dir.join("src.py"), "def hello():\n    pass\n").unwrap();
        std::process::Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["commit", "-m", "init"]).current_dir(dir).output().unwrap();

        // Modify source and add a test file
        std::fs::write(dir.join("src.py"), "def hello():\n    return 42\n").unwrap();
        std::fs::write(dir.join("test_src.py"), "def test_hello():\n    assert True\n").unwrap();
        std::process::Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["commit", "-m", "fix"]).current_dir(dir).output().unwrap();

        let summary = capture_diff_summary(dir).unwrap();

        // Should contain per-file details block for source file
        assert!(summary.contains("<details>"));
        assert!(summary.contains("<summary>src.py"));
        assert!(summary.contains("```diff"));

        // Test file should be excluded
        assert!(!summary.contains("test_src.py"));
    }

    #[test]
    fn test_capture_diff_summary_empty_when_no_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        std::process::Command::new("git").args(["init"]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["config", "user.email", "test@test.com"]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["config", "user.name", "Test"]).current_dir(dir).output().unwrap();

        std::fs::write(dir.join("src.py"), "pass\n").unwrap();
        std::process::Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["commit", "-m", "init"]).current_dir(dir).output().unwrap();

        // Only add a test file (no source changes)
        std::fs::write(dir.join("test_foo.py"), "assert True\n").unwrap();
        std::process::Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
        std::process::Command::new("git").args(["commit", "-m", "test only"]).current_dir(dir).output().unwrap();

        // All changed files are test files → should return None
        let summary = capture_diff_summary(dir);
        assert!(summary.is_none());
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

    // -- extract_uncovered_snippets --

    #[test]
    fn test_extract_uncovered_snippets_basic() {
        let source = "line1\nline2\nline3\nline4\nline5\n";
        let uncovered = vec![3];
        let result = extract_uncovered_snippets(source, &uncovered, 80);
        assert!(result.contains("Lines 3-3 (UNCOVERED)"));
        assert!(result.contains(">   3: line3"));
        // Context lines
        assert!(result.contains("    2: line2"));
        assert!(result.contains("    4: line4"));
    }

    #[test]
    fn test_extract_uncovered_snippets_groups_consecutive() {
        let source = "a\nb\nc\nd\ne\nf\ng\n";
        let uncovered = vec![2, 3, 4];
        let result = extract_uncovered_snippets(source, &uncovered, 80);
        // Should be a single group
        assert!(result.contains("Lines 2-4 (UNCOVERED)"));
        assert!(result.contains(">   2: b"));
        assert!(result.contains(">   3: c"));
        assert!(result.contains(">   4: d"));
    }

    #[test]
    fn test_extract_uncovered_snippets_separate_groups() {
        let source = (1..=20).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
        let uncovered = vec![2, 10]; // far apart → separate groups
        let result = extract_uncovered_snippets(&source, &uncovered, 80);
        assert!(result.contains("Lines 2-2 (UNCOVERED)"));
        assert!(result.contains("Lines 10-10 (UNCOVERED)"));
    }

    #[test]
    fn test_extract_uncovered_snippets_max_lines_cap() {
        let source = (1..=100).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
        let uncovered: Vec<u32> = (1..=100).collect();
        let result = extract_uncovered_snippets(&source, &uncovered, 5);
        assert!(result.contains("and 95 more uncovered lines"));
    }

    #[test]
    fn test_extract_uncovered_snippets_empty_inputs() {
        assert!(extract_uncovered_snippets("", &[1, 2], 80).is_empty());
        assert!(extract_uncovered_snippets("line1\n", &[], 80).is_empty());
    }

    // -- detect_boundary_hints (US-067) --

    #[test]
    fn detect_boundary_hints_numeric_comparison() {
        let snippet = "// Lines 42-42 (UNCOVERED):\n>  42:     if (value > 100) { return; }\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.contains("numeric comparison"), "got: {}", hints);
        assert!(hints.contains("> 100"));
    }

    #[test]
    fn detect_boundary_hints_throw_statement() {
        let snippet = "// Lines 5-5 (UNCOVERED):\n>   5:     throw new IllegalArgumentException(\"bad\");\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.contains("explicit throw"), "got: {}", hints);
    }

    #[test]
    fn detect_boundary_hints_empty_check() {
        let snippet = "// Lines 10-10 (UNCOVERED):\n>  10:     if (list.isEmpty()) return null;\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.contains("empty-check"), "got: {}", hints);
    }

    #[test]
    fn detect_boundary_hints_null_usage() {
        let snippet = "// Lines 20-20 (UNCOVERED):\n>  20:     if (user == null) return;\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.contains("null"), "got: {}", hints);
    }

    #[test]
    fn detect_boundary_hints_index_access() {
        let snippet = "// Lines 30-30 (UNCOVERED):\n>  30:     return items.get(idx);\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.contains("index access"), "got: {}", hints);
    }

    #[test]
    fn detect_boundary_hints_empty_input_returns_empty() {
        let hints = detect_boundary_hints("");
        assert!(hints.is_empty());
    }

    #[test]
    fn detect_boundary_hints_trivial_code_returns_empty() {
        // Pure assignments shouldn't trigger any hint
        let snippet = "// Lines 1-1:\n>   1:     let x = other;\n";
        let hints = detect_boundary_hints(snippet);
        assert!(hints.is_empty(), "got: {}", hints);
    }

    #[test]
    fn detect_boundary_hints_deduplicates() {
        // Two identical lines shouldn't produce two identical hints
        let snippet = "\
>   1:     if (x > 10) return;
>   2:     if (x > 10) return;
";
        let hints = detect_boundary_hints(snippet);
        // Both hints are "Line N: numeric..." with different line numbers so
        // they're distinct — this verifies dedup doesn't eat distinct lines.
        assert_eq!(hints.matches("numeric comparison").count(), 2);
    }

    #[test]
    fn detect_boundary_hints_capped_at_10() {
        // Generate 15 uncovered lines with comparisons
        let mut snippet = String::new();
        for i in 1..=15 {
            snippet.push_str(&format!(">  {}:     if (a{} > 5) return;\n", i, i));
        }
        let hints = detect_boundary_hints(&snippet);
        assert_eq!(hints.lines().count(), 10, "should be capped at 10");
    }

    // -- compact_method_snippet (US-053) --

    #[test]
    fn test_compact_method_snippet_short_unchanged() {
        let snippet = "// Method: foo (lines 1-5)\n 1: fn foo() {\n 2:     let x = 1;\n>3:     x\n 4: }\n";
        let result = compact_method_snippet(snippet, 80);
        assert_eq!(result, snippet, "Short snippet should not be modified");
    }

    #[test]
    fn test_compact_method_snippet_zero_max_unchanged() {
        let body = (1..=100).map(|i| format!(" {:>4}: line_{}\n", i, i)).collect::<String>();
        let snippet = format!("// Method: big (lines 1-100)\n{}", body);
        let result = compact_method_snippet(&snippet, 0);
        assert_eq!(result, snippet, "max_lines=0 should always return full snippet");
    }

    #[test]
    fn test_compact_method_snippet_large_method_is_smaller() {
        // Build a 120-line method with uncovered lines at 60 and 61
        let mut body = String::new();
        for i in 1..=120usize {
            let marker = if i == 60 || i == 61 { ">" } else { " " };
            body.push_str(&format!("{}{:>4}: line_{}\n", marker, i, i));
        }
        let snippet = format!("// Method: big (lines 1-120)\n{}", body);

        let result = compact_method_snippet(&snippet, 80);
        let original_lines = snippet.lines().count();
        let result_lines = result.lines().count();

        assert!(result_lines < original_lines, "Compact result ({} lines) should be shorter than original ({} lines)", result_lines, original_lines);
        // Should include the uncovered lines
        assert!(result.contains(">  60: line_60"), "Should include uncovered line 60");
        assert!(result.contains(">  61: line_61"), "Should include uncovered line 61");
        // Should include the header
        assert!(result.contains("// Method: big"), "Should include header");
        // Should include a gap comment
        assert!(result.contains("lines omitted"), "Should include omission comment");
    }

    #[test]
    fn test_compact_method_snippet_preserves_closing_brace() {
        // Build a 100-line method; uncovered line at line 20 only
        let mut body = String::new();
        for i in 1..=100usize {
            let marker = if i == 20 { ">" } else { " " };
            body.push_str(&format!("{}{:>4}: line_{}\n", marker, i, i));
        }
        let snippet = format!("// Method: m (lines 1-100)\n{}", body);
        let result = compact_method_snippet(&snippet, 80);
        // Last body line (line_100) must be preserved as the closing brace equivalent
        assert!(result.contains("line_100"), "Should preserve the last body line");
    }

    // -- split_into_method_chunks / method detection --

    #[test]
    fn test_split_java_methods() {
        let java_src = r#"package com.example;

public class Calc {
    public int add(int a, int b) {
        return a + b;
    }

    private int multiply(int a, int b) {
        int result = a * b;
        return result;
    }
}"#;
        // Lines 4-6 = add, lines 8-11 = multiply
        let uncovered = vec![5, 9, 10]; // return a+b, int result, return result
        let chunks = split_into_method_chunks(java_src, &uncovered, "Calc.java");
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].label == "add" || chunks[1].label == "add");
        assert!(chunks[0].label == "multiply" || chunks[1].label == "multiply");
        // Each chunk should have the right uncovered lines
        let add_chunk = chunks.iter().find(|c| c.label == "add").unwrap();
        assert_eq!(add_chunk.uncovered_count, 1);
        assert!(add_chunk.snippet.contains("return a + b"));
        let mul_chunk = chunks.iter().find(|c| c.label == "multiply").unwrap();
        assert_eq!(mul_chunk.uncovered_count, 2);
        assert!(mul_chunk.snippet.contains("int result"));
    }

    #[test]
    fn test_split_python_functions() {
        let py_src = "def foo():\n    x = 1\n    return x\n\ndef bar():\n    y = 2\n    return y\n";
        let uncovered = vec![2, 6]; // x = 1, y = 2
        let chunks = split_into_method_chunks(py_src, &uncovered, "module.py");
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().any(|c| c.label == "foo"));
        assert!(chunks.iter().any(|c| c.label == "bar"));
    }

    #[test]
    fn test_split_few_lines_returns_single_fallback() {
        let src = "a\nb\nc\nd\ne\n";
        // Only 2 lines → below threshold, but split_into_method_chunks
        // doesn't enforce the threshold (caller does).
        // With no detected methods, falls back to contiguous groups.
        let uncovered = vec![2, 3];
        let chunks = split_into_method_chunks(src, &uncovered, "unknown.txt");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].uncovered_count, 2);
    }

    #[test]
    fn test_split_go_functions() {
        let go_src = "package main\n\nfunc Add(a, b int) int {\n\treturn a + b\n}\n\nfunc Sub(a, b int) int {\n\treturn a - b\n}\n";
        let uncovered = vec![4, 8];
        let chunks = split_into_method_chunks(go_src, &uncovered, "main.go");
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().any(|c| c.label == "Add"));
        assert!(chunks.iter().any(|c| c.label == "Sub"));
    }

    #[test]
    fn test_split_js_functions() {
        let js_src = "function greet(name) {\n  return `Hello ${name}`;\n}\n\nconst add = (a, b) => {\n  return a + b;\n};\n";
        let uncovered = vec![2, 6];
        let chunks = split_into_method_chunks(js_src, &uncovered, "util.js");
        assert!(chunks.len() >= 1); // At least greet should be detected
        assert!(chunks.iter().any(|c| c.label == "greet"));
    }

    #[test]
    fn test_split_unassigned_lines_go_to_class_level() {
        // When there ARE methods but some lines fall outside them → class-level chunk
        let java_src = r#"package com.example;

public class Foo {
    private int x = 42;

    public int getX() {
        return x;
    }
}"#;
        let uncovered = vec![4, 7]; // field init (class-level) + return x (inside getX)
        let chunks = split_into_method_chunks(java_src, &uncovered, "Foo.java");
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().any(|c| c.label == "getX"));
        assert!(chunks.iter().any(|c| c.label.contains("class-level")));
    }

    #[test]
    fn test_split_no_methods_falls_back_to_groups() {
        // No detectable methods → contiguous group fallback
        let java_src = "package com.example;\n\npublic class Foo {\n    private int x = 42;\n}\n";
        let uncovered = vec![4];
        let chunks = split_into_method_chunks(java_src, &uncovered, "Foo.java");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].label.contains("lines"));
    }

    // -- US-058: robustness improvements to method detector --

    #[test]
    fn test_java_detects_nested_generics_with_spaces() {
        let java_src = "\
public class Service {
    public Map<String, List<Order>> processAll(List<Request> reqs) {
        return reqs.stream()
            .map(this::process)
            .collect(Collectors.toMap(Order::getId, Function.identity()));
    }
}
";
        let uncovered = vec![3];
        let chunks = split_into_method_chunks(java_src, &uncovered, "Service.java");
        // Should find `processAll` via method detector, NOT fall back
        assert!(!chunks.is_empty());
        // Fallback label format is "Lines X-Y (N uncovered)", so if we see a method name we're good.
        let has_method_label = chunks.iter().any(|c| c.label == "processAll");
        assert!(has_method_label, "Expected to detect processAll method, got labels: {:?}",
            chunks.iter().map(|c| &c.label).collect::<Vec<_>>());
    }

    #[test]
    fn test_java_includes_annotations_in_boundary() {
        // Annotation line 3, method signature line 4, uncovered line 5 inside method.
        let java_src = "\
public class S {

    @Override
    public void foo() {
        bar();
    }
}
";
        let uncovered = vec![5];
        let chunks = split_into_method_chunks(java_src, &uncovered, "S.java");
        assert!(!chunks.is_empty());
        let foo_chunk = chunks.iter().find(|c| c.label == "foo").expect("foo not found");
        // The annotation line 3 should appear in the snippet
        assert!(foo_chunk.snippet.contains("@Override"),
            "Expected @Override annotation in snippet, got:\n{}", foo_chunk.snippet);
    }

    #[test]
    fn test_kotlin_extension_function() {
        let kt_src = "\
fun String.toSnakeCase(): String {
    return this
        .replace(Regex(\"([a-z])([A-Z])\"), \"$1_$2\")
        .lowercase()
}
";
        let uncovered = vec![3];
        let chunks = split_into_method_chunks(kt_src, &uncovered, "StringExt.kt");
        let has = chunks.iter().any(|c| c.label == "toSnakeCase");
        assert!(has, "Expected toSnakeCase detected, got: {:?}",
            chunks.iter().map(|c| &c.label).collect::<Vec<_>>());
    }

    #[test]
    fn test_python_async_def() {
        let py_src = "\
async def fetch_data(url):
    async with session.get(url) as resp:
        return await resp.json()
";
        let uncovered = vec![3];
        let chunks = split_into_method_chunks(py_src, &uncovered, "client.py");
        let has = chunks.iter().any(|c| c.label == "fetch_data");
        assert!(has);
    }

    #[test]
    fn test_python_decorator_included() {
        let py_src = "\
@pytest.fixture
def sample_data():
    return {'key': 'value'}
";
        let uncovered = vec![3];
        let chunks = split_into_method_chunks(py_src, &uncovered, "test_foo.py");
        let fc = chunks.iter().find(|c| c.label == "sample_data").expect("function not found");
        assert!(fc.snippet.contains("@pytest.fixture"));
    }

    #[test]
    fn test_python_docstring_does_not_end_function() {
        let py_src = "\
def foo():
    \"\"\"
    Example:
x = 1
    \"\"\"
    return 42
";
        let uncovered = vec![6];
        let chunks = split_into_method_chunks(py_src, &uncovered, "mod.py");
        let fc = chunks.iter().find(|c| c.label == "foo").expect("foo not found");
        // The `return 42` line must be within foo's boundary
        assert!(fc.snippet.contains("return 42"),
            "Expected 'return 42' in foo snippet, got:\n{}", fc.snippet);
    }

    #[test]
    fn test_js_arrow_function_with_const() {
        let js_src = "\
export const handleClick = async (e) => {
    await processEvent(e);
    return true;
};
";
        let uncovered = vec![2];
        let chunks = split_into_method_chunks(js_src, &uncovered, "handler.ts");
        let has = chunks.iter().any(|c| c.label == "handleClick");
        assert!(has, "Expected handleClick detected, got: {:?}",
            chunks.iter().map(|c| &c.label).collect::<Vec<_>>());
    }

    #[test]
    fn test_js_class_method_shorthand() {
        let ts_src = "\
class Service {
    async fetchUser(id: string): Promise<User> {
        return this.client.get(id);
    }
}
";
        let uncovered = vec![3];
        let chunks = split_into_method_chunks(ts_src, &uncovered, "service.ts");
        let has = chunks.iter().any(|c| c.label == "fetchUser");
        assert!(has, "Expected fetchUser detected, got: {:?}",
            chunks.iter().map(|c| &c.label).collect::<Vec<_>>());
    }

    // -- group_chunks_into_batches --

    fn make_chunk(label: &str, uncovered_count: usize, snippet_lines: usize) -> MethodChunk {
        let snippet = (0..snippet_lines).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        MethodChunk {
            label: label.to_string(),
            uncovered_lines: (1..=uncovered_count as u32).collect(),
            snippet,
            uncovered_count,
        }
    }

    #[test]
    fn test_group_chunks_empty() {
        assert!(group_chunks_into_batches(vec![]).is_empty());
    }

    #[test]
    fn test_group_chunks_single_small() {
        // A single small chunk → one batch of one
        let batches = group_chunks_into_batches(vec![make_chunk("a", 5, 30)]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_group_chunks_large_solo() {
        // A chunk with >25 uncovered lines → its own batch
        let batches = group_chunks_into_batches(vec![make_chunk("big", 30, 50)]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0][0].label, "big");
    }

    #[test]
    fn test_group_chunks_large_snippet_solo() {
        // A chunk with >100 snippet lines → solo even if few uncovered lines
        let batches = group_chunks_into_batches(vec![make_chunk("wide", 5, 110)]);
        assert_eq!(batches.len(), 1);
    }

    #[test]
    fn test_group_chunks_two_small_batched_together() {
        // Two small chunks that fit within the budget → single batch
        let chunks = vec![make_chunk("a", 5, 30), make_chunk("b", 5, 30)];
        let batches = group_chunks_into_batches(chunks);
        assert_eq!(batches.len(), 1, "two small chunks should be one batch");
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_group_chunks_simple_merged_into_one_batch() {
        // Four simple chunks (≤5 uncovered, ≤30 snippet) → all merged into one batch
        // (simple-method fast path raises the batch-size limit to 8).
        let chunks = vec![
            make_chunk("a", 3, 10),
            make_chunk("b", 3, 10),
            make_chunk("c", 3, 10),
            make_chunk("d", 3, 10),
        ];
        let batches = group_chunks_into_batches(chunks);
        assert_eq!(batches.len(), 1, "four simple chunks should be one batch");
        assert_eq!(batches[0].len(), 4);
    }

    #[test]
    fn test_group_chunks_regular_max_three_per_batch() {
        // Four moderate chunks (>5 uncovered, not simple) → first three batched, fourth starts new.
        let chunks = vec![
            make_chunk("a", 8, 40),
            make_chunk("b", 8, 40),
            make_chunk("c", 8, 40),
            make_chunk("d", 8, 40),
        ];
        let batches = group_chunks_into_batches(chunks);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_group_chunks_overflow_uncovered_splits_batch() {
        // Two chunks whose combined uncovered lines exceed MAX_BATCH_UNCOVERED (30) → two batches
        let chunks = vec![make_chunk("a", 20, 20), make_chunk("b", 20, 20)];
        let batches = group_chunks_into_batches(chunks);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_group_chunks_solo_flushes_pending_small() {
        // A pending small batch should be flushed before a solo large chunk
        let chunks = vec![
            make_chunk("small", 5, 20),
            make_chunk("big",   30, 50),
        ];
        let batches = group_chunks_into_batches(chunks);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].label, "small");
        assert_eq!(batches[1][0].label, "big");
    }
}
