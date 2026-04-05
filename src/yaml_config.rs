//! YAML configuration file support (US-014).
//!
//! Loads `reparo.yaml` from the project root and merges with CLI/env config.
//! Priority: CLI > ENV > YAML > defaults.

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Top-level YAML config structure.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct YamlConfig {
    pub sonar: SonarYaml,
    pub git: GitYaml,
    pub execution: ExecutionYaml,
    pub commands: CommandsYaml,
    pub prompts: PromptsYaml,
    /// Documentation quality configuration (ISO 25000 / MDR compliance)
    #[serde(default)]
    pub documentation: DocumentationYaml,
    /// Pact/contract testing configuration. Presence of this section is what marks
    /// the pact phase as configured — missing section + no `--skip-pact` is an error.
    /// When present without an explicit `enabled`, it defaults to enabled (opt-out).
    pub pact: Option<PactYaml>,
    /// Files that Claude must never modify during fixes (reverted automatically).
    /// List of exact filenames (matched against the basename, case-insensitive).
    #[serde(default)]
    pub protected_files: Vec<String>,
    /// Test generation hints for framework-aware prompts (US-040)
    #[serde(default)]
    pub test_generation: TestGenerationYaml,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SonarYaml {
    pub project_id: Option<String>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub skip_scan: Option<bool>,
    pub scanner_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct GitYaml {
    pub branch: Option<String>,
    pub batch_size: Option<usize>,
    /// Commit message template. Placeholders: {type}, {scope}, {message}, {issue_key}, {rule}, {file}
    /// Example: "{type}({scope})[PROJ-123]: {message}"
    /// Default: "{type}({scope}): {message}"
    pub commit_format: Option<String>,
    /// Extra variables for commit format placeholders (e.g., gitlab_issue: "PROJ-123")
    #[serde(default)]
    pub commit_vars: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ExecutionYaml {
    pub max_issues: Option<usize>,
    /// Process issues in reverse severity order: least severe first (default: false)
    pub reverse_severity: Option<bool>,
    pub dry_run: Option<bool>,
    pub timeout: Option<u64>,
    pub log_format: Option<String>,
    pub test_timeout: Option<u64>,
    pub claude_timeout: Option<u64>,
    pub min_coverage: Option<f64>,
    pub min_file_coverage: Option<f64>,
    /// Run formatter and commit before starting fixes (default: true)
    pub format_on_start: Option<bool>,
    /// Enable coverage boost step (default: true)
    pub coverage_boost: Option<bool>,
    /// Number of test generation attempts for coverage per issue (default: 3)
    pub coverage_attempts: Option<u32>,
    /// Maximum coverage rounds per file during boost (default: 3, 0 = unlimited while improving)
    pub coverage_rounds: Option<u32>,
    /// Glob patterns to exclude from coverage boost (e.g., ["*.html", "**/generated/**"])
    #[serde(default)]
    pub coverage_exclude: Vec<String>,
    /// Files per wave before running the test suite once (default: 3)
    pub coverage_wave_size: Option<u32>,
    /// Files per coverage boost commit (0 = same as coverage_wave_size, 1 = one commit per file)
    pub coverage_commit_batch: Option<u32>,
    /// Stop coverage boost after N consecutive wave failures (0 = disabled, default: 5)
    pub max_boost_failures: Option<usize>,
    /// Retry failed wave files with error context in per-file fallback (default: true)
    pub retry_failed_wave_files: Option<bool>,
    /// Enable final validation — run full test suite after all fixes (default: true)
    pub final_validation: Option<bool>,
    /// Maximum repair attempts during final validation — all tests must pass (default: 5)
    pub final_validation_attempts: Option<u32>,
    /// Run deduplication step after fixes (default: true)
    pub dedup_on_completion: Option<bool>,
    /// Maximum deduplication iterations (default: 10)
    pub max_dedup: Option<usize>,
    /// Rebase fix branch onto latest base before push/PR (default: true)
    pub auto_rebase: Option<bool>,
}

/// Project commands that Reparo executes directly (no heuristics, no LLM).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CommandsYaml {
    /// Setup command — runs once before pre-flight (e.g., npm install)
    pub setup: Option<String>,
    /// Clean artifacts before each fix
    pub clean: Option<String>,
    /// Build/compile the project
    pub build: Option<String>,
    /// Run tests
    pub test: Option<String>,
    /// Run tests with coverage
    pub coverage: Option<String>,
    /// Format code after fix
    pub format: Option<String>,
    /// Lint/static analysis after tests (non-blocking)
    pub lint: Option<String>,
    /// Path to the coverage report file (bypasses auto-detection)
    pub coverage_report: Option<String>,
    /// Documentation generation/validation command (e.g., "npx typedoc", "javadoc", "pydoc")
    pub docs: Option<String>,
    /// Fast compile-only command for tests (e.g., "mvn test-compile"). Falls back to `build`.
    pub test_compile: Option<String>,
}

/// Documentation quality standards configuration.
/// Controls the final documentation compliance step (ISO 25000 / MDR).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DocumentationYaml {
    /// Enable documentation quality step (default: false — opt-in)
    pub enabled: Option<bool>,
    /// Documentation style per language.
    /// Values: "jsdoc", "tsdoc", "javadoc", "pydoc", "rustdoc", "godoc", "xmldoc", "doxygen"
    pub style: Option<String>,
    /// Standards to enforce. Values: "iso25000", "mdr", "custom"
    #[serde(default)]
    pub standards: Vec<String>,
    /// What to document. Defaults: ["public_api", "classes", "methods", "parameters", "returns"]
    #[serde(default)]
    pub scope: Vec<String>,
    /// Custom documentation rules/instructions (appended to the Claude prompt)
    pub rules: Option<String>,
    /// File patterns to include (e.g., ["src/**/*.ts", "src/**/*.java"])
    #[serde(default)]
    pub include: Vec<String>,
    /// File patterns to exclude (e.g., ["**/*.spec.ts", "**/*.test.ts"])
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Maximum files to process per run (0 = all)
    pub max_files: Option<usize>,
    /// Required documentation elements per method/function for compliance
    #[serde(default)]
    pub required_elements: Vec<String>,
}

/// Pact/contract testing configuration.
/// Controls the contract verification step between coverage check and fix.
/// When the section is present, the phase runs unless `--skip-pact` is passed;
/// sub-steps are opt-in and default to false.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PactYaml {
    /// Master switch. Defaults to true when the section is present (opt-out).
    /// Set to false to keep the configuration but disable the phase.
    pub enabled: Option<bool>,
    /// Path to pact files directory. Can be absolute or relative to project root.
    /// Supports external/shared pact directories across projects.
    pub pact_dir: Option<String>,
    /// This project's provider name (for provider verification)
    pub provider_name: Option<String>,
    /// This project's consumer name (for consumer-driven contracts)
    pub consumer_name: Option<String>,

    // --- Sub-step toggles (all default false) ---

    /// Check existing contracts before fix (default: false)
    pub check_contracts: Option<bool>,
    /// Generate contract tests with Claude if none exist (default: false)
    pub generate_tests: Option<bool>,
    /// Run contract verification before applying the fix (default: false)
    pub verify_before_fix: Option<bool>,
    /// Run contract verification after fix to ensure no regressions (default: false)
    pub verify_after_fix: Option<bool>,

    // --- Commands ---

    /// Command to verify pact contracts (e.g., "npm run test:pact:verify")
    pub verify_command: Option<String>,
    /// Command specifically for running contract tests (e.g., "npm run test:pact")
    pub test_command: Option<String>,

    // --- Tuning ---

    /// Retry count for contract test generation (default: 3)
    pub attempts: Option<u32>,
    /// File patterns that indicate API interaction (e.g., ["**/api/**", "**/services/**"]).
    /// If empty, all files are considered candidates when pact is enabled.
    #[serde(default)]
    pub api_patterns: Vec<String>,
}

