use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Default per-call timeout for Claude in seconds (US-015).
pub const DEFAULT_CLAUDE_TIMEOUT: u64 = 300;

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}...", &s[..max]) }
}

/// Model and effort tier for a Claude invocation.
#[derive(Debug, Clone)]
pub struct ClaudeTier {
    /// Model alias: "sonnet", "opus", "haiku"
    pub model: &'static str,
    /// Effort level: "low", "medium", "high", "max"
    pub effort: &'static str,
    /// Timeout multiplier relative to the base claude_timeout (1.0 = no change)
    pub timeout_multiplier: f64,
}

impl ClaudeTier {
    pub const fn new(model: &'static str, effort: &'static str) -> Self {
        Self { model, effort, timeout_multiplier: 1.0 }
    }

    pub const fn with_timeout(model: &'static str, effort: &'static str, multiplier: f64) -> Self {
        Self { model, effort, timeout_multiplier: multiplier }
    }

    /// Compute the effective timeout from a base timeout.
    pub fn effective_timeout(&self, base_timeout: u64) -> u64 {
        (base_timeout as f64 * self.timeout_multiplier) as u64
    }
}

impl std::fmt::Display for ClaudeTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.model, self.effort)
    }
}

/// Classify a SonarQube issue into a Claude model+effort tier.
///
/// Criteria:
/// - Rule type (simple mechanical fix vs complex refactoring)
/// - Severity (CRITICAL/BLOCKER = higher tier)
/// - Cognitive complexity delta (how much refactoring is needed)
/// - Number of affected lines
pub fn classify_issue_tier(
    rule: &str,
    severity: &str,
    message: &str,
    affected_lines: u32,
) -> ClaudeTier {
    // --- Tier 1: Haiku + low effort — trivial mechanical fixes ---
    let trivial_rules = [
        "S1128",  // unused import
        "S1481",  // unused variable
        "S7772",  // prefer node: prefix
        "S7764",  // prefer globalThis
        "S7773",  // prefer Number.isNaN/parseInt/parseFloat
        "S6535",  // unnecessary escape character
        "S7781",  // prefer String#replaceAll
        "S7719",  // unnecessary .getTime()
        "S3863",  // imported multiple times
        "S4325",  // unnecessary type assertion
        "S7778",  // don't call push multiple times
        "S6353",  // use concise character class
        "S7735",  // unexpected negated condition
        "S6594",  // use RegExp.exec
        "S7766",  // prefer Math.max
        "S1854",  // useless assignment
        "S2933",  // mark as readonly
        "S7756",  // prefer Blob#arrayBuffer
        "S1788",  // default parameters should be last
    ];

    let rule_suffix = rule.split(':').last().unwrap_or(rule);

    if trivial_rules.iter().any(|r| rule_suffix == *r) {
        return ClaudeTier::with_timeout("haiku", "low", 0.3); // 30% of base timeout
    }

    // --- Tier 2: Sonnet + medium effort — moderate fixes ---
    let moderate_rules = [
        "S3358",  // nested ternary
        "S6679",  // use Number.isNaN
        "S107",   // too many parameters
        "S6582",  // prefer optional chain
        "S7785",  // prefer top-level await
        "S6853",  // form label accessibility
        "S7924",  // CSS contrast
    ];

    if moderate_rules.iter().any(|r| rule_suffix == *r) {
        return ClaudeTier::with_timeout("sonnet", "medium", 0.5); // 50% of base timeout
    }

    // --- Tier 3 & 4: Cognitive complexity — scale by delta ---
    if rule_suffix == "S3776" {
        // Parse complexity from message: "...from 18 to the 15 allowed."
        let complexity = parse_complexity_from_message(message);
        return match complexity {
            Some(c) if c <= 20 => ClaudeTier::with_timeout("sonnet", "medium", 0.7),
            Some(c) if c <= 40 => ClaudeTier::with_timeout("sonnet", "high", 1.0),
            Some(c) if c <= 70 => ClaudeTier::with_timeout("opus", "high", 1.5),
            Some(_) => ClaudeTier::with_timeout("opus", "max", 2.0), // 108+ complexity
            None => {
                // Can't parse — use affected lines as proxy
                if affected_lines > 200 {
                    ClaudeTier::with_timeout("opus", "high", 1.5)
                } else {
                    ClaudeTier::with_timeout("sonnet", "high", 1.0)
                }
            }
        };
    }

    // --- Default: severity-based fallback ---
    match severity {
        "BLOCKER" => ClaudeTier::with_timeout("opus", "high", 1.5),
        "CRITICAL" => ClaudeTier::with_timeout("sonnet", "high", 1.0),
        "MAJOR" => ClaudeTier::with_timeout("sonnet", "medium", 0.7),
        _ => ClaudeTier::with_timeout("sonnet", "low", 0.5),
    }
}

