use anyhow::Result;
use std::path::Path;

/// Default per-call timeout for Claude in seconds (US-015).
pub const DEFAULT_CLAUDE_TIMEOUT: u64 = 300;

/// Shared robustness guidelines injected into every test-generation prompt.
/// Ensures generated tests are self-contained, deterministic, and safe for
/// parallel execution regardless of the project language or framework.
const TEST_ROBUSTNESS_GUIDELINES: &str = r#"
## Test rules (MANDATORY):
1. Isolation: each test sets up and tears down its own data; use unique IDs for every created resource (DB rows, files, queue messages, etc.).
2. No live I/O: mock/stub ALL HTTP, databases, message brokers, and filesystem access outside a temp directory; use in-memory or ephemeral DBs for integration tests.
3. Deterministic: freeze or mock wall-clock time and RNG; use fixed locale/timezone where formatting matters.
4. Parallel-safe: never hard-code ports, file paths, or global names — bind to port 0, use OS-assigned temp directories.
5. Fast: each test < 2 s unless explicitly an integration test; avoid sleep/setTimeout except for real async behaviour.
6. Assertions: use framework matchers (assert_eq!, assertEqual, expect().toBe) with descriptive failure messages.
7. No pollution: restore env vars, singletons, and monkey-patches in teardown; write only to tmpdir.
"#;

/// UTF-8-safe string truncation.
fn truncate_str(s: &str, max: usize) -> String {
    let truncated: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{}...", truncated)
    } else {
        truncated
    }
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
/// Delegates to `engine::run_engine` with a Claude invocation.
pub fn run_claude(project_path: &Path, prompt: &str, timeout_secs: u64, skip_permissions: bool, show_prompt: bool) -> Result<String> {
    run_claude_with_tier(project_path, prompt, timeout_secs, skip_permissions, show_prompt, None)
}

/// Run `claude -d` with a specific model+effort tier.
///
/// Delegates to `engine::run_engine` with a Claude invocation. Kept for backward
/// compatibility — new code should use `engine::run_engine` directly.
pub fn run_claude_with_tier(project_path: &Path, prompt: &str, timeout_secs: u64, skip_permissions: bool, show_prompt: bool, tier: Option<&ClaudeTier>) -> Result<String> {
    let invocation = crate::engine::EngineInvocation {
        engine_kind: crate::engine::EngineKind::Claude,
        command: "claude".to_string(),
        base_args: vec!["-d".to_string(), "--output-format".to_string(), "text".to_string()],
        model: tier.map(|t| t.model.to_string()),
        effort: tier.map(|t| t.effort.to_string()),
        prompt_flag: "-p".to_string(),
        prompt_via_stdin: false,
    };
    crate::engine::run_engine(project_path, prompt, timeout_secs, skip_permissions, show_prompt, &invocation)
}

/// Build a prompt for generating unit tests (US-005).
pub fn build_test_generation_prompt(
    source_file: &str,
    uncovered_summary: &str,
    uncovered_snippets: &str,
    test_framework_hint: &str,
    existing_test_examples: &str,
    framework_context: &str,
) -> String {
    let fc_section = if framework_context.is_empty() {
        String::new()
    } else {
        format!("\n## Test stack:\n{}\n", framework_context)
    };
    let examples_section = if existing_test_examples.is_empty() {
        String::new()
    } else {
        format!("\n## Existing test patterns (follow this style):\n{}\n", existing_test_examples)
    };
    let snippets_section = if uncovered_snippets.is_empty() {
        // Fallback when we don't have line-level data
        String::new()
    } else {
        format!("\n## Source code of uncovered lines (lines marked `>` need coverage):\n```\n{}\n```\n", uncovered_snippets)
    };
    format!(
        r#"Write unit tests for `{source_file}` targeting ONLY the uncovered lines below.

## Coverage gap:
{uncovered_summary}
{snippets_section}
## Framework: {test_framework_hint}
{fc_section}{examples_section}
## Rules:
- Cover every line marked `>` — write the minimum tests needed to hit those branches/paths
- Do NOT modify source code — only create or append to test files
- Mock external I/O (HTTP, DB, filesystem); use framework matchers for assertions
- Follow project conventions for test file location and naming

Write the tests now."#
    )
}

/// Build a focused prompt for a single method/block chunk of uncovered code.
///
/// Used when the file has many uncovered lines and is split into method-level
/// chunks for faster, more targeted test generation.
pub fn build_chunk_test_prompt(
    source_file: &str,
    chunk_label: &str,
    chunk_snippet: &str,
    chunk_index: usize,
    total_chunks: usize,
    test_framework_hint: &str,
    framework_context: &str,
) -> String {
    let fc_section = if framework_context.is_empty() {
        String::new()
    } else {
        format!("\n## Test stack:\n{}\n", framework_context)
    };
    format!(
        r#"Write unit tests for `{source_file}` — chunk {chunk_index}/{total_chunks}: **{chunk_label}**.

## Code to cover (lines marked `>` need tests):
```
{chunk_snippet}
```

## Framework: {test_framework_hint}
{fc_section}
## Rules:
- Write the MINIMUM tests to cover every `>` line — target specific branch conditions
- Add tests to the existing test file if one already exists for this source file
- Do NOT modify source code; mock external I/O
- Follow project conventions for test file location and naming

Write the tests now."#
    )
}