/// Test generation hints for framework-aware prompts (US-040).
/// Allows users to specify the test framework, mock library, assertions, etc.
/// so that generated tests compile and work with the project's actual setup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TestGenerationYaml {
    /// Test framework override (e.g., "junit5", "junit4", "testng")
    pub framework: Option<String>,
    /// Mock framework (e.g., "mockito", "easymock")
    pub mock_framework: Option<String>,
    /// Assertion library (e.g., "assertj", "hamcrest")
    pub assertion_library: Option<String>,
    /// Avoid @SpringBootTest for unit tests (default: false)
    pub avoid_spring_context: Option<bool>,
    /// Custom instructions appended to every test generation prompt
    pub custom_instructions: Option<String>,
}

/// Prompt customization per rule or category (US-019).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct PromptsYaml {
    /// Per-rule hints: key is the SonarQube rule key (e.g. "java:S3776")
    pub rules: std::collections::HashMap<String, RulePrompt>,
    /// Per-category hints: key is "vulnerability", "code_smell", "bug", "security_hotspot"
    pub categories: std::collections::HashMap<String, CategoryPrompt>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RulePrompt {
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryPrompt {
    #[serde(default)]
    pub hint: Option<String>,
}

/// Resolve the prompt hint for a given issue rule and type (US-019).
/// Priority: exact rule match > rule prefix match > category match > None.
pub fn resolve_prompt_hint(prompts: &PromptsYaml, rule: &str, issue_type: &str) -> Option<String> {
    // 1. Exact rule match
    if let Some(rp) = prompts.rules.get(rule) {
        if let Some(ref hint) = rp.hint {
            return Some(hint.clone());
        }
    }

    // 2. Prefix match (e.g. "java:S" matches "java:S1234")
    for (pattern, rp) in &prompts.rules {
        if pattern.ends_with('*') {
            let prefix = &pattern[..pattern.len() - 1];
            if rule.starts_with(prefix) {
                if let Some(ref hint) = rp.hint {
                    return Some(hint.clone());
                }
            }
        }
    }

    // 3. Category match
    let category = issue_type.to_lowercase().replace(' ', "_");
    if let Some(cp) = prompts.categories.get(&category) {
        if let Some(ref hint) = cp.hint {
            return Some(hint.clone());
        }
    }

    None
}

/// Resolved project commands for use at runtime.
#[derive(Debug, Clone, Default)]
pub struct ProjectCommands {
    pub setup: Option<String>,
    pub clean: Option<String>,
    pub build: Option<String>,
    pub test: Option<String>,
    pub coverage: Option<String>,
    pub format: Option<String>,
    pub lint: Option<String>,
    /// Explicit path to the coverage report file (bypasses auto-detection)
    pub coverage_report: Option<String>,
    /// Fast compile-only command for tests (e.g., "mvn test-compile"). Falls back to `build`.
    pub test_compile: Option<String>,
}

impl ProjectCommands {
    pub fn has_any(&self) -> bool {
        self.setup.is_some()
            || self.clean.is_some()
            || self.build.is_some()
            || self.test.is_some()
            || self.coverage.is_some()
            || self.format.is_some()
            || self.lint.is_some()
    }

    pub fn print_summary(&self) {
        if !self.has_any() {
            return;
        }
        info!("  Project commands (from YAML):");
        if let Some(c) = &self.setup {
            info!("    setup:    {}", c);
        }
        if let Some(c) = &self.clean {
            info!("    clean:    {}", c);
        }
        if let Some(c) = &self.build {
            info!("    build:    {}", c);
        }
        if let Some(c) = &self.test {
            info!("    test:     {}", c);
        }
        if let Some(c) = &self.coverage {
            info!("    coverage: {}", c);
        }
        if let Some(c) = &self.format {
            info!("    format:   {}", c);
        }
        if let Some(c) = &self.lint {
            info!("    lint:     {}", c);
        }
    }
}

/// Try to load a YAML config file.
///
/// Looks for `reparo.yaml` or `reparo.yml` in the given directory,
/// or uses the explicit path if provided.
pub fn load_yaml_config(
    project_path: &Path,
    explicit_config: Option<&str>,
) -> Result<Option<YamlConfig>> {
    let config_path = if let Some(p) = explicit_config {
        let pb = std::path::PathBuf::from(p);
        if !pb.exists() {
            anyhow::bail!("Config file not found: {}", p);
        }
        pb
    } else {
        let yaml = project_path.join("reparo.yaml");
        let yml = project_path.join("reparo.yml");
        if yaml.exists() {
            yaml
        } else if yml.exists() {
            yml
        } else {
            return Ok(None);
        }
    };

    info!("Loading config from {}", config_path.display());
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    // Interpolate environment variables
    let interpolated = interpolate_env_vars(&raw);

    let config: YamlConfig = serde_yaml::from_str(&interpolated)
        .with_context(|| format!("Failed to parse YAML config: {}", config_path.display()))?;

    Ok(Some(config))
}

/// Replace `${VAR}` patterns with environment variable values.
/// Warns on undefined variables and replaces them with empty string.
fn interpolate_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        match std::env::var(var_name) {
            Ok(val) => val,
            Err(_) => {
                warn!(
                    "Environment variable '{}' referenced in YAML config is not set",
                    var_name
                );
                String::new()
            }
        }
    })
    .to_string()
}

