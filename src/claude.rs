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
    // --- Linter-origin rules: `lint:<format>:<rule>` ---
    // Most linter smells are trivial mechanical fixes (unused imports,
    // formatting, simple rewrites). A handful of complexity rules are the
    // exception — route them through the existing complexity pipeline below.
    if let Some(lint_rule) = rule.strip_prefix("lint:") {
        // `lint_rule` is e.g. "clippy:cognitive_complexity" or "ruff:F401"
        let lower = lint_rule.to_lowercase();
        let is_complex = lower.contains("cognitive_complexity")
            || lower.contains("cyclomatic_complexity")
            || lower.contains("too_many_lines")
            || lower.contains("too_many_arguments");
        if !is_complex {
            // BLOCKER/CRITICAL linter findings escalate one tier — better
            // models are cheap insurance for security-flavoured lints.
            let sev_upper = severity.to_uppercase();
            if sev_upper == "BLOCKER" || sev_upper == "CRITICAL" {
                return ClaudeTier::with_timeout("sonnet", "low", 0.4);
            }
            return ClaudeTier::with_timeout("haiku", "low", 0.3);
        }
        // Fall through to the complexity handling below for complex lint rules.
    }

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

    // --- Mechanical BLOCKER/CRITICAL rules that DON'T need opus ---
    //
    // Sonar marks many mechanical rules as BLOCKER/CRITICAL even though the
    // fix is one line (missing assertion, no tests in test class, nullable
    // dereference, resource not closed). Routing these to opus:high because
    // they happen to be severity=BLOCKER wastes ~$0.50/call and ~5 min of
    // latency each; sonnet:medium handles them in seconds. Add more rules
    // here as you see them over-escalated in the usage table.
    let mechanical_rules = [
        "S2699",  // test has no assertion
        "S2187",  // test class has no tests
        "S2259",  // null pointer dereference
        "S2095",  // resource not closed
        "S1135",  // TODO comment
        "S1172",  // unused method parameter
        "S1186",  // empty method
        "S1192",  // duplicated string literal
        "S1118",  // utility class instantiable
        "S1068",  // unused private field
        "S3457",  // printf format string
        "S112",   // generic exception
        "S1161",  // missing @Override
        "S00117", // variable name convention
        "S00100", // method name convention
        "S00101", // class name convention
        "S1125",  // boolean literal in expression
        "S1126",  // if-return boolean
        "S3008",  // static field name convention
    ];
    if mechanical_rules.iter().any(|r| rule_suffix == *r) {
        return ClaudeTier::with_timeout("sonnet", "medium", 0.7);
    }

    // --- Cognitive complexity: single-method difficulty ---
    //
    // `classify_issue_tier` sees one issue in one file. Cognitive complexity,
    // however high, is always within a single method. Opus's advantage over
    // Sonnet is cross-file reasoning — not "think harder about this one
    // method". So we keep every cognitive-complexity fix on Sonnet, with
    // effort (not model) scaling by the size of the refactor. The only case
    // that escalates to Opus is extreme complexity (>80 after max); even
    // there, we use effort=high (not max) because max is reserved for
    // genuinely multi-class work (see `classify_dedup_tier`).
    if rule_suffix == "S3776" {
        let complexity = parse_complexity_from_message(message);
        return match complexity {
            Some(c) if c <= 20 => ClaudeTier::with_timeout("sonnet", "medium", 0.7),
            Some(c) if c <= 40 => ClaudeTier::with_timeout("sonnet", "medium", 0.7),
            Some(c) if c <= 70 => ClaudeTier::with_timeout("sonnet", "high", 1.0),
            Some(_) => ClaudeTier::with_timeout("opus", "high", 1.5),
            None => {
                // Can't parse — use affected lines as proxy.
                if affected_lines > 200 {
                    ClaudeTier::with_timeout("sonnet", "high", 1.0)
                } else {
                    ClaudeTier::with_timeout("sonnet", "medium", 0.7)
                }
            }
        };
    }

    // --- Default: severity + scope-based fallback ---
    //
    // Severity is NOT a reliable proxy for difficulty. Line count isn't
    // either — a 100-line BLOCKER could still be a rote edit. Since this
    // classifier only sees a single-file issue, the ceiling is sonnet:high.
    // Opus is reachable only via `classify_dedup_tier` (cross-class work
    // by definition) and the extreme-cognitive-complexity branch above.
    match severity {
        "BLOCKER" => {
            if affected_lines > 80 {
                ClaudeTier::with_timeout("sonnet", "high", 1.0)
            } else {
                ClaudeTier::with_timeout("sonnet", "medium", 0.7)
            }
        }
        "CRITICAL" => ClaudeTier::with_timeout("sonnet", "medium", 0.7),
        "MAJOR" => ClaudeTier::with_timeout("sonnet", "medium", 0.5),
        _ => ClaudeTier::with_timeout("sonnet", "low", 0.3),
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

/// Build a `ClaudeTier` from a `TierSpec`, using a conventional timeout multiplier
/// per model.
fn tier_from_spec(spec: &crate::config::TierSpec) -> ClaudeTier {
    let mult = match (spec.model.as_str(), spec.effort.as_str()) {
        ("haiku", _) => 0.3,
        ("sonnet", "low") => 0.5,
        ("sonnet", "medium") => 0.7,
        ("sonnet", "high") => 1.0,
        ("opus", "high") => 1.5,
        ("opus", "max") => 2.0,
        _ => 0.7,
    };
    ClaudeTier::with_timeout(
        model_to_static(&spec.model),
        effort_to_static(&spec.effort),
        mult,
    )
}

/// Map dynamic model string to a static str. Falls back to "sonnet".
fn model_to_static(model: &str) -> &'static str {
    match model {
        "haiku" => "haiku",
        "sonnet" => "sonnet",
        "opus" => "opus",
        _ => "sonnet",
    }
}

