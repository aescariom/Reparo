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

/// A detected method/function boundary in source code.
struct MethodBoundary {
    name: String,
    start_line: usize, // 1-indexed, inclusive
    end_line: usize,   // 1-indexed, inclusive
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

/// Java/Kotlin: method = line with access modifier + return type + name + `(`, ending at balanced `}`.
/// Excludes class/interface/enum declarations.
fn detect_java_methods(lines: &[&str]) -> Vec<MethodBoundary> {
    let method_re = regex::Regex::new(
        r"^\s*(public|private|protected|static|final|abstract|synchronized|default|override\s)[\s\w<>\[\],?]*\s+(\w+)\s*\("
    ).unwrap();
    let class_re = regex::Regex::new(
        r"\b(class|interface|enum|record)\s+"
    ).unwrap();
    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = method_re.captures(lines[i]) {
            // Skip class/interface/enum declarations
            if class_re.is_match(lines[i]) {
                i += 1;
                continue;
            }
            let name = caps.get(2).map(|m| m.as_str().to_string())
                .unwrap_or_else(|| format!("anonymous_{}", i + 1));
            let start = i + 1;
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

fn detect_js_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    let func_re = regex::Regex::new(
        r"(?:^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)|^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s+)?\(?|^\s*(?:async\s+)?(\w+)\s*\([^)]*\)\s*\{)"
    ).unwrap();
    detect_brace_delimited_multi_capture(lines, &func_re, &[1, 2, 3])
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

/// Python: function = `def name(` with indentation-based end detection.
fn detect_python_functions(lines: &[&str]) -> Vec<MethodBoundary> {
    let def_re = regex::Regex::new(r"^(\s*)def\s+(\w+)\s*\(").unwrap();
    let mut methods = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = def_re.captures(lines[i]) {
            let indent = caps.get(1).map(|m| m.as_str().len()).unwrap_or(0);
            let name = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let start = i + 1; // 1-indexed

            // Function body: all subsequent lines with indent > function def indent,
            // or blank lines within the body
            let mut end = i;
            let mut j = i + 1;
            while j < lines.len() {
                let line = lines[j];
                if line.trim().is_empty() {
                    // Blank lines are part of the body
                    j += 1;
                    continue;
                }
                let line_indent = line.len() - line.trim_start().len();
                if line_indent > indent {
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
            i = end + 1;
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
}