/// Classify deduplication difficulty.
/// Lines that are not 100% identical (structural duplicates with small variations)
/// are much harder to refactor — need opus + max effort.
pub fn classify_dedup_tier(duplicated_lines: u64, duplication_pct: f64) -> ClaudeTier {
    // Very high duplication with many lines = very difficult
    if duplicated_lines > 200 || duplication_pct > 50.0 {
        ClaudeTier::with_timeout("opus", "max", 2.0)
    } else if duplicated_lines > 100 || duplication_pct > 30.0 {
        ClaudeTier::with_timeout("opus", "high", 1.5)
    } else if duplicated_lines > 50 || duplication_pct > 15.0 {
        ClaudeTier::with_timeout("sonnet", "high", 1.0)
    } else {
        ClaudeTier::with_timeout("sonnet", "medium", 0.7)
    }
}

/// Classify test generation difficulty based on number of uncovered lines.
pub fn classify_test_gen_tier(uncovered_lines: usize, total_file_lines: usize) -> ClaudeTier {
    if uncovered_lines > 150 || total_file_lines > 500 {
        ClaudeTier::with_timeout("opus", "high", 1.5)
    } else if uncovered_lines > 80 || total_file_lines > 300 {
        ClaudeTier::with_timeout("sonnet", "high", 1.0)
    } else if uncovered_lines > 30 {
        ClaudeTier::with_timeout("sonnet", "medium", 0.7)
    } else {
        ClaudeTier::with_timeout("sonnet", "low", 0.5)
    }
}

/// Classify build/test/lint repair — always fast, targeted fixes.
pub fn classify_repair_tier() -> ClaudeTier {
    ClaudeTier::with_timeout("sonnet", "medium", 0.5)
}

/// Classify contract test generation difficulty for tier selection.
pub fn classify_contract_test_tier(api_interaction_count: usize) -> ClaudeTier {
    if api_interaction_count > 10 {
        ClaudeTier::with_timeout("sonnet", "high", 1.5)
    } else if api_interaction_count > 5 {
        ClaudeTier::with_timeout("sonnet", "high", 1.0)
    } else {
        ClaudeTier::with_timeout("sonnet", "medium", 0.7)
    }
}

/// Parse cognitive complexity value from SonarQube message.
/// Example: "Refactor this function to reduce its Cognitive Complexity from 45 to the 15 allowed."
fn parse_complexity_from_message(message: &str) -> Option<u32> {
    // Look for "from N to"
    let from_idx = message.find("from ")?;
    let after_from = &message[from_idx + 5..];
    let end = after_from.find(|c: char| !c.is_ascii_digit())?;
    after_from[..end].parse().ok()
}

/// Run `claude -d` with a prompt and a per-call timeout (US-015).
///
/// Spawns Claude as a child process and kills it if it exceeds `timeout_secs`.
/// Returns the stdout output from claude.
pub fn run_claude(project_path: &Path, prompt: &str, timeout_secs: u64, skip_permissions: bool, show_prompt: bool) -> Result<String> {
    run_claude_with_tier(project_path, prompt, timeout_secs, skip_permissions, show_prompt, None)
}