/// Map dynamic effort string to a static str. Falls back to "medium".
fn effort_to_static(effort: &str) -> &'static str {
    match effort {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "max" => "max",
        _ => "medium",
    }
}

/// Classify test generation difficulty for a whole-file (single-prompt) coverage pass.
///
/// Complexity is driven by the number of uncovered lines the AI must reason about,
/// NOT the total file size — a 600-line file with 5 uncovered lines is an easy task.
///
/// The `tiers` parameter supplies the model/effort for each band — defaults come
/// from `TestGenTiers::default()` and can be overridden in `reparo.yaml`.
pub fn classify_test_gen_tier(
    uncovered_lines: usize,
    _total_file_lines: usize,
    tiers: &crate::config::TestGenTiers,
) -> ClaudeTier {
    if uncovered_lines > 60 {
        tier_from_spec(&tiers.complex)
    } else if uncovered_lines > 30 {
        tier_from_spec(&tiers.high)
    } else if uncovered_lines > 20 {
        tier_from_spec(&tiers.medium)
    } else {
        tier_from_spec(&tiers.low)
    }
}

/// Classify test generation difficulty for a single method/block chunk.
///
/// Method-level chunks are smaller and more focused than whole-file prompts, so
/// the thresholds are tighter. The key complexity driver is the number of branches
/// the method contains (approximated by uncovered line count) and the method's
/// total size (more context = harder to reason about).
pub fn classify_chunk_test_gen_tier(
    uncovered_lines: usize,
    method_total_lines: usize,
    tiers: &crate::config::TestGenTiers,
) -> ClaudeTier {
    // A method with >40 uncovered lines or >80 total lines is genuinely complex
    // (deep nesting, state machines, parsers).
    if uncovered_lines > 40 || method_total_lines > 80 {
        tier_from_spec(&tiers.complex)
    } else if uncovered_lines > 20 || method_total_lines > 50 {
        tier_from_spec(&tiers.high)
    } else if uncovered_lines > 10 {
        tier_from_spec(&tiers.medium)
    } else if uncovered_lines > 5 || method_total_lines > 30 {
        tier_from_spec(&tiers.low)
    } else {
        // ≤5 uncovered lines in a small method — haiku can handle it
        tier_from_spec(&tiers.trivial)
    }
}

/// Classify build/test/lint repair — always fast, targeted fixes.
pub fn classify_repair_tier() -> ClaudeTier {
    ClaudeTier::with_timeout("sonnet", "medium", 0.5)
}

/// Tier for repairing test failures after a fix. Most test failures are
/// shallow (missing stub, wrong signature, off-by-one) and don't need sonnet
/// depth — haiku repairs them in a fraction of the time. When haiku can't
/// solve it, fix_loop's outer retry on the original issue escalates back up.
pub fn classify_test_repair_tier() -> ClaudeTier {
    ClaudeTier::with_timeout("haiku", "medium", 0.4)
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
        base_args: vec!["-d".to_string(), "--output-format".to_string(), "json".to_string()],
        model: tier.map(|t| t.model.to_string()),
        effort: tier.map(|t| t.effort.to_string()),
        prompt_flag: "-p".to_string(),
        prompt_via_stdin: false,
        extra_args: Vec::new(),
    };
    crate::engine::run_engine(project_path, prompt, timeout_secs, skip_permissions, show_prompt, &invocation)
}

/// US-066: Compliance trace context injected into test generation prompts when
/// compliance mode is enabled. Populates the `@Reparo.*` annotations the AI
/// must include in every generated test.
///
/// `risk_class` is gated on `--health-mode`: only populated (and only required
/// in the trace block) for medical/health software governed by IEC 62304
/// Class A/B/C. For baseline `--compliance` (ISO 25010 / ENS Alto) it stays
/// `None` and the field is omitted from the trace block.
#[derive(Debug, Clone)]
pub struct ComplianceTraceContext {
    pub run_id: String,
    pub requirement: String,
    /// IEC 62304 software safety class. `None` when not in health mode.
    pub risk_class: Option<String>,
}

