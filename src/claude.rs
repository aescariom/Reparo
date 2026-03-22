use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Default per-call timeout for Claude in seconds (US-015).
pub const DEFAULT_CLAUDE_TIMEOUT: u64 = 300;

/// Run `claude -d` with a prompt and a per-call timeout (US-015).
///
/// Spawns Claude as a child process and kills it if it exceeds `timeout_secs`.
/// Returns the stdout output from claude.
pub fn run_claude(project_path: &Path, prompt: &str, timeout_secs: u64, skip_permissions: bool, show_prompt: bool) -> Result<String> {
    info!("Running claude -d (prompt: {} chars, timeout: {}s)", prompt.len(), timeout_secs);
    if show_prompt {
        info!("Claude prompt:\n{}", prompt);
    }
    let start = Instant::now();

    let mut args = vec!["-d", "--output-format", "text"];
    if skip_permissions {
        args.push("--dangerously-skip-permissions");
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
}