/// Run `claude -d` with a specific model+effort tier.
pub fn run_claude_with_tier(project_path: &Path, prompt: &str, timeout_secs: u64, skip_permissions: bool, show_prompt: bool, tier: Option<&ClaudeTier>) -> Result<String> {
    let tier_desc = tier.map(|t| format!(" [{}]", t)).unwrap_or_default();
    info!("Running claude -d (prompt: {} chars, timeout: {}s{})", prompt.len(), timeout_secs, tier_desc);
    if show_prompt {
        info!("Claude prompt:\n{}", prompt);
    }
    let start = Instant::now();

    let mut args = vec!["-d", "--output-format", "text"];
    if skip_permissions {
        args.push("--dangerously-skip-permissions");
    }

    // Add model and effort from tier
    let model_str;
    let effort_str;
    if let Some(t) = tier {
        model_str = t.model.to_string();
        effort_str = t.effort.to_string();
        args.extend(["--model", &model_str, "--effort", &effort_str]);
    }

    args.extend(["-p", prompt]);

    let mut child = Command::new("claude")
        .current_dir(project_path)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn 'claude' CLI. Is it installed and in PATH?")?;

    // Wait with timeout
    let timeout = Duration::from_secs(timeout_secs);
    let result = wait_with_timeout(&mut child, timeout);

    match result {
        WaitResult::Completed(output) => {
            let elapsed = start.elapsed().as_secs();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                warn!("claude exited with status: {} ({}s)", output.status, elapsed);
                if !stderr.is_empty() {
                    warn!("claude stderr: {}", stderr);
                }
                // If Claude failed very quickly (< 10s), it's likely a CLI error, not a legitimate run
                if elapsed < 10 {
                    let error_detail = if !stderr.is_empty() {
                        stderr.clone()
                    } else if !stdout.is_empty() {
                        truncate_str(&stdout, 500)
                    } else {
                        format!("exit status: {} (no output)", output.status)
                    };
                    anyhow::bail!("Claude CLI failed immediately ({}s): {}", elapsed, error_detail);
                }
                // Longer runs that exit non-zero may have done partial work — return stdout
                warn!("Claude exited non-zero after {}s — checking for changes anyway", elapsed);
            } else {
                info!("claude completed in {}s", elapsed);
            }

            Ok(stdout)
        }
        WaitResult::TimedOut => {
            // Kill the process
            let _ = child.kill();
            let _ = child.wait(); // reap zombie
            let elapsed = start.elapsed().as_secs();
            anyhow::bail!(
                "Claude timed out after {}s (limit: {}s). The process was killed.",
                elapsed,
                timeout_secs
            );
        }
    }
}

enum WaitResult {
    Completed(std::process::Output),
    TimedOut,
}

/// Wait for a child process with a timeout, polling every 500ms.
fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> WaitResult {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process finished — collect output
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_end(&mut stderr);
                }
                return WaitResult::Completed(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                // Still running
                if start.elapsed() >= timeout {
                    return WaitResult::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => {
                // Error checking — treat as timeout
                return WaitResult::TimedOut;
            }
        }
    }
}

/// Build a prompt for generating unit tests (US-005).
pub fn build_test_generation_prompt(
    source_file: &str,
    source_content: &str,
    uncovered_lines: &str,
    test_framework_hint: &str,
    existing_test_examples: &str,
) -> String {
    format!(
        r#"I need you to generate unit tests for the following source file to achieve 100% code coverage on the specified lines.

## Source file: `{source_file}`
```
{source_content}
```

## Lines that need test coverage:
{uncovered_lines}

## Testing framework:
{test_framework_hint}

## Examples of existing tests in this project:
{existing_test_examples}

## Instructions:
1. Write unit tests that cover ALL the specified uncovered lines
2. Follow the same style, conventions, and patterns as the existing tests
3. Place the test file in the appropriate location following project conventions
4. Each test should be focused and test one behavior
5. Include both positive and negative test cases where applicable
6. Do NOT modify any existing source code — only create new test files or add tests to existing test files
7. Make sure all tests pass
8. For each uncovered line, ensure at least one test exercises that line's branch/path

Write the tests now."#
    )
}

/// Build a retry prompt for test generation when the first attempt didn't achieve full coverage (US-005).
pub fn build_test_generation_retry_prompt(
    source_file: &str,
    source_content: &str,
    still_uncovered_lines: &str,
    test_framework_hint: &str,
    existing_test_examples: &str,
    attempt: u32,
    previous_test_output: &str,
) -> String {
    format!(
        r#"The previous test generation attempt ({attempt}/3) did NOT achieve 100% coverage. Some lines are still uncovered.

## Source file: `{source_file}`
```
{source_content}
```

## Lines STILL uncovered after previous attempt:
{still_uncovered_lines}

## Testing framework:
{test_framework_hint}

## Examples of existing tests in this project:
{existing_test_examples}

## Output from the previous test run:
```
{previous_test_output}
```

## Instructions:
1. Analyze WHY the above lines are still uncovered — they likely require specific inputs, edge cases, or branch conditions
2. Write ADDITIONAL unit tests that specifically target these uncovered lines
3. For conditional branches, ensure you test both the true and false paths
4. For error handling code, write tests that trigger those error conditions
5. Do NOT modify any existing source code — only create new test files or add tests to existing test files
6. Do NOT duplicate existing tests — add new ones that cover the gaps
7. Make sure all tests (old and new) pass

Write the additional tests now."#
    )
}

