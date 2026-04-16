//! MC/DC (Modified Condition/Decision Coverage) analysis for Class C files (US-070).
#![allow(dead_code)]
//!
//! Uses regex-based heuristics to detect compound boolean decisions in source code
//! and compute the minimum number of test cases needed for MC/DC coverage.
//!
//! Conservative: if a decision cannot be parsed reliably, it is skipped
//! (better to under-report than to emit false positives).

use regex::Regex;
use std::sync::OnceLock;

/// A logical operator in a compound boolean expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

/// A compound boolean decision point detected in source code.
///
/// MC/DC requires `N + 1` test cases for a decision with N conditions,
/// where each condition independently affects the decision outcome.
#[derive(Debug, Clone)]
pub struct DecisionPoint {
    /// Source file path (relative to project root).
    pub file: String,
    /// 1-based line number of the decision.
    pub line: u32,
    /// Individual conditions (identifiers or sub-expressions) extracted.
    pub conditions: Vec<String>,
    /// Logical operators between conditions.
    pub operators: Vec<LogicalOp>,
    /// Minimum number of test cases required for MC/DC (= number of conditions + 1).
    pub min_tests_required: usize,
}

impl DecisionPoint {
    pub fn new(file: &str, line: u32, conditions: Vec<String>, operators: Vec<LogicalOp>) -> Self {
        let n = conditions.len();
        Self {
            file: file.to_string(),
            line,
            conditions,
            operators,
            min_tests_required: n + 1,
        }
    }
}

/// Regex patterns for compound boolean expressions, compiled once.
static RE_AND_OR_JAVA_RUST_GO: OnceLock<Regex> = OnceLock::new();
static RE_AND_OR_PYTHON: OnceLock<Regex> = OnceLock::new();
static RE_DECISION_LINE_JAVA: OnceLock<Regex> = OnceLock::new();
static RE_DECISION_LINE_PYTHON: OnceLock<Regex> = OnceLock::new();

fn re_and_or_java_rust_go() -> &'static Regex {
    RE_AND_OR_JAVA_RUST_GO.get_or_init(|| {
        Regex::new(r"\&\&|\|\|").unwrap()
    })
}

fn re_and_or_python() -> &'static Regex {
    RE_AND_OR_PYTHON.get_or_init(|| {
        Regex::new(r"\b(and|or)\b").unwrap()
    })
}

fn re_decision_line_java() -> &'static Regex {
    RE_DECISION_LINE_JAVA.get_or_init(|| {
        // Match if/while/return/assert that contain at least one &&/||
        Regex::new(r"^\s*(if|while|return|assert)\s*\(.*(\&\&|\|\|).*").unwrap()
    })
}

fn re_decision_line_python() -> &'static Regex {
    RE_DECISION_LINE_PYTHON.get_or_init(|| {
        // Match if/while/return/assert that contain at least one and/or
        Regex::new(r"^\s*(if|while|return|assert)\b.+\b(and|or)\b").unwrap()
    })
}

/// Parse source code and return all decision points with ≥2 conditions.
///
/// Only decisions appearing in `if`, `while`, `return`, or `assert` statements
/// are analyzed. Single-condition decisions (no `&&`/`||`/`and`/`or`) are skipped.
///
/// `language` is a hint for selecting the operator syntax:
/// - "python" → uses `and`/`or`
/// - anything else → uses `&&`/`||`
///
/// # Safety
/// Conservative: lines that cannot be cleanly parsed are silently skipped.
pub fn extract_decision_points(source: &str, file: &str, language: &str) -> Vec<DecisionPoint> {
    let use_python_ops = language.eq_ignore_ascii_case("python");
    let decision_re = if use_python_ops {
        re_decision_line_python()
    } else {
        re_decision_line_java()
    };
    let op_re = if use_python_ops {
        re_and_or_python()
    } else {
        re_and_or_java_rust_go()
    };

    let mut results = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let line_no = (line_idx + 1) as u32;

        // Skip comment lines (very conservatively)
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }

        if !decision_re.is_match(line) {
            continue;
        }

        // Count operators to determine number of conditions
        let op_matches: Vec<_> = op_re.find_iter(line).collect();
        let op_count = op_matches.len();
        if op_count < 1 {
            // Need at least 1 operator (= 2 conditions) for MC/DC to be meaningful
            continue;
        }

        // Build operator list and synthetic condition names
        let mut operators = Vec::new();
        let mut conditions = Vec::new();

        for (i, m) in op_matches.iter().enumerate() {
            if i == 0 {
                conditions.push(format!("cond_{}", i + 1));
            }
            let op = match m.as_str() {
                "&&" | "and" => LogicalOp::And,
                "||" | "or" => LogicalOp::Or,
                _ => LogicalOp::And,
            };
            operators.push(op);
            conditions.push(format!("cond_{}", i + 2));
        }

        results.push(DecisionPoint::new(file, line_no, conditions, operators));
    }

    results
}