/// Merge YAML config into a CLI Config, respecting priority: CLI > ENV > YAML > defaults.
///
/// clap already handles CLI > ENV > defaults. We only apply YAML values for fields
/// that are still at their default value (i.e., not set by CLI or ENV).
pub fn merge_yaml_into_config(
    config: &mut crate::config::Config,
    yaml: &YamlConfig,
) {
    // Helper: only apply if the current value is the default
    // For String fields with defaults
    if config.sonar_project_id.is_empty() {
        if let Some(ref v) = yaml.sonar.project_id {
            config.sonar_project_id = v.clone();
        }
    }
    if config.sonar_url == "http://localhost:9000" {
        if let Some(ref v) = yaml.sonar.url {
            config.sonar_url = v.clone();
        }
    }
    if config.sonar_token.is_empty() {
        if let Some(ref v) = yaml.sonar.token {
            config.sonar_token = v.clone();
        }
    }
    // NOTE: git.branch from YAML is NOT merged into config.branch here.
    // config.branch is only set via CLI --branch. If not set, validate() detects
    // the current checked-out branch. This ensures re-running on a fix branch
    // continues there instead of always jumping back to the YAML-configured branch.
    // YAML git.branch is stored separately as yaml_base_branch for PR target.
    if config.batch_size == 1 {
        if let Some(v) = yaml.git.batch_size {
            config.batch_size = v;
        }
    }
    if config.test_command.is_none() {
        config.test_command = yaml.commands.test.clone();
    }
    if config.coverage_command.is_none() {
        config.coverage_command = yaml.commands.coverage.clone();
    }
    if !config.dry_run {
        if let Some(v) = yaml.execution.dry_run {
            config.dry_run = v;
        }
    }
    if config.max_issues == 0 {
        if let Some(v) = yaml.execution.max_issues {
            config.max_issues = v;
        }
    }
    // reverse_severity: YAML can enable reverse order (CLI wins if set)
    if !config.reverse_severity {
        if let Some(true) = yaml.execution.reverse_severity {
            config.reverse_severity = true;
        }
    }
    if config.log_format == "text" {
        if let Some(ref v) = yaml.execution.log_format {
            config.log_format = v.clone();
        }
    }
    if config.test_timeout == 600 {
        if let Some(v) = yaml.execution.test_timeout {
            config.test_timeout = v;
        }
    }
    if !config.skip_scan {
        if let Some(v) = yaml.sonar.skip_scan {
            config.skip_scan = v;
        }
    }
    if config.scanner_path.is_none() {
        config.scanner_path = yaml.sonar.scanner_path.clone();
    }
    if config.timeout == 0 {
        if let Some(v) = yaml.execution.timeout {
            config.timeout = v;
        }
    }
    if config.claude_timeout == 300 {
        if let Some(v) = yaml.execution.claude_timeout {
            config.claude_timeout = v;
        }
    }
    // min_coverage: only override if CLI is at default (80)
    if (config.min_coverage - 80.0).abs() < f64::EPSILON {
        if let Some(v) = yaml.execution.min_coverage {
            config.min_coverage = v;
        }
    }
    // min_file_coverage: only override if CLI is at default (0)
    if config.min_file_coverage == 0.0 {
        if let Some(v) = yaml.execution.min_file_coverage {
            config.min_file_coverage = v;
        }
    }
    // format_on_start: YAML can disable initial formatting (default: true)
    if !config.skip_format {
        if let Some(false) = yaml.execution.format_on_start {
            config.skip_format = true;
        }
    }
    // coverage_boost: YAML can disable coverage boost (default: true)
    if !config.skip_coverage {
        if let Some(false) = yaml.execution.coverage_boost {
            config.skip_coverage = true;
        }
    }
    // coverage_attempts: only override if CLI is at default (3)
    if config.coverage_attempts == 3 {
        if let Some(v) = yaml.execution.coverage_attempts {
            config.coverage_attempts = v;
        }
    }
    // coverage_rounds: only override if CLI is at default (3)
    if config.coverage_rounds == 3 {
        if let Some(v) = yaml.execution.coverage_rounds {
            config.coverage_rounds = v;
        }
    }
    // coverage_exclude: YAML provides glob patterns (no CLI equivalent)
    if config.coverage_exclude.is_empty() && !yaml.execution.coverage_exclude.is_empty() {
        config.coverage_exclude = yaml.execution.coverage_exclude.clone();
    }
    // coverage_wave_size: only override if CLI is at default (3)
    if config.coverage_wave_size == 3 {
        if let Some(v) = yaml.execution.coverage_wave_size {
            config.coverage_wave_size = v;
        }
    }
    // coverage_commit_batch: only override if CLI is at default (0)
    if config.coverage_commit_batch == 0 {
        if let Some(v) = yaml.execution.coverage_commit_batch {
            config.coverage_commit_batch = v;
        }
    }
    // max_boost_failures: only override if CLI is at default (5)
    if config.max_boost_failures == 5 {
        if let Some(v) = yaml.execution.max_boost_failures {
            config.max_boost_failures = v;
        }
    }
    // retry_failed_wave_files: YAML can disable retry (default: true)
    if !config.skip_retry_failed_wave_files {
        if let Some(false) = yaml.execution.retry_failed_wave_files {
            config.skip_retry_failed_wave_files = true;
        }
    }
    // final_validation: YAML can disable final validation (default: true)
    if !config.skip_final_validation {
        if let Some(false) = yaml.execution.final_validation {
            config.skip_final_validation = true;
        }
    }
    // final_validation_attempts: only override if CLI is at default (5)
    if config.final_validation_attempts == 5 {
        if let Some(v) = yaml.execution.final_validation_attempts {
            config.final_validation_attempts = v;
        }
    }
    // dedup_on_completion: YAML can disable dedup (default: true)
    if !config.skip_dedup {
        if let Some(false) = yaml.execution.dedup_on_completion {
            config.skip_dedup = true;
        }
    }
    // max_dedup: only override if CLI is at default (10)
    if config.max_dedup == 10 {
        if let Some(v) = yaml.execution.max_dedup {
            config.max_dedup = v;
        }
    }
    // auto_rebase: YAML can disable pre-push rebase (default: true)
    if !config.skip_rebase {
        if let Some(false) = yaml.execution.auto_rebase {
            config.skip_rebase = true;
        }
    }
    // protected_files: always take from YAML (no CLI equivalent)
    if !yaml.protected_files.is_empty() {
        config.protected_files = yaml.protected_files.clone();
    }
    // commit_format: always take from YAML (no CLI equivalent)
    if let Some(ref fmt) = yaml.git.commit_format {
        config.commit_format = fmt.clone();
    }
    // commit_vars: always take from YAML
    if !yaml.git.commit_vars.is_empty() {
        config.commit_vars = yaml.git.commit_vars.clone();
    }
    // documentation: resolve from YAML
    let doc = &yaml.documentation;
    config.documentation = crate::config::DocumentationConfig {
        enabled: doc.enabled.unwrap_or(false),
        style: doc.style.clone().unwrap_or_default(),
        standards: doc.standards.clone(),
        scope: if doc.scope.is_empty() {
            vec![
                "public_api".to_string(),
                "classes".to_string(),
                "methods".to_string(),
                "parameters".to_string(),
                "returns".to_string(),
            ]
        } else {
            doc.scope.clone()
        },
        rules: doc.rules.clone(),
        include: doc.include.clone(),
        exclude: if doc.exclude.is_empty() {
            vec![
                "**/*.spec.*".to_string(),
                "**/*.test.*".to_string(),
                "**/node_modules/**".to_string(),
            ]
        } else {
            doc.exclude.clone()
        },
        max_files: doc.max_files.unwrap_or(0),
        required_elements: if doc.required_elements.is_empty() {
            vec![
                "description".to_string(),
                "params".to_string(),
                "returns".to_string(),
            ]
        } else {
            doc.required_elements.clone()
        },
        docs_command: yaml.commands.docs.clone(),
    };
    // pact: resolve from YAML. Section missing → configured=false, which causes
    // `PactConfig::validate()` to bail unless --skip-pact is set.
    config.pact = match &yaml.pact {
        Some(pact) => crate::config::PactConfig {
            configured: true,
            // Default to enabled when the section is present (opt-out via --skip-pact).
            enabled: pact.enabled.unwrap_or(true),
            pact_dir: pact.pact_dir.clone(),
            provider_name: pact.provider_name.clone(),
            consumer_name: pact.consumer_name.clone(),
            check_contracts: pact.check_contracts.unwrap_or(false),
            generate_tests: pact.generate_tests.unwrap_or(false),
            verify_before_fix: pact.verify_before_fix.unwrap_or(false),
            verify_after_fix: pact.verify_after_fix.unwrap_or(false),
            verify_command: pact.verify_command.clone(),
            test_command: pact.test_command.clone(),
            attempts: pact.attempts.unwrap_or(3),
            api_patterns: pact.api_patterns.clone(),
        },
        None => crate::config::PactConfig::default(),
    };

    // test_generation: resolve from YAML (US-040)
    let tg = &yaml.test_generation;
    config.test_generation = crate::config::TestGenerationConfig {
        framework: tg.framework.clone(),
        mock_framework: tg.mock_framework.clone(),
        assertion_library: tg.assertion_library.clone(),
        avoid_spring_context: tg.avoid_spring_context.unwrap_or(false),
        custom_instructions: tg.custom_instructions.clone(),
    };
}