/// Build a prompt for generating pact/contract tests.
pub fn build_contract_test_prompt(
    source_file: &str,
    source_content: &str,
    provider_name: &str,
    consumer_name: &str,
    pact_framework: &str,
    existing_contract_examples: &str,
    existing_pact_files: &str,
) -> String {
    format!(
        r#"I need you to generate pact/contract tests for the following source file
that interacts with APIs.

## Source file: `{source_file}`
```
{source_content}
```

## Provider: {provider_name}
## Consumer: {consumer_name}
## Pact framework: {pact_framework}

## Existing contract test examples in this project:
{existing_contract_examples}

## Existing pact contract files:
{existing_pact_files}

## CRITICAL RULES:
1. Do NOT modify any existing source code or test files — only CREATE new contract test files
2. Place new files alongside existing test files, following project conventions
3. Use the {pact_framework} framework, following existing patterns shown above

## Instructions:
1. Identify all API interactions (HTTP calls, message queues, gRPC) in this file
2. For each interaction, create a pact test that defines the expected contract
3. Define provider states that match realistic scenarios
4. Test both successful responses and key error scenarios (4xx, 5xx)
5. Include proper type definitions for request/response bodies
6. Ensure all contract tests pass independently

Write the contract tests now."#
    )
}

/// Build a retry prompt for contract test generation.
pub fn build_contract_test_retry_prompt(
    source_file: &str,
    source_content: &str,
    provider_name: &str,
    consumer_name: &str,
    pact_framework: &str,
    existing_contract_examples: &str,
    attempt: u32,
    previous_output: &str,
) -> String {
    format!(
        r#"The previous contract test generation attempt ({attempt}) failed verification.
Please fix the issues.

## Source file: `{source_file}`
```
{source_content}
```

## Provider: {provider_name}
## Consumer: {consumer_name}
## Pact framework: {pact_framework}

## Existing contract test examples:
{existing_contract_examples}

## Output from the previous attempt:
```
{previous_output}
```

## CRITICAL RULES:
1. Do NOT modify existing source code or test files
2. Fix only the contract tests that were generated in the previous attempt

## Instructions:
1. Analyze the error output to understand what went wrong
2. Fix the contract tests — ensure request/response bodies match the actual API schema
3. Verify provider states are correctly defined
4. Make sure all contract tests pass

Fix the contract tests now."#
    )
}

/// Build a prompt for fixing a SonarQube issue
pub fn build_fix_prompt(
    issue_key: &str,
    issue_type: &str,
    severity: &str,
    rule: &str,
    message: &str,
    file_path: &str,
    file_content: &str,
    start_line: u32,
    end_line: u32,
    rule_description: &str,
) -> String {
    format!(
        r#"Fix the following SonarQube issue in this project.

## Issue details
- **Key**: {issue_key}
- **Type**: {issue_type}
- **Severity**: {severity}
- **Rule**: {rule}
- **Message**: {message}
- **File**: `{file_path}` (lines {start_line}-{end_line})

## Rule description:
{rule_description}

## Current file content (`{file_path}`):
```
{file_content}
```

## CRITICAL RULES:
- **NEVER modify test files** — do NOT touch any file matching *.spec.ts, *.test.ts, *_test.*, test_*.*, *.spec.js, *.test.js. This is an absolute rule. Test file modifications will be automatically reverted.
- **ONLY modify `{file_path}`** and files directly required for the fix to compile.

## Instructions:
1. Fix ONLY the issue described above in `{file_path}`
2. Do NOT change functionality — only fix the specific issue
3. Keep changes minimal and focused on the affected function/lines
4. Ensure the fix follows the existing code style
5. Do NOT introduce code duplication — reuse existing functions/helpers
6. When extracting helper functions, keep them in the SAME file as private/unexported

Apply the fix now."#
    )
}