impl ComplianceTraceContext {
    /// Baseline compliance context (ISO 25010 / ENS Alto).
    /// Does NOT include a `risk_class` — health-mode callers should use
    /// `with_risk_class` to add one.
    pub fn new(run_id: impl Into<String>, requirement: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            requirement: requirement.into(),
            risk_class: None,
        }
    }

    /// Attach a health-mode risk class (IEC 62304 Class A/B/C).
    pub fn with_risk_class(mut self, risk_class: impl Into<String>) -> Self {
        self.risk_class = Some(risk_class.into());
        self
    }
}

/// US-066: Build the "Compliance requirements" section of the prompt.
///
/// When compliance is enabled, every generated test MUST include a trace block
/// in its docstring with the @Reparo.* fields so auditors can trace tests back
/// to the run and the requirement they verify.
///
/// The `@Reparo.riskClass` line is **only emitted when the context has a risk
/// class**, i.e. when the caller is in `--health-mode` (IEC 62304 Class A/B/C
/// classification). For baseline `--compliance` (ISO 25010 / ENS Alto) the
/// field is omitted and the AI is not asked to populate it.
fn build_compliance_section(ctx: Option<&ComplianceTraceContext>) -> String {
    let Some(c) = ctx else { return String::new() };

    // Standards list: always includes ISO 25010 / ENS Alto; adds medical
    // standards only when a risk class is present (health mode).
    let standards_line = if c.risk_class.is_some() {
        "This project is configured for regulated medical software (MDR 2017/745 / IEC 62304 / IEC 62305 / ISO 14971) on top of baseline compliance (ISO 25010 / ENS Alto)."
    } else {
        "This project is configured for regulated software (ISO 25010 software quality / ISO/IEC 33020 process assessment / ENS Alto)."
    };

    // riskClass field is conditional: present only in health mode.
    let (risk_line_in_spec, risk_line_in_example) = match c.risk_class.as_deref() {
        Some(rc) => (
            format!("    @Reparo.riskClass  {}  (IEC 62304 software safety classification — mandatory in health mode)\n", rc),
            format!("     * @Reparo.riskClass {}\n", rc),
        ),
        None => (String::new(), String::new()),
    };

    format!(
        r#"
## Compliance requirements (MANDATORY — auditable, do not skip):

{standards_line}

Every test you write MUST include a docstring/javadoc IMMEDIATELY above the test function
with ALL of the following fields, using the language-appropriate comment syntax (javadoc
for Java/Kotlin, triple-quoted docstring for Python, JSDoc for JS/TS, doc-comment for Rust):

    @Reparo.purpose     <one-line business-language description of the behaviour verified>
    @Reparo.requirement {requirement}
    @Reparo.testType    <unit|integration|boundary|negative|security>
{risk_line_in_spec}    @Reparo.runId       {run_id}

Classification of @Reparo.testType for this run:
- happy-path tests (normal inputs) → `unit`
- tests for boundary values, empties, range extremes → `boundary`
- tests that trigger exceptions, errors or defensive paths → `negative`
- tests that verify authentication / authorization / input sanitization → `security`

Tests WITHOUT a complete trace block WILL BE REJECTED.

Example for Java (apply the same structure in other languages):

    /**
     * @Reparo.purpose Verifies processOrder returns FAILED when the customer id is unknown.
     * @Reparo.requirement {requirement}
     * @Reparo.testType negative
{risk_line_in_example}     * @Reparo.runId {run_id}
     */
    @Test
    void testProcessOrder_unknownCustomer_returnsFailed() {{ ... }}
"#,
        standards_line = standards_line,
        requirement = c.requirement,
        risk_line_in_spec = risk_line_in_spec,
        risk_line_in_example = risk_line_in_example,
        run_id = c.run_id,
    )
}

/// Build a prompt for generating unit tests (US-005).
///
/// US-060: Stable content (task description, rules, framework stack, test style
/// examples) is placed at the START of the prompt so Anthropic's automatic prompt
/// caching can reuse the same prefix across multiple files within a run. Variable
/// content (source file name, coverage gap, uncovered snippets) goes at the END.
///
/// US-067: When `boundary_hints` is non-empty, adds a "Mandatory test coverage
/// categories" section that requires boundary/negative/null testing — critical
/// for ISO 25010 Reliability and IEC 62304 §5.5.4 software unit testing.
pub fn build_test_generation_prompt(
    source_file: &str,
    uncovered_summary: &str,
    uncovered_snippets: &str,
    test_framework_hint: &str,
    existing_test_examples: &str,
    framework_context: &str,
    boundary_hints: &str,
    compliance_ctx: Option<&ComplianceTraceContext>,
    expected_test_path: Option<&str>,
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
    let mandatory_section = build_mandatory_categories_section(boundary_hints);
    let compliance_section = build_compliance_section(compliance_ctx);
    let snippets_section = if uncovered_snippets.is_empty() {
        // Fallback when we don't have line-level data
        String::new()
    } else {
        format!("\n## Source code of uncovered lines (lines marked `>` need coverage):\n```\n{}\n```\n", uncovered_snippets)
    };
    let placement_rule = match expected_test_path {
        Some(p) => format!("- Write the test file to: `{}` (do not place it next to the source file)\n", p),
        None => "- Follow project conventions for test file location and naming\n".to_string(),
    };
    format!(
        r#"You are generating unit tests to cover specific lines of source code.

## Rules (apply to every test you write):
- Cover every line marked `>` — write the minimum tests needed to hit those branches/paths
- Do NOT modify source code — only create or append to test files
- Mock external I/O (HTTP, DB, filesystem); use framework matchers for assertions
- {placement_rule}- Target branches that are NOT taken (when branch data is provided) to cover both paths
{mandatory_section}{compliance_section}
## Framework: {test_framework_hint}
{fc_section}{examples_section}
---

## Target file: `{source_file}`

## Coverage gap:
{uncovered_summary}
{snippets_section}
Write the tests now."#
    )
}