/// Extract ProjectCommands from YAML, with CLI overrides.
pub fn resolve_commands(
    yaml: Option<&YamlConfig>,
    cli_test_command: &Option<String>,
    cli_coverage_command: &Option<String>,
) -> ProjectCommands {
    let base = yaml
        .map(|y| &y.commands)
        .cloned()
        .unwrap_or_default();

    ProjectCommands {
        setup: base.setup,
        clean: base.clean,
        build: base.build,
        // CLI overrides YAML for test/coverage
        test: cli_test_command.clone().or(base.test),
        coverage: cli_coverage_command.clone().or(base.coverage),
        format: base.format,
        lint: base.lint,
        coverage_report: base.coverage_report,
        test_compile: base.test_compile,
    }
}

/// Validate that command binaries exist in PATH.
pub fn validate_commands(commands: &ProjectCommands, project_path: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let cmds = [
        ("setup", &commands.setup),
        ("clean", &commands.clean),
        ("build", &commands.build),
        ("test", &commands.test),
        ("coverage", &commands.coverage),
        ("format", &commands.format),
        ("lint", &commands.lint),
    ];

    for (name, cmd) in &cmds {
        if let Some(cmd_str) = cmd {
            let binary = cmd_str.split_whitespace().next().unwrap_or("");
            if !binary.is_empty() {
                // Check if it's a path relative to the project or in PATH
                let abs = project_path.join(binary);
                if !abs.exists() && which::which(binary).is_err() {
                    warnings.push(format!(
                        "Command '{}' binary '{}' not found in PATH",
                        name, binary
                    ));
                }
            }
        }
    }

    warnings
}

// ---------------------------------------------------------------------------
// Personal configuration (~/.config/reparo/config.yaml)
// ---------------------------------------------------------------------------