/// Build a prompt asking Claude to eliminate code duplication in a file.
pub fn build_dedup_prompt(
    file_path: &str,
    file_content: &str,
    duplicated_ranges: &[(u32, u32)], // (from_line, to_line) pairs
    duplication_pct: f64,
) -> String {
    let ranges_desc = duplicated_ranges
        .iter()
        .map(|(from, to)| format!("  - Lines {}-{}", from, to))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"Refactor the following file to eliminate code duplication.

## File: `{file_path}`
- **Duplication**: {duplication_pct:.1}% of lines are duplicated
- **Duplicated ranges**:
{ranges_desc}

## Current file content (`{file_path}`):
```
{file_content}
```

## Instructions:
1. Identify the duplicated code patterns in the ranges listed above
2. Extract common logic into shared helper functions, utilities, or base classes
3. Replace ALL duplicated instances with calls to the shared code
4. Do NOT modify any test files
5. Do NOT change the external behavior or public API of the code
6. Keep the same function signatures for public/exported functions
7. Ensure all extracted helpers are well-named and documented
8. If the duplication spans multiple files, focus ONLY on `{file_path}` — do NOT modify other source files
9. Prefer composition and reuse over copy-paste patterns
10. The goal is to reduce duplicated lines while keeping the code readable

Apply the refactoring now."#
    )
}

/// Build a prompt asking Claude to fix a build or test failure without modifying tests.
pub fn build_fix_error_prompt(
    error_type: &str, // "build" or "test"
    error_output: &str,
    file_path: &str,
    original_issue_message: &str,
) -> String {
    format!(
        r#"The previous fix for a SonarQube issue caused a {error_type} failure. Fix the {error_type} error WITHOUT modifying any test files.

## {error_type} error output:
```
{error_output}
```

## Context:
- The original issue was in `{file_path}`: {original_issue_message}
- The fix was applied but it broke the {error_type}

## Instructions:
1. Fix ONLY the {error_type} error shown above
2. Do NOT modify any test files (*.spec.ts, *.test.ts, *_test.go, test_*.py, etc.)
3. Do NOT revert the original fix — fix the code so it compiles/passes while keeping the intent of the fix
4. Keep changes minimal — only fix what is needed to make the {error_type} succeed
5. If the fix introduced a type error, missing import, or incorrect refactoring, correct it

Fix the {error_type} error now."#
    )
}