/// Build a retry prompt for test generation when the first attempt didn't achieve full coverage (US-005).
pub fn build_test_generation_retry_prompt(
    source_file: &str,
    still_uncovered_summary: &str,
    uncovered_snippets: &str,
    test_framework_hint: &str,
    attempt: u32,
    previous_test_output: &str,
    framework_context: &str,
) -> String {
    let fc_section = if framework_context.is_empty() {
        String::new()
    } else {
        format!("\n## Test stack:\n{}\n", framework_context)
    };
    let snippets_section = if uncovered_snippets.is_empty() {
        String::new()
    } else {
        format!("\n## Source code still uncovered (lines marked `>`):\n```\n{}\n```\n", uncovered_snippets)
    };
    format!(
        r#"Retry {attempt} — `{source_file}` still has uncovered lines after the previous attempt.

## Remaining gap:
{still_uncovered_summary}
{snippets_section}
## Framework: {test_framework_hint}
{fc_section}
## Previous test output:
```
{previous_test_output}
```

## What to do:
- Look at each `>` line: identify the branch condition or input that reaches it
- Write ADDITIONAL tests that force execution through those paths
- For conditionals: test both true/false; for error handling: trigger the error
- Do NOT modify source code or duplicate existing tests

Write the additional tests now."#
    )
}

/// Build a prompt for generating pact/contract tests.
pub fn build_contract_test_prompt(
    source_file: &str,
    provider_name: &str,
    consumer_name: &str,
    pact_framework: &str,
    existing_contract_examples: &str,
    existing_pact_files: &str,
) -> String {
    format!(
        r#"Generate pact/contract tests for `{source_file}` (read the file to find API interactions).

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

{TEST_ROBUSTNESS_GUIDELINES}
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
/// Omits source content and examples (already seen on first attempt), and skips guidelines.
pub fn build_contract_test_retry_prompt(
    source_file: &str,
    provider_name: &str,
    consumer_name: &str,
    pact_framework: &str,
    attempt: u32,
    previous_output: &str,
) -> String {
    format!(
        r#"Contract test attempt {attempt} for `{source_file}` failed. Re-read the source file and fix the issues.

## Provider: {provider_name}
## Consumer: {consumer_name}
## Pact framework: {pact_framework}

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
    start_line: u32,
    end_line: u32,
    rule_description: &str,
) -> String {
    format!(
        r#"Fix the following SonarQube issue. Read `{file_path}` to see the current code.

## Issue details
- **Key**: {issue_key}
- **Type**: {issue_type}
- **Severity**: {severity}
- **Rule**: {rule}
- **Message**: {message}
- **File**: `{file_path}` (lines {start_line}-{end_line})

## Rule description:
{rule_description}

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
    duplicated_ranges: &[(u32, u32)], // (from_line, to_line) pairs
    duplication_pct: f64,
) -> String {
    let ranges_desc = duplicated_ranges
        .iter()
        .map(|(from, to)| format!("  - Lines {}-{}", from, to))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"Refactor `{file_path}` to eliminate code duplication. Read the file first.

## Duplication: {duplication_pct:.1}% of lines are duplicated
## Duplicated ranges:
{ranges_desc}

## Instructions:
1. Read `{file_path}` and identify duplicated code in the ranges above
2. Extract common logic into shared helper functions, utilities, or base classes
3. Replace ALL duplicated instances with calls to the shared code
4. Do NOT modify any test files
5. Do NOT change the external behavior or public API of the code
6. Keep the same function signatures for public/exported functions
7. If the duplication spans multiple files, focus ONLY on `{file_path}`
8. Prefer composition and reuse over copy-paste patterns

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
        r#"Improve documentation in `{file_path}` to meet quality standards. Read the file first.

## Documentation style: {style_desc}

## Standards to comply with: {standards_desc}

## Scope: {scope_desc}

## Required elements per function/method/class: {elements_desc}
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
        let snippets = "// Lines 2-2 (UNCOVERED):\n   1: def add(a, b):\n>  2:     return a + b\n";
        let prompt = build_test_generation_prompt(
            "src/calculator.py",
            "1 uncovered line out of 2 total",
            snippets,
            "pytest",
            "// File: tests/test_math.py\ndef test_sub(): assert sub(3,1) == 2",
            "",
        );
        assert!(prompt.contains("src/calculator.py"));
        assert!(prompt.contains("1 uncovered line"));
        assert!(prompt.contains("pytest"));
        assert!(prompt.contains("test_math.py"));
        assert!(prompt.contains("Do NOT modify source code"));
        // Source snippets ARE embedded directly in the prompt
        assert!(prompt.contains("return a + b"));
        assert!(prompt.contains("UNCOVERED"));
    }

    #[test]
    fn test_build_retry_prompt_includes_attempt_and_output() {
        let snippets = "// Lines 3-3 (UNCOVERED):\n>  3: raise ValueError\n";
        let prompt = build_test_generation_retry_prompt(
            "src/main.py",
            "1 line still uncovered",
            snippets,
            "pytest",
            2,
            "FAILED test_foo - AssertionError",
            "",
        );
        assert!(prompt.contains("Retry 2"));
        assert!(prompt.contains("1 line still uncovered"));
        assert!(prompt.contains("FAILED test_foo"));
        assert!(prompt.contains("raise ValueError"));
        assert!(prompt.contains("Do NOT modify source code"));
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
        assert!(prompt.contains("NEVER modify test files"));
        // File content NOT embedded
        assert!(!prompt.contains("obj = None"));
    }

    #[test]
    fn test_build_contract_test_prompt_contains_context() {
        let prompt = build_contract_test_prompt(
            "src/api/users.ts",
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
            "UserService",
            "WebApp",
            "pact-js",
            2,
            "Error: expected 200 got 404",
        );
        assert!(prompt.contains("attempt 2"));
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
