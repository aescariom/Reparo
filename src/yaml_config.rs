//! YAML configuration file support (US-014).
//!
//! Loads `reparo.yaml` from the project root and merges with CLI/env config.
//! Priority: CLI > ENV > YAML > defaults.

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;
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
    /// Files that Claude must never modify during fixes (reverted automatically).
    /// List of exact filenames (matched against the basename, case-insensitive).
    #[serde(default)]
    pub protected_files: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SonarYaml {
    pub project_id: Option<String>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub skip_scan: Option<bool>,
    pub scanner_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExecutionYaml {
    pub max_issues: Option<usize>,
    pub dry_run: Option<bool>,
    pub timeout: Option<u64>,
    pub log_format: Option<String>,
    pub test_timeout: Option<u64>,
    pub claude_timeout: Option<u64>,
    pub min_coverage: Option<f64>,
    pub min_file_coverage: Option<f64>,
    /// Run formatter and commit before starting fixes (default: true)
    pub format_on_start: Option<bool>,
    /// Number of test generation attempts for coverage per issue (default: 3)
    pub coverage_attempts: Option<u32>,
    /// Run deduplication step after fixes (default: true)
    pub dedup_on_completion: Option<bool>,
    /// Maximum deduplication iterations (default: 10)
    pub max_dedup: Option<usize>,
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
    // coverage_attempts: only override if CLI is at default (3)
    if config.coverage_attempts == 3 {
        if let Some(v) = yaml.execution.coverage_attempts {
            config.coverage_attempts = v;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