/// Personal config structure stored in ~/.config/reparo/config.yaml.
///
/// Contains engine routing and user-level defaults that apply across all projects.
/// Priority: CLI > ENV > project YAML > personal YAML > defaults.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct PersonalConfig {
    /// Version of Reparo that last wrote this file
    pub reparo_version: Option<String>,
    /// AI engine definitions
    pub engines: HashMap<String, crate::engine::EngineConfig>,
    /// Tier-to-engine routing
    pub routing: crate::engine::RoutingConfig,
    /// Personal execution defaults
    pub execution: ExecutionYaml,
    /// Personal sonar defaults
    pub sonar: SonarYaml,
    /// Personal git defaults
    pub git: GitYaml,
}

/// Get the path to the personal config file: ~/.config/reparo/config.yaml
pub fn personal_config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .context("Could not determine user config directory")?;
    Ok(config_dir.join("reparo").join("config.yaml"))
}

/// Build the default personal config.
pub fn default_personal_config() -> PersonalConfig {
    PersonalConfig {
        reparo_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        engines: crate::engine::default_engines(),
        routing: crate::engine::default_routing(),
        execution: ExecutionYaml::default(),
        sonar: SonarYaml::default(),
        git: GitYaml::default(),
    }
}

/// Load the personal config from ~/.config/reparo/config.yaml.
///
/// - If the file does not exist, creates it with defaults and returns the defaults.
/// - If the file exists but is missing fields, `serde(default)` fills them in.
/// - If `reparo_version` doesn't match the current version, emits a warning.
/// - If `reparo_version` was missing, stamps it and rewrites the file.
pub fn load_personal_config() -> Result<PersonalConfig> {
    let path = personal_config_path()?;

    if !path.exists() {
        // Create with defaults
        let config = default_personal_config();
        write_personal_config(&path, &config)?;
        info!("Created personal config at {}", path.display());
        return Ok(config);
    }

    // Load and parse
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read personal config: {}", path.display()))?;

    let mut config: PersonalConfig = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse personal config: {}", path.display()))?;

    // Version check
    let current_version = env!("CARGO_PKG_VERSION");
    match &config.reparo_version {
        Some(stored) if stored != current_version => {
            warn!(
                "Personal config {} was created by Reparo v{}, \
                 but you are running v{}. Run --restore-personal-yaml to update defaults.",
                path.display(), stored, current_version
            );
        }
        None => {
            // Stamp version and rewrite
            config.reparo_version = Some(current_version.to_string());
            write_personal_config(&path, &config)?;
            info!("Stamped version {} in personal config", current_version);
        }
        _ => {}
    }

    Ok(config)
}

/// Restore the personal config to defaults for the current program version.
pub fn restore_personal_config() -> Result<()> {
    let path = personal_config_path()?;
    let config = default_personal_config();
    write_personal_config(&path, &config)?;
    Ok(())
}

/// Write the personal config to disk, creating parent directories if needed.
fn write_personal_config(path: &Path, config: &PersonalConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }
    let yaml = serde_yaml::to_string(config)
        .context("Failed to serialize personal config")?;
    std::fs::write(path, yaml)
        .with_context(|| format!("Failed to write personal config: {}", path.display()))?;
    Ok(())
}