/// US-067: Build the "Mandatory test coverage categories" section that forces
/// the AI to generate boundary, negative and null tests — not just happy path.
/// Empty when no hints were detected (keeps the prompt lean for trivial files).
fn build_mandatory_categories_section(boundary_hints: &str) -> String {
    if boundary_hints.is_empty() {
        return String::new();
    }
    format!(
        r#"
## Mandatory test coverage categories (ISO 25010 Reliability / IEC 62304 §5.5.4):

For the uncovered code below you MUST generate tests in the following categories where applicable:

1. **Happy path**: at least one test verifying normal operation with valid inputs.
2. **Boundary values**: for every numeric comparison, generate tests at the boundary (==), one unit below, and one unit above. For collections/strings test empty, single-element, and near-capacity cases.
3. **Negative / error paths**: for every `throw`/`raise`/error return, generate a test that triggers it. Verify the exception type AND message where feasible.
4. **Null / empty inputs**: for every nullable parameter generate a test passing null/None/undefined. Verify defensive behaviour (no NullPointerException escaping as IllegalStateException etc.).
5. **Range extremes**: for numeric parameters test MIN, MAX and 0.

If a category does not apply (e.g. method has no numeric params), explicitly skip it and note why in a short comment at the top of the test file.

## Source-code hints detected (use these as guidance):
{boundary_hints}
"#
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
    boundary_hints: &str,
    compliance_ctx: Option<&ComplianceTraceContext>,
) -> String {
    let fc_section = if framework_context.is_empty() {
        String::new()
    } else {
        format!("\n## Test stack:\n{}\n", framework_context)
    };
    let mandatory_section = build_mandatory_categories_section(boundary_hints);
    let compliance_section = build_compliance_section(compliance_ctx);
    // US-060: stable prefix first (rules, framework, stack) so prompt caching
    // can reuse it across every chunk call of the run.
    format!(
        r#"You are generating unit tests to cover specific lines within a single method.

## Rules (apply to every chunk):
- Write the MINIMUM tests to cover every `>` line — target specific branch conditions
- Add tests to the existing test file if one already exists for this source file
- Do NOT modify source code; mock external I/O
- Follow project conventions for test file location and naming
{mandatory_section}{compliance_section}
## Framework: {test_framework_hint}
{fc_section}
---

## Target file: `{source_file}` — chunk {chunk_index}/{total_chunks}: **{chunk_label}**

## Code to cover (lines marked `>` need tests):
```
{chunk_snippet}
```

Write the tests now."#
    )
}

/// Build a test generation prompt for a batch of 1 or more method chunks from
/// the same source file.
///
/// When the batch contains a single chunk the output is identical to
/// `build_chunk_test_prompt`. When it contains multiple chunks each method is
/// rendered as a separate numbered section so Claude can write targeted tests
/// for all of them in one call, saving API round-trips for small methods.
/// Build a prompt for the **general whole-file coverage pass**.
///
/// Used when a file is far below the coverage threshold (> 20 pp gap) and small
/// enough to embed in full.  The entire source is included with line numbers and
/// `>` markers on uncovered lines.  One call establishes broad coverage; a
/// subsequent fine-tune pass (chunk or single-prompt) can then close the gap.
pub fn build_whole_file_coverage_prompt(
    source_file: &str,
    coverage_pct: f64,
    min_coverage: f64,
    annotated_source: &str,
    test_framework_hint: &str,
    existing_test_examples: &str,
    framework_context: &str,
    compliance_ctx: Option<&ComplianceTraceContext>,
    expected_test_path: Option<&str>,
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
    let compliance_section = build_compliance_section(compliance_ctx);
    let placement_rule = match expected_test_path {
        Some(p) => format!("- Write the test file to: `{}` (do not place it next to the source file)\n", p),
        None => "- Follow project conventions for test file location and naming\n".to_string(),
    };
    format!(
        r#"You are generating unit tests for a source file that is significantly under-covered and needs a comprehensive first pass.

## Rules:
- Cover ALL lines marked `>` — these are currently uncovered
- Do NOT modify source code — only create or append to test files
- Mock external I/O (HTTP, DB, filesystem); use framework matchers for assertions
- {placement_rule}- Generate tests for ALL uncovered paths: happy path, error cases, edge cases
{compliance_section}## Framework: {test_framework_hint}
{fc_section}{examples_section}
---

## Target file: `{source_file}`

## Coverage status: {coverage_pct:.1}% → target ≥{min_coverage:.0}% (lines marked `>` are uncovered)

## Full source with coverage markers (`>` = uncovered line that needs a test):
```
{annotated_source}
```

Write comprehensive tests now. Each `>` line must be reachable by at least one test."#
    )
}

/// Annotate source code with line numbers and `>` markers on uncovered lines.
///
/// Returns a string like:
/// ```
///   1 | class Foo {
/// > 2 |   public int get() { return value; }
///   3 | }
/// ```
pub fn annotate_source_with_coverage(source: &str, uncovered_lines: &[u32]) -> String {
    use std::collections::HashSet;
    let uncovered_set: HashSet<u32> = uncovered_lines.iter().copied().collect();
    let total = source.lines().count();
    let width = total.to_string().len().max(1);
    source
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let n = (i + 1) as u32;
            if uncovered_set.contains(&n) {
                format!("> {:width$} | {}", n, line, width = width)
            } else {
                format!("  {:width$} | {}", n, line, width = width)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

///
/// `chunks` is a slice of `(label, snippet)` pairs — callers compact snippets
/// with `compact_method_snippet` before calling this function.
pub fn build_batched_chunk_test_prompt(
    source_file: &str,
    chunks: &[(&str, &str)],
    batch_index: usize,
    total_batches: usize,
    test_framework_hint: &str,
    framework_context: &str,
    boundary_hints: &str,
    compliance_ctx: Option<&ComplianceTraceContext>,
    expected_test_path: Option<&str>,
) -> String {
    let fc_section = if framework_context.is_empty() {
        String::new()
    } else {
        format!("\n## Test stack:\n{}\n", framework_context)
    };
    let mandatory_section = build_mandatory_categories_section(boundary_hints);
    let compliance_section = build_compliance_section(compliance_ctx);
    let placement_rule = match expected_test_path {
        Some(p) => format!("- Write the test file to: `{}` (do not place it next to the source file)\n", p),
        None => "- Follow project conventions for test file location and naming\n".to_string(),
    };

    let code_section = if chunks.len() == 1 {
        let (label, snippet) = &chunks[0];
        format!(
            "## Target file: `{source_file}` — batch {batch_index}/{total_batches}: **{label}**\n\n\
             ## Code to cover (lines marked `>` need tests):\n```\n{snippet}\n```\n"
        )
    } else {
        let mut s = format!(
            "## Target file: `{source_file}` — batch {batch_index}/{total_batches} ({} methods)\n\n\
             Cover every `>` line across all methods below.\n\n",
            chunks.len()
        );
        for (i, (label, snippet)) in chunks.iter().enumerate() {
            s.push_str(&format!(
                "### Method {}: {}\n```\n{}\n```\n\n",
                i + 1,
                label,
                snippet
            ));
        }
        s
    };

    // US-060: stable prefix (rules + framework + stack) at the top so it can be
    // cached across every batch call of the run. Variable parts (file, code) go last.
    format!(
        r#"You are generating unit tests to cover specific lines within one or more methods.

## Rules (apply to every method):
- Write the MINIMUM tests to cover every `>` line — target specific branch conditions
- Add tests to the existing test file if one already exists for this source file
- Do NOT modify source code; mock external I/O
- {placement_rule}{mandatory_section}{compliance_section}
## Framework: {test_framework_hint}
{fc_section}
---

{code_section}Write the tests now."#
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
    // When snippets are available they show exactly which lines need coverage —
    // the text summary is redundant and wastes tokens.
    let gap_section = if uncovered_snippets.is_empty() {
        format!("## Remaining gap:\n{}\n\n", still_uncovered_summary)
    } else {
        format!(
            "## Source code still uncovered (lines marked `>`):\n```\n{}\n```\n\n",
            uncovered_snippets
        )
    };
    // US-060: stable prefix (rules + framework) at the top so it can be cached
    // across retry rounds. Variable parts (attempt #, source file, gap, previous
    // output) go at the end.
    format!(
        r#"You are retrying test generation for a file where the previous attempt
did not achieve full coverage.

## Rules (apply to every retry):
- Look at each `>` line: identify the branch condition or input that reaches it
- Write ADDITIONAL tests that force execution through those paths
- For conditionals: test both true/false; for error handling: trigger the error
- Do NOT modify source code or duplicate existing tests

## Framework: {test_framework_hint}
{fc_section}
---

## Retry {attempt} — target file: `{source_file}`

{gap_section}## Previous test output:
```
{previous_test_output}
```

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
/// Lightweight test-file detector used by prompt builders to keep claude.rs
/// independent of orchestrator/helpers.rs. Mirrors `helpers::is_test_file`.
fn is_test_path(p: &str) -> bool {
    let lower = p.to_lowercase();
    lower.contains("/test/") || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.ends_with("test.java") || lower.ends_with("tests.java") || lower.ends_with("it.java")
        || lower.ends_with("test.kt") || lower.ends_with("spec.kt")
        || lower.ends_with("_test.go")
        || lower.ends_with(".test.ts") || lower.ends_with(".test.tsx")
        || lower.ends_with(".test.js") || lower.ends_with(".test.jsx")
        || lower.ends_with(".spec.ts") || lower.ends_with(".spec.tsx")
        || lower.ends_with(".spec.js") || lower.ends_with(".spec.jsx")
        || std::path::Path::new(&lower)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with("test_") || s.ends_with("_test.py") || s.ends_with("_spec.rb"))
            .unwrap_or(false)
}

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
    // Scope-aware rule: when the issue is in a test file, source files are
    // off-limits; otherwise tests are off-limits. Mixed-scope edits are
    // automatically reverted.
    let is_test_target = is_test_path(file_path);
    let scope_rule = if is_test_target {
        "- **DO NOT modify any source files** — only `{file_path}` (a test file) and other test files are in scope. Source-file modifications will be automatically reverted.\n- **ONLY modify `{file_path}`** and other test files directly required for the fix to compile."
    } else {
        "- **NEVER modify test files** — do NOT touch any file matching *.spec.ts, *.test.ts, *_test.*, test_*.*, *.spec.js, *.test.js. This is an absolute rule. Test file modifications will be automatically reverted.\n- **ONLY modify `{file_path}`** and files directly required for the fix to compile."
    };
    let scope_rule = scope_rule.replace("{file_path}", file_path);
    // Linter-origin rules carry synthetic `lint:<format>:<rule>` keys. Point
    // that out up front so the model knows the finding came from a local
    // static-analysis tool (rather than SonarQube's server).
    let origin_line = if let Some(rest) = rule.strip_prefix("lint:") {
        let format = rest.split(':').next().unwrap_or("linter");
        format!(
            "\n**Note:** this issue was reported by your local linter (`{}`), not SonarQube.\n",
            format
        )
    } else {
        String::new()
    };
    format!(
        r#"Fix the following SonarQube issue. Read `{file_path}` to see the current code.
{origin_line}
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
{scope_rule}

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

    // --- Tier classification ---

    #[test]
    fn test_classify_blocker_missing_assertion_stays_sonnet_not_opus() {
        // java:S2699 = missing test assertion. Sonar flags it as BLOCKER but
        // the fix is literally one line. Should NOT route to opus.
        let tier = classify_issue_tier("java:S2699", "BLOCKER", "Add at least one assertion to this test case.", 1);
        assert_eq!(tier.model, "sonnet", "S2699 should not use opus");
        assert_eq!(tier.effort, "medium");
    }

    #[test]
    fn test_classify_blocker_empty_test_class_stays_sonnet_not_opus() {
        // java:S2187 = test class has no tests. Mechanical.
        let tier = classify_issue_tier("java:S2187", "BLOCKER", "Add some tests to this class or remove it.", 1);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "medium");
    }

    #[test]
    fn test_classify_blocker_unknown_small_range_uses_sonnet_medium() {
        // An unknown BLOCKER rule with a small range is almost always
        // mechanical. Should stay on sonnet:medium, not opus:high.
        let tier = classify_issue_tier("java:S9999", "BLOCKER", "hypothetical", 5);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "medium");
    }

    #[test]
    fn test_classify_blocker_large_range_stays_on_sonnet() {
        // A >80-line BLOCKER is still single-file. Opus's advantage is
        // cross-file reasoning, which this classifier can't detect. Cap at
        // sonnet:high.
        let tier = classify_issue_tier("java:S9999", "BLOCKER", "big refactor", 120);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "high");
    }

    #[test]
    fn test_classify_cognitive_complexity_scales_effort_not_model() {
        // ≤40: sonnet:medium — single-method logic, model doesn't matter much
        let tier = classify_issue_tier("java:S3776", "MAJOR", "from 18 to the 15 allowed.", 30);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "medium");

        let tier = classify_issue_tier("java:S3776", "MAJOR", "from 35 to the 15 allowed", 50);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "medium");

        // 41-70: sonnet:high — deep single-method refactor, more thinking tokens
        let tier = classify_issue_tier("java:S3776", "CRITICAL", "from 65 to the 15 allowed", 100);
        assert_eq!(tier.model, "sonnet");
        assert_eq!(tier.effort, "high");

        // >70 extreme: opus:high (NOT max). Max reserved for multi-class dedup.
        let tier = classify_issue_tier("java:S3776", "CRITICAL", "from 108 to the 15 allowed", 200);
        assert_eq!(tier.model, "opus");
        assert_eq!(tier.effort, "high");
    }

    #[test]
    fn test_classify_issue_tier_never_returns_opus_max() {
        // `classify_issue_tier` must never return opus:max — that tier is
        // reserved for `classify_dedup_tier` (multi-class work by definition).
        // Try a broad set of severities and complexities; none should hit max.
        let cases = [
            ("java:S3776", "BLOCKER", "from 150 to the 15 allowed", 500),
            ("java:S3776", "CRITICAL", "from 200 to the 15 allowed", 1000),
            ("java:S9999", "BLOCKER", "whatever", 500),
        ];
        for (rule, sev, msg, lines) in cases {
            let tier = classify_issue_tier(rule, sev, msg, lines);
            assert_ne!(
                tier.effort, "max",
                "classify_issue_tier returned opus:max for ({rule}, {sev}, {lines}) — that tier is multi-class only"
            );
        }
    }

    #[test]
    fn test_classify_trivial_rule_uses_haiku() {
        let tier = classify_issue_tier("java:S1128", "MINOR", "unused import", 1);
        assert_eq!(tier.model, "haiku");
    }

    // --- Existing prompt tests ---

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
            "", // US-067: no boundary hints for this simple case
            None, // US-066: no compliance context
            None,
        );
        assert!(prompt.contains("src/calculator.py"));
        assert!(prompt.contains("1 uncovered line"));
        assert!(prompt.contains("pytest"));
        assert!(prompt.contains("test_math.py"));
        assert!(prompt.contains("Do NOT modify source code"));
        // Source snippets ARE embedded directly in the prompt
        assert!(prompt.contains("return a + b"));
        assert!(prompt.contains("UNCOVERED"));
        // No boundary hints → no mandatory categories section
        assert!(!prompt.contains("Mandatory test coverage categories"));
    }

    // US-066: when compliance_ctx is provided, the compliance section appears with trace block fields
    #[test]
    fn test_build_test_generation_prompt_with_compliance_context() {
        let ctx = ComplianceTraceContext::new(
            "20260412_143022_myproj",
            "SONAR:java:S2259",
        ).with_risk_class("C");
        let prompt = build_test_generation_prompt(
            "src/safety/DoseCalc.java",
            "1 uncovered",
            "snippet",
            "mvn test",
            "",
            "",
            "",
            Some(&ctx),
            None,
        );
        // Compliance section must be present
        assert!(prompt.contains("Compliance requirements"));
        // All @Reparo.* fields must appear
        assert!(prompt.contains("@Reparo.purpose"));
        assert!(prompt.contains("@Reparo.requirement"));
        assert!(prompt.contains("@Reparo.testType"));
        assert!(prompt.contains("@Reparo.riskClass"));
        assert!(prompt.contains("@Reparo.runId"));
        // Context values must be injected
        assert!(prompt.contains("SONAR:java:S2259"));
        assert!(prompt.contains("20260412_143022_myproj"));
        // Risk class C appears in the template example
        assert!(prompt.contains("@Reparo.riskClass  C"));
    }

    #[test]
    fn test_build_test_generation_prompt_without_compliance_context_no_section() {
        let prompt = build_test_generation_prompt(
            "src/x.rs", "1", "s", "cargo test", "", "", "", None, None,
        );
        assert!(!prompt.contains("Compliance requirements"));
        assert!(!prompt.contains("@Reparo.purpose"));
    }

    // US-067: when boundary_hints is provided, the mandatory categories section appears
    #[test]
    fn test_build_test_generation_prompt_with_boundary_hints() {
        let prompt = build_test_generation_prompt(
            "src/calc.rs",
            "1 uncovered",
            "snippet",
            "cargo test",
            "",
            "",
            "- Line 3: numeric comparison `> 100` — test boundary values",
            None,
            None,
        );
        assert!(prompt.contains("Mandatory test coverage categories"));
        assert!(prompt.contains("Boundary values"));
        assert!(prompt.contains("Negative / error paths"));
        assert!(prompt.contains("Null / empty inputs"));
        assert!(prompt.contains("Line 3: numeric comparison"));
    }

    // US-060: verify the stable prefix (rules + framework) appears BEFORE the
    // variable content (source file path, uncovered summary) so prompt caching
    // can reuse the same prefix across different files in one run.
    #[test]
    fn test_prompt_stable_prefix_before_variable_content() {
        let prompt = build_test_generation_prompt(
            "src/calculator.py",
            "1 uncovered line",
            "snippet",
            "pytest",
            "",
            "Test framework: pytest",
            "",
            None,
            None,
        );
        let rules_idx = prompt.find("Rules").expect("Rules section missing");
        let framework_idx = prompt.find("Framework:").expect("Framework section missing");
        let target_idx = prompt.find("Target file:").expect("Target file section missing");
        let gap_idx = prompt.find("Coverage gap:").expect("Coverage gap section missing");

        // Rules come first (stable)
        assert!(rules_idx < framework_idx, "Rules must precede Framework");
        // Framework context is stable across the run → before target file
        assert!(framework_idx < target_idx, "Framework must precede target file (stable first)");
        // Target file + coverage gap are variable → at the end
        assert!(target_idx < gap_idx, "Target file must precede coverage gap");
    }

    #[test]
    fn test_chunk_prompt_stable_prefix_first() {
        let prompt = build_chunk_test_prompt(
            "src/Service.java",
            "processOrder",
            "// snippet",
            1,
            3,
            "mvn test",
            "",
            "",
            None,
        );
        let rules_idx = prompt.find("Rules").expect("Rules section missing");
        let target_idx = prompt.find("Target file:").expect("Target file section missing");
        assert!(rules_idx < target_idx, "Rules must precede target file");
    }

    #[test]
    fn test_retry_prompt_stable_prefix_first() {
        let prompt = build_test_generation_retry_prompt(
            "src/Foo.java",
            "",
            "snippet",
            "mvn test",
            2,
            "FAILED",
            "",
        );
        let rules_idx = prompt.find("Rules").expect("Rules section missing");
        let retry_idx = prompt.find("Retry 2").expect("Retry marker missing");
        assert!(rules_idx < retry_idx, "Rules must precede Retry N");
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
        // When snippets are present, summary is omitted (US-057)
        assert!(!prompt.contains("1 line still uncovered"));
        assert!(prompt.contains("FAILED test_foo"));
        assert!(prompt.contains("raise ValueError"));
        assert!(prompt.contains("Do NOT modify source code"));
    }

    #[test]
    fn test_build_retry_prompt_summary_shown_when_no_snippets() {
        // When snippets are absent, the text summary is the only indicator
        let prompt = build_test_generation_retry_prompt(
            "src/main.py",
            "3 lines still uncovered",
            "",
            "pytest",
            2,
            "FAILED test_foo - AssertionError",
            "",
        );
        assert!(prompt.contains("Retry 2"));
        assert!(prompt.contains("3 lines still uncovered"));
        assert!(prompt.contains("Remaining gap"));
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
    fn build_fix_prompt_inverts_scope_for_test_files() {
        // Issue in a Java test file: source files should be off-limits
        let prompt = build_fix_prompt(
            "T-1", "CODE_SMELL", "MAJOR", "java:S1234", "issue in test",
            "src/test/java/com/example/FooTest.java", 10, 12, "rule",
        );
        assert!(prompt.contains("DO NOT modify any source files"), "prompt: {}", prompt);
        assert!(!prompt.contains("NEVER modify test files"), "wrong scope rule applied: {}", prompt);
    }

    #[test]
    fn is_test_path_detects_common_layouts() {
        assert!(is_test_path("src/test/java/com/example/FooTest.java"));
        assert!(is_test_path("src/test/java/com/example/FooIT.java"));
        assert!(is_test_path("tests/unit/test_foo.py"));
        assert!(is_test_path("src/components/Button.test.tsx"));
        assert!(is_test_path("src/components/Button.spec.ts"));
        assert!(is_test_path("internal/foo_test.go"));
        assert!(!is_test_path("src/main/java/com/example/Foo.java"));
        assert!(!is_test_path("src/components/Button.tsx"));
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
    fn test_classify_test_gen_tier_by_uncovered_lines() {
        let tiers = crate::config::TestGenTiers::default();

        // Few uncovered lines → low effort regardless of file size
        let small = classify_test_gen_tier(5, 1000, &tiers);
        assert_eq!(small.model, "sonnet");
        assert_eq!(small.effort, "low");

        // Medium uncovered count
        let med = classify_test_gen_tier(30, 50, &tiers);
        assert_eq!(med.effort, "medium");

        // Many uncovered lines → opus
        let large = classify_test_gen_tier(120, 200, &tiers);
        assert_eq!(large.model, "opus");
    }

    #[test]
    fn test_classify_chunk_test_gen_tier_method_level() {
        let tiers = crate::config::TestGenTiers::default();

        // Tiny method with few uncovered lines → haiku (trivial)
        let tiny = classify_chunk_test_gen_tier(3, 20, &tiers);
        assert_eq!(tiny.model, "haiku");
        assert_eq!(tiny.effort, "low");

        // Medium method
        let med = classify_chunk_test_gen_tier(15, 40, &tiers);
        assert_eq!(med.effort, "medium");

        // Large method
        let large = classify_chunk_test_gen_tier(40, 90, &tiers);
        assert_eq!(large.effort, "high");

        // Very complex method
        let complex = classify_chunk_test_gen_tier(70, 200, &tiers);
        assert_eq!(complex.model, "opus");
    }

    #[test]
    fn test_classify_chunk_tier_custom_tiers() {
        use crate::config::{TierSpec, TestGenTiers};
        let tiers = TestGenTiers {
            trivial: TierSpec::new("haiku", "low"),
            low: TierSpec::new("haiku", "medium"),  // override: use haiku for low too
            medium: TierSpec::new("sonnet", "low"),
            high: TierSpec::new("sonnet", "high"),
            complex: TierSpec::new("sonnet", "high"),  // no opus at all
        };

        // Low band → haiku/medium per custom config
        let low = classify_chunk_test_gen_tier(8, 35, &tiers);
        assert_eq!(low.model, "haiku");
        assert_eq!(low.effort, "medium");

        // Complex band → sonnet/high (no opus)
        let complex = classify_chunk_test_gen_tier(80, 200, &tiers);
        assert_eq!(complex.model, "sonnet");
        assert_eq!(complex.effort, "high");
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