/// Build a prompt for documentation quality improvement.
///
/// Generates instructions for Claude to add/improve code documentation
/// to meet ISO 25000 and/or MDR standards.
pub fn build_documentation_prompt(
    file_path: &str,
    file_content: &str,
    style: &str,
    standards: &[String],
    scope: &[String],
    required_elements: &[String],
    custom_rules: Option<&str>,
) -> String {
    let standards_desc = if standards.is_empty() {
        "general best practices".to_string()
    } else {
        standards.iter().map(|s| match s.as_str() {
            "iso25000" => "ISO/IEC 25000 (SQuaRE) — software quality: maintainability, analyzability, modifiability",
            "mdr" => "EU MDR 2017/745 — Medical Device Regulation: traceability, risk documentation, safety annotations",
            other => other,
        }).collect::<Vec<_>>().join(", ")
    };

    let scope_desc = scope.join(", ");

    let style_desc = match style {
        "jsdoc" => "JSDoc (/** @param {type} name - description */)",
        "tsdoc" => "TSDoc (/** @param name - description */)",
        "javadoc" => "Javadoc (/** @param name description */)",
        "pydoc" => "Python docstrings (Google/NumPy style)",
        "rustdoc" => "Rustdoc (/// and //! comments with # Examples)",
        "godoc" => "Godoc (// FunctionName does X)",
        "xmldoc" => "XML documentation comments (/// <summary>)",
        "doxygen" => "Doxygen (/** @brief, @param, @return */)",
        other if !other.is_empty() => other,
        _ => "language-appropriate documentation comments",
    };

    let elements_desc = required_elements.join(", ");

    let custom_section = if let Some(rules) = custom_rules {
        format!("\n\n## Custom project rules:\n{}", rules)
    } else {
        String::new()
    };

    format!(
        r#"Improve the documentation of the following source file to meet quality standards.

## File: `{file_path}`

## Documentation style: {style_desc}

## Standards to comply with: {standards_desc}

## Scope: {scope_desc}

## Required elements per function/method/class: {elements_desc}

## Current file content (`{file_path}`):
```
{file_content}
```
{custom_section}

## Instructions:
1. Add or improve documentation for ALL public classes, interfaces, types, functions, and methods
2. Use the **{style_desc}** documentation style consistently
3. Every function/method MUST have: {elements_desc}
4. For classes/interfaces: add a description explaining purpose, responsibilities, and usage
5. For complex logic: add inline comments explaining WHY, not WHAT
6. Do NOT modify any logic, functionality, or test files
7. Do NOT remove existing documentation — only improve or extend it
8. If a parameter can be null/undefined, document that explicitly
9. Document thrown exceptions/errors
10. For MDR compliance: add @safety, @risk, or @regulatory annotations where applicable to safety-critical code
11. For ISO 25000: ensure documentation supports analyzability (someone new can understand the code from docs alone)
12. Keep documentation concise but complete — avoid redundant descriptions that just restate the name

Apply the documentation improvements now."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_test_generation_prompt_contains_all_context() {
        let prompt = build_test_generation_prompt(
            "src/calculator.py",
            "def add(a, b):\n    return a + b",
            "Lines 1-2 (specifically uncovered: 2)",
            "pytest",
            "// File: tests/test_math.py\ndef test_sub(): assert sub(3,1) == 2",
        );
        assert!(prompt.contains("src/calculator.py"));
        assert!(prompt.contains("def add(a, b)"));
        assert!(prompt.contains("uncovered: 2"));
        assert!(prompt.contains("pytest"));
        assert!(prompt.contains("test_math.py"));
        assert!(prompt.contains("Do NOT modify any existing source code"));
    }

    #[test]
    fn test_build_retry_prompt_includes_attempt_and_output() {
        let prompt = build_test_generation_retry_prompt(
            "src/main.py",
            "x = 1\nif x > 0:\n    print('yes')",
            "Lines still uncovered: 3",
            "pytest",
            "",
            2,
            "FAILED test_foo - AssertionError",
        );
        assert!(prompt.contains("2/3"));
        assert!(prompt.contains("Lines still uncovered: 3"));
        assert!(prompt.contains("FAILED test_foo"));
        assert!(prompt.contains("WHY the above lines are still uncovered"));
        assert!(prompt.contains("Do NOT duplicate existing tests"));
    }

    #[test]
    fn test_build_fix_prompt_contains_issue_details() {
        let prompt = build_fix_prompt(
            "AX123",
            "BUG",
            "CRITICAL",
            "python:S1234",
            "Null dereference",
            "src/service.py",
            "obj = None\nobj.method()",
            1,
            2,
            "# Null Dereference\nAvoid calling methods on null.",
        );
        assert!(prompt.contains("AX123"));
        assert!(prompt.contains("BUG"));
        assert!(prompt.contains("CRITICAL"));
        assert!(prompt.contains("python:S1234"));
        assert!(prompt.contains("Null dereference"));
        assert!(prompt.contains("src/service.py"));
        assert!(prompt.contains("obj = None"));
        assert!(prompt.contains("NEVER modify test files"));
    }

    #[test]
    fn test_build_contract_test_prompt_contains_context() {
        let prompt = build_contract_test_prompt(
            "src/api/users.ts",
            "fetch('/api/users')",
            "UserService",
            "WebApp",
            "pact-js",
            "// existing pact test example",
            "// existing pact file",
        );
        assert!(prompt.contains("src/api/users.ts"));
        assert!(prompt.contains("UserService"));
        assert!(prompt.contains("WebApp"));
        assert!(prompt.contains("pact-js"));
        assert!(prompt.contains("existing pact test example"));
        assert!(prompt.contains("existing pact file"));
        assert!(prompt.contains("Do NOT modify"));
    }

    #[test]
    fn test_build_contract_test_retry_prompt_contains_attempt() {
        let prompt = build_contract_test_retry_prompt(
            "src/api/users.ts",
            "fetch('/api/users')",
            "UserService",
            "WebApp",
            "pact-js",
            "",
            2,
            "Error: expected 200 got 404",
        );
        assert!(prompt.contains("(2)"));
        assert!(prompt.contains("Error: expected 200 got 404"));
        assert!(prompt.contains("Do NOT modify"));
    }

    #[test]
    fn test_classify_contract_test_tier() {
        let small = classify_contract_test_tier(3);
        assert_eq!(small.model, "sonnet");
        assert_eq!(small.effort, "medium");

        let medium = classify_contract_test_tier(7);
        assert_eq!(medium.effort, "high");

        let large = classify_contract_test_tier(15);
        assert_eq!(large.effort, "high");
        assert!(large.timeout_multiplier > medium.timeout_multiplier);
    }
}