/// Merge personal config values into Config at lower priority than project YAML.
///
/// Uses the same "only if at default" logic as merge_yaml_into_config.
/// Called BEFORE merge_yaml_into_config so project values win.
pub fn merge_personal_into_config(config: &mut crate::config::Config, personal: &PersonalConfig) {
    // Merge execution defaults
    let exec = &personal.execution;
    if let Some(timeout) = exec.timeout {
        if config.timeout == 0 {
            config.timeout = timeout;
        }
    }
    if let Some(ct) = exec.claude_timeout {
        if config.claude_timeout == crate::claude::DEFAULT_CLAUDE_TIMEOUT {
            config.claude_timeout = ct;
        }
    }
    if let Some(max) = exec.max_issues {
        if config.max_issues == 0 {
            config.max_issues = max;
        }
    }
    if let Some(tt) = exec.test_timeout {
        if config.test_timeout == 600 {
            config.test_timeout = tt;
        }
    }
    if let Some(ref lf) = exec.log_format {
        if config.log_format == "text" {
            config.log_format = lf.clone();
        }
    }

    // Merge sonar defaults
    let sonar = &personal.sonar;
    if let Some(ref url) = sonar.url {
        if config.sonar_url == "http://localhost:9000" {
            config.sonar_url = url.clone();
        }
    }
    if let Some(ref token) = sonar.token {
        if config.sonar_token.is_empty() {
            config.sonar_token = token.clone();
        }
    }

    // Merge coverage wave size
    if let Some(v) = exec.coverage_wave_size {
        if config.coverage_wave_size == 3 {
            config.coverage_wave_size = v;
        }
    }
    if let Some(v) = exec.coverage_commit_batch {
        if config.coverage_commit_batch == 0 {
            config.coverage_commit_batch = v;
        }
    }
    if let Some(v) = exec.max_boost_failures {
        if config.max_boost_failures == 5 {
            config.max_boost_failures = v;
        }
    }
    // Merge git defaults
    let git = &personal.git;
    if let Some(bs) = git.batch_size {
        if config.batch_size == 1 {
            config.batch_size = bs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_interpolate_env_vars() {
        std::env::set_var("REPARO_TEST_VAR_A", "hello");
        let result = interpolate_env_vars("url: ${REPARO_TEST_VAR_A}/api");
        assert_eq!(result, "url: hello/api");
        std::env::remove_var("REPARO_TEST_VAR_A");
    }

    #[test]
    fn test_interpolate_env_vars_missing() {
        let result = interpolate_env_vars("token: ${REPARO_NONEXISTENT_VAR_XYZ}");
        assert_eq!(result, "token: ");
    }

    #[test]
    fn test_interpolate_multiple_vars() {
        std::env::set_var("REPARO_TEST_X", "foo");
        std::env::set_var("REPARO_TEST_Y", "bar");
        let result = interpolate_env_vars("${REPARO_TEST_X}-${REPARO_TEST_Y}");
        assert_eq!(result, "foo-bar");
        std::env::remove_var("REPARO_TEST_X");
        std::env::remove_var("REPARO_TEST_Y");
    }

    #[test]
    fn test_parse_full_yaml() {
        let yaml_str = r#"
sonar:
  project_id: "my-proj"
  url: "https://sonar.example.com"
  token: "tok123"
  skip_scan: true

git:
  branch: "develop"
  batch_size: 5

execution:
  max_issues: 10
  dry_run: false
  timeout: 1800
  log_format: "json"
  test_timeout: 300

commands:
  clean: "mvn clean"
  build: "mvn compile -DskipTests"
  test: "mvn test"
  coverage: "mvn verify -Pcoverage"
  format: "mvn spotless:apply"
  lint: "mvn checkstyle:check"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(config.sonar.project_id.unwrap(), "my-proj");
        assert_eq!(config.sonar.url.unwrap(), "https://sonar.example.com");
        assert_eq!(config.git.branch.unwrap(), "develop");
        assert_eq!(config.git.batch_size.unwrap(), 5);
        assert_eq!(config.execution.timeout.unwrap(), 1800);
        assert_eq!(config.commands.build.unwrap(), "mvn compile -DskipTests");
        assert_eq!(config.commands.lint.unwrap(), "mvn checkstyle:check");
    }

    #[test]
    fn test_parse_minimal_yaml() {
        let yaml_str = r#"
sonar:
  project_id: "test"
commands:
  test: "npm test"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(config.sonar.project_id.unwrap(), "test");
        assert_eq!(config.commands.test.unwrap(), "npm test");
        assert!(config.commands.build.is_none());
        assert!(config.commands.clean.is_none());
        assert!(config.git.branch.is_none());
    }

    #[test]
    fn test_parse_empty_yaml() {
        let config: YamlConfig = serde_yaml::from_str("").unwrap();
        assert!(config.sonar.project_id.is_none());
        assert!(config.commands.test.is_none());
    }

    #[test]
    fn test_parse_test_generation_yaml() {
        let yaml_str = r#"
sonar:
  project_id: "demo"
test_generation:
  framework: "junit5"
  mock_framework: "mockito"
  assertion_library: "assertj"
  avoid_spring_context: true
  custom_instructions: "Always use @ExtendWith(MockitoExtension.class)"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        let tg = &config.test_generation;
        assert_eq!(tg.framework.as_deref(), Some("junit5"));
        assert_eq!(tg.mock_framework.as_deref(), Some("mockito"));
        assert_eq!(tg.assertion_library.as_deref(), Some("assertj"));
        assert_eq!(tg.avoid_spring_context, Some(true));
        assert!(tg.custom_instructions.as_deref().unwrap().contains("MockitoExtension"));
    }

    #[test]
    fn test_resolve_commands_yaml_only() {
        let yaml = YamlConfig {
            commands: CommandsYaml {
                test: Some("pytest".to_string()),
                build: Some("make build".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = resolve_commands(Some(&yaml), &None, &None);
        assert_eq!(cmds.test.unwrap(), "pytest");
        assert_eq!(cmds.build.unwrap(), "make build");
    }

    #[test]
    fn test_resolve_commands_cli_overrides_yaml() {
        let yaml = YamlConfig {
            commands: CommandsYaml {
                test: Some("pytest".to_string()),
                coverage: Some("pytest --cov".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cli_test = Some("npm test".to_string());
        let cmds = resolve_commands(Some(&yaml), &cli_test, &None);
        assert_eq!(cmds.test.unwrap(), "npm test"); // CLI wins
        assert_eq!(cmds.coverage.unwrap(), "pytest --cov"); // YAML kept
    }

    #[test]
    fn test_resolve_commands_no_yaml() {
        let cmds = resolve_commands(None, &None, &None);
        assert!(cmds.test.is_none());
        assert!(cmds.build.is_none());
        assert!(!cmds.has_any());
    }

    #[test]
    fn test_resolve_commands_coverage_report_passthrough() {
        let yaml = YamlConfig {
            commands: CommandsYaml {
                coverage_report: Some("target/site/jacoco/jacoco.xml".to_string()),
                coverage: Some("mvn test jacoco:report".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = resolve_commands(Some(&yaml), &None, &None);
        assert_eq!(cmds.coverage_report.as_deref(), Some("target/site/jacoco/jacoco.xml"));
        assert_eq!(cmds.coverage.as_deref(), Some("mvn test jacoco:report"));
    }

    #[test]
    fn test_resolve_commands_test_compile_passthrough() {
        let yaml = YamlConfig {
            commands: CommandsYaml {
                test_compile: Some("mvn test-compile".to_string()),
                build: Some("mvn compile".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = resolve_commands(Some(&yaml), &None, &None);
        assert_eq!(cmds.test_compile.as_deref(), Some("mvn test-compile"));
        assert_eq!(cmds.build.as_deref(), Some("mvn compile"));
    }

    #[test]
    fn test_project_commands_has_any() {
        let empty = ProjectCommands::default();
        assert!(!empty.has_any());

        let with_build = ProjectCommands {
            build: Some("make".to_string()),
            ..Default::default()
        };
        assert!(with_build.has_any());
    }

    #[test]
    fn test_load_yaml_config_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("reparo.yaml"),
            "sonar:\n  project_id: \"from-yaml\"\ncommands:\n  test: \"cargo test\"\n",
        )
        .unwrap();

        let config = load_yaml_config(tmp.path(), None).unwrap();
        assert!(config.is_some());
        let c = config.unwrap();
        assert_eq!(c.sonar.project_id.unwrap(), "from-yaml");
        assert_eq!(c.commands.test.unwrap(), "cargo test");
    }

    #[test]
    fn test_load_yaml_config_yml_extension() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("reparo.yml"),
            "commands:\n  build: \"make\"\n",
        )
        .unwrap();

        let config = load_yaml_config(tmp.path(), None).unwrap();
        assert!(config.is_some());
        assert_eq!(config.unwrap().commands.build.unwrap(), "make");
    }

    #[test]
    fn test_load_yaml_config_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load_yaml_config(tmp.path(), None).unwrap();
        assert!(config.is_none());
    }

    #[test]
    fn test_load_yaml_config_explicit_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("my-config.yaml");
        std::fs::write(&cfg_path, "sonar:\n  project_id: \"explicit\"\n").unwrap();

        let config = load_yaml_config(tmp.path(), Some(cfg_path.to_str().unwrap())).unwrap();
        assert_eq!(config.unwrap().sonar.project_id.unwrap(), "explicit");
    }

    #[test]
    fn test_load_yaml_config_explicit_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_yaml_config(tmp.path(), Some("/nonexistent.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_commands_existing_binary() {
        let cmds = ProjectCommands {
            test: Some("echo hello".to_string()), // echo always exists
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let warnings = validate_commands(&cmds, tmp.path());
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_validate_commands_missing_binary() {
        let cmds = ProjectCommands {
            build: Some("nonexistent_binary_xyz_123 --flag".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let warnings = validate_commands(&cmds, tmp.path());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonexistent_binary_xyz_123"));
    }

    // -- Prompt hints (US-019) --

    #[test]
    fn test_resolve_prompt_hint_exact_rule() {
        let mut prompts = PromptsYaml::default();
        prompts.rules.insert(
            "java:S3776".to_string(),
            RulePrompt {
                strategy: Some("refactor".to_string()),
                hint: Some("Extract methods to reduce complexity".to_string()),
            },
        );
        let hint = resolve_prompt_hint(&prompts, "java:S3776", "CODE_SMELL");
        assert_eq!(hint.unwrap(), "Extract methods to reduce complexity");
    }

    #[test]
    fn test_resolve_prompt_hint_prefix_match() {
        let mut prompts = PromptsYaml::default();
        prompts.rules.insert(
            "java:*".to_string(),
            RulePrompt {
                strategy: None,
                hint: Some("Follow Java conventions".to_string()),
            },
        );
        let hint = resolve_prompt_hint(&prompts, "java:S9999", "BUG");
        assert_eq!(hint.unwrap(), "Follow Java conventions");
    }

    #[test]
    fn test_resolve_prompt_hint_category_fallback() {
        let mut prompts = PromptsYaml::default();
        prompts.categories.insert(
            "vulnerability".to_string(),
            CategoryPrompt {
                hint: Some("Follow OWASP guidelines".to_string()),
            },
        );
        let hint = resolve_prompt_hint(&prompts, "unknown:rule", "VULNERABILITY");
        assert_eq!(hint.unwrap(), "Follow OWASP guidelines");
    }

    #[test]
    fn test_resolve_prompt_hint_none() {
        let prompts = PromptsYaml::default();
        let hint = resolve_prompt_hint(&prompts, "java:S1234", "BUG");
        assert!(hint.is_none());
    }

    #[test]
    fn test_parse_yaml_with_pact() {
        let yaml_str = r#"
sonar:
  project_id: "test"
pact:
  enabled: true
  pact_dir: "../shared-pacts"
  provider_name: "UserService"
  consumer_name: "WebApp"
  check_contracts: true
  generate_tests: true
  verify_before_fix: true
  verify_after_fix: true
  verify_command: "npm run test:pact:verify"
  test_command: "npm run test:pact"
  attempts: 5
  api_patterns:
    - "**/api/**"
    - "**/services/**"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        let pact = config.pact.expect("pact section should be present");
        assert_eq!(pact.enabled, Some(true));
        assert_eq!(pact.pact_dir.as_deref(), Some("../shared-pacts"));
        assert_eq!(pact.provider_name.as_deref(), Some("UserService"));
        assert_eq!(pact.consumer_name.as_deref(), Some("WebApp"));
        assert_eq!(pact.check_contracts, Some(true));
        assert_eq!(pact.generate_tests, Some(true));
        assert_eq!(pact.verify_before_fix, Some(true));
        assert_eq!(pact.verify_after_fix, Some(true));
        assert_eq!(pact.verify_command.as_deref(), Some("npm run test:pact:verify"));
        assert_eq!(pact.test_command.as_deref(), Some("npm run test:pact"));
        assert_eq!(pact.attempts, Some(5));
        assert_eq!(pact.api_patterns.len(), 2);
    }

    #[test]
    fn test_parse_yaml_pact_minimal() {
        let yaml_str = r#"
pact:
  enabled: true
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        let pact = config.pact.expect("pact section should be present");
        assert_eq!(pact.enabled, Some(true));
        assert!(pact.pact_dir.is_none());
        assert!(pact.provider_name.is_none());
        assert!(pact.check_contracts.is_none());
        assert!(pact.generate_tests.is_none());
        assert!(pact.verify_before_fix.is_none());
        assert!(pact.verify_after_fix.is_none());
        assert!(pact.verify_command.is_none());
        assert!(pact.test_command.is_none());
        assert!(pact.attempts.is_none());
        assert!(pact.api_patterns.is_empty());
    }

    #[test]
    fn test_parse_yaml_pact_empty() {
        // Empty YAML → no pact section → None (marks phase as not configured).
        let config: YamlConfig = serde_yaml::from_str("").unwrap();
        assert!(config.pact.is_none());
    }

    #[test]
    fn test_parse_yaml_pact_section_without_enabled_defaults_to_none_in_yaml() {
        // Presence of the section alone is what marks it configured; the
        // opt-out default is applied later during merging, not here.
        let yaml_str = r#"
pact:
  provider_name: "API"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        let pact = config.pact.expect("pact section should be present");
        assert!(pact.enabled.is_none());
        assert_eq!(pact.provider_name.as_deref(), Some("API"));
    }

    #[test]
    fn test_resolve_prompt_hint_rule_over_category() {
        let mut prompts = PromptsYaml::default();
        prompts.rules.insert(
            "java:S1234".to_string(),
            RulePrompt {
                strategy: None,
                hint: Some("Specific rule hint".to_string()),
            },
        );
        prompts.categories.insert(
            "bug".to_string(),
            CategoryPrompt {
                hint: Some("Generic bug hint".to_string()),
            },
        );
        let hint = resolve_prompt_hint(&prompts, "java:S1234", "BUG");
        assert_eq!(hint.unwrap(), "Specific rule hint"); // rule wins over category
    }

    #[test]
    fn test_parse_yaml_with_prompts() {
        let yaml_str = r#"
sonar:
  project_id: "test"
prompts:
  rules:
    "java:S3776":
      strategy: "refactor"
      hint: "Extract helper methods"
    "python:*":
      hint: "Follow PEP 8"
  categories:
    vulnerability:
      hint: "Check OWASP Top 10"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(config.prompts.rules.len(), 2);
        assert!(config.prompts.rules.contains_key("java:S3776"));
        assert!(config.prompts.categories.contains_key("vulnerability"));
    }

    // --- Personal config tests ---

    #[test]
    fn test_default_personal_config_has_version() {
        let config = default_personal_config();
        assert_eq!(
            config.reparo_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn test_default_personal_config_has_all_engines() {
        let config = default_personal_config();
        assert!(config.engines.contains_key("claude"));
        assert!(config.engines.contains_key("gemini"));
        assert!(config.engines.contains_key("aider"));
        assert!(config.engines["claude"].enabled);
        assert!(!config.engines["gemini"].enabled);
        assert!(!config.engines["aider"].enabled);
    }

    #[test]
    fn test_default_personal_config_routing_all_claude() {
        let config = default_personal_config();
        assert_eq!(config.routing.tier1.as_ref().unwrap().engine, "claude");
        assert_eq!(config.routing.tier4.as_ref().unwrap().engine, "claude");
    }

    #[test]
    fn test_personal_config_serialization_roundtrip() {
        let config = default_personal_config();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: PersonalConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.reparo_version, config.reparo_version);
        assert_eq!(parsed.engines.len(), config.engines.len());
        assert!(parsed.routing.tier1.is_some());
    }

    #[test]
    fn test_personal_config_missing_fields_get_defaults() {
        let yaml = "reparo_version: \"0.1.0\"\n";
        let config: PersonalConfig = serde_yaml::from_str(yaml).unwrap();
        // engines should default to empty (no engines auto-created from partial YAML)
        // routing should default to None tiers
        assert_eq!(config.reparo_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn test_write_and_read_personal_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let config = default_personal_config();
        write_personal_config(&path, &config).unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: PersonalConfig = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed.reparo_version, config.reparo_version);
    }

    #[test]
    fn test_write_personal_config_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("config.yaml");
        let config = default_personal_config();
        write_personal_config(&path, &config).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_merge_personal_into_config_sonar_url() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
        ]);
        let mut personal = default_personal_config();
        personal.sonar.url = Some("https://sonar.example.com".to_string());

        // Default sonar_url is "http://localhost:9000"
        merge_personal_into_config(&mut config, &personal);
        assert_eq!(config.sonar_url, "https://sonar.example.com");
    }

    #[test]
    fn test_merge_personal_into_config_cli_wins() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
            "--sonar-url", "https://cli-value.com",
        ]);
        let mut personal = default_personal_config();
        personal.sonar.url = Some("https://personal-value.com".to_string());

        // CLI value should NOT be overwritten by personal config
        merge_personal_into_config(&mut config, &personal);
        assert_eq!(config.sonar_url, "https://cli-value.com");
    }

    #[test]
    fn test_yaml_max_boost_failures_override() {
        let yaml_str = r#"
execution:
  max_boost_failures: 10
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(yaml.execution.max_boost_failures, Some(10));
    }

    #[test]
    fn test_yaml_max_boost_failures_default_absent() {
        let yaml_str = r#"
execution:
  coverage_wave_size: 5
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(yaml.execution.max_boost_failures, None);
    }

    #[test]
    fn test_merge_yaml_max_boost_failures() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
        ]);
        assert_eq!(config.max_boost_failures, 5); // default

        let yaml_str = r#"
execution:
  max_boost_failures: 8
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        merge_yaml_into_config(&mut config, &yaml);
        assert_eq!(config.max_boost_failures, 8);
    }

    #[test]
    fn test_merge_yaml_max_boost_failures_cli_wins() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
            "--max-boost-failures", "3",
        ]);
        assert_eq!(config.max_boost_failures, 3);

        let yaml_str = r#"
execution:
  max_boost_failures: 10
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        merge_yaml_into_config(&mut config, &yaml);
        // CLI value (3) != default (5), so YAML should NOT override
        assert_eq!(config.max_boost_failures, 3);
    }

    #[test]
    fn test_parse_yaml_test_compile() {
        let yaml_str = r#"
commands:
  build: "mvn compile -DskipTests"
  test: "mvn test"
  test_compile: "mvn test-compile"
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(config.commands.test_compile.as_deref(), Some("mvn test-compile"));
        assert_eq!(config.commands.build.as_deref(), Some("mvn compile -DskipTests"));
    }

    #[test]
    fn test_resolve_commands_test_compile_none_by_default() {
        let cmds = resolve_commands(None, &None, &None);
        assert!(cmds.test_compile.is_none());
    }

    #[test]
    fn test_parse_yaml_retry_failed_wave_files() {
        let yaml_str = r#"
execution:
  retry_failed_wave_files: false
"#;
        let config: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert_eq!(config.execution.retry_failed_wave_files, Some(false));
    }

    #[test]
    fn test_merge_yaml_retry_failed_wave_files_disables() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
        ]);
        assert!(!config.skip_retry_failed_wave_files);

        let yaml_str = r#"
execution:
  retry_failed_wave_files: false
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        merge_yaml_into_config(&mut config, &yaml);
        assert!(config.skip_retry_failed_wave_files);
    }

    #[test]
    fn test_merge_yaml_retry_failed_wave_files_cli_wins() {
        let mut config = crate::config::Config::parse_from(vec![
            "reparo", "--path", ".", "--sonar-project-id", "test",
            "--skip-retry-failed-wave-files",
        ]);
        assert!(config.skip_retry_failed_wave_files);

        let yaml_str = r#"
execution:
  retry_failed_wave_files: true
"#;
        let yaml: YamlConfig = serde_yaml::from_str(yaml_str).unwrap();
        merge_yaml_into_config(&mut config, &yaml);
        // CLI already disabled, YAML should NOT re-enable
        assert!(config.skip_retry_failed_wave_files);
    }
}