/// Detect the language from a file extension. Returns a language hint string.
pub fn detect_language(file: &str) -> &'static str {
    let ext = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_lowercase().as_str() {
        "py" => "python",
        "java" | "kt" | "scala" => "java",
        "js" | "ts" | "jsx" | "tsx" => "javascript",
        "rs" => "rust",
        "go" => "go",
        "c" | "cpp" | "h" | "hpp" | "cc" => "c",
        "cs" => "csharp",
        _ => "java", // safe default: use && / ||
    }
}

/// Build the MC/DC gaps section for injection into coverage boost prompts.
///
/// Returns an empty string when `gaps` is empty.
pub fn build_mcdc_prompt_section(gaps: &[DecisionPoint]) -> String {
    if gaps.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\n## MC/DC coverage gaps (Class C — mandatory):\n\n\
         The following decision points do not yet have enough test cases to demonstrate\n\
         Modified Condition/Decision Coverage:\n\n",
    );

    for gap in gaps.iter().take(10) {
        let cond_count = gap.conditions.len();
        let required = gap.min_tests_required;
        out.push_str(&format!(
            "- Line {}: compound condition with {} conditions. \
             MC/DC requires at least {} test cases showing that each condition\n\
             independently affects the outcome. Add tests that vary each condition\n\
             while keeping others fixed to show it changes the result.\n\
             Tag these tests with @Reparo.testType = \"mcdc\"\n\n",
            gap.line, cond_count, required
        ));
    }

    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_no_compound_decisions() {
        let src = "if (x > 0) { return x; }";
        let points = extract_decision_points(src, "src/Calc.java", "java");
        assert!(points.is_empty(), "Single condition should not produce a decision point");
    }

    #[test]
    fn test_extract_and_decision_java() {
        let src = "  if (a && b) { doSomething(); }";
        let points = extract_decision_points(src, "src/Calc.java", "java");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].conditions.len(), 2);
        assert_eq!(points[0].operators, vec![LogicalOp::And]);
        assert_eq!(points[0].min_tests_required, 3); // N+1 = 2+1
    }

    #[test]
    fn test_extract_or_decision_java() {
        let src = "  if (a || b || c) { doSomething(); }";
        let points = extract_decision_points(src, "src/Calc.java", "java");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].conditions.len(), 3); // 2 operators → 3 conditions
        assert_eq!(points[0].min_tests_required, 4); // N+1 = 3+1
    }

    #[test]
    fn test_extract_python_and_or() {
        let src = "if validated and authorized or emergency_override:\n    pass";
        let points = extract_decision_points(src, "src/auth.py", "python");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].conditions.len(), 3);
        assert_eq!(points[0].min_tests_required, 4);
    }

    #[test]
    fn test_extract_while_decision() {
        let src = "  while (i < max && !stopped) { i++; }";
        let points = extract_decision_points(src, "src/Calc.java", "java");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].min_tests_required, 3);
    }

    #[test]
    fn test_skip_comment_lines() {
        let src = "// if (a && b) { /* comment */ }\n  if (x && y) { return; }";
        let points = extract_decision_points(src, "src/Calc.java", "java");
        // First line is a comment, only second should be detected
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].line, 2);
    }

    #[test]
    fn test_min_tests_required_calculation() {
        // N conditions → N+1 tests
        let dp = DecisionPoint::new(
            "f.java",
            10,
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            vec![LogicalOp::And, LogicalOp::Or, LogicalOp::And],
        );
        assert_eq!(dp.min_tests_required, 5); // 4+1
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("src/Calc.java"), "java");
        assert_eq!(detect_language("src/auth.py"), "python");
        assert_eq!(detect_language("src/main.rs"), "rust");
        assert_eq!(detect_language("src/lib.go"), "go");
        assert_eq!(detect_language("unknown.xyz"), "java");
    }

    #[test]
    fn test_build_mcdc_prompt_section_empty() {
        let section = build_mcdc_prompt_section(&[]);
        assert!(section.is_empty());
    }

    #[test]
    fn test_build_mcdc_prompt_section_with_gaps() {
        let dp = DecisionPoint::new(
            "src/Calc.java",
            42,
            vec!["a".into(), "b".into(), "c".into()],
            vec![LogicalOp::And, LogicalOp::Or],
        );
        let section = build_mcdc_prompt_section(&[dp]);
        assert!(section.contains("MC/DC"));
        assert!(section.contains("Line 42"));
        assert!(section.contains("3 conditions"));
        assert!(section.contains("4 test cases"));
    }
}
