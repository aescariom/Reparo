use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::yaml_config::ProjectCommands;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "reparo",
    version,
    about = "Automated SonarQube technical debt fixer using Claude"
)]
pub struct Config {
    /// Path to the project to analyze
    #[arg(long)]
    pub path: PathBuf,

    /// SonarQube project ID (can also be set in YAML: sonar.project_id)
    #[arg(long, env = "SONAR_PROJECT_ID", default_value = "")]
    pub sonar_project_id: String,

    /// SonarQube server URL
    #[arg(long, env = "SONAR_URL", default_value = "http://localhost:9000")]
    pub sonar_url: String,

    /// SonarQube authentication token
    #[arg(long, env = "SONAR_TOKEN", default_value = "")]
    pub sonar_token: String,

    /// Git branch to work from (defaults to current branch).
    /// The fix branch will be created from this branch.
    #[arg(long, env = "REPARO_BRANCH")]
    pub branch: Option<String>,

    /// Number of fixes to batch into a single PR (0 = all in one PR)
    #[arg(long, env = "REPARO_BATCH_SIZE", default_value = "1")]
    pub batch_size: usize,

    /// Command to run tests (auto-detected if not set)
    #[arg(long, env = "REPARO_TEST_COMMAND")]
    pub test_command: Option<String>,

    /// Command to run tests with coverage report (auto-detected if not set)
    #[arg(long, env = "REPARO_COVERAGE_COMMAND")]
    pub coverage_command: Option<String>,

    /// Analyze and report without applying fixes
    #[arg(long, env = "REPARO_DRY_RUN", default_value = "false")]
    pub dry_run: bool,

    /// Maximum number of issues to process (0 = all)
    #[arg(long, env = "REPARO_MAX_ISSUES", default_value = "0")]
    pub max_issues: usize,

    /// Reverse severity order: process least severe issues first (INFO → BLOCKER)
    #[arg(long, default_value = "false")]
    pub reverse_severity: bool,

    /// Log format: text or json
    #[arg(long, env = "REPARO_LOG_FORMAT", default_value = "text")]
    pub log_format: String,

    /// Test execution timeout in seconds
    #[arg(long, env = "REPARO_TEST_TIMEOUT", default_value = "600")]
    pub test_timeout: u64,

    /// Skip running sonar-scanner (use existing analysis)
    #[arg(long, env = "REPARO_SKIP_SCAN", default_value = "false")]
    pub skip_scan: bool,

    /// Path to the scanner binary (auto-detected if not set).
    /// Supports sonar-scanner, mvn, and gradle.
    #[arg(long, env = "REPARO_SCANNER_PATH")]
    pub scanner_path: Option<String>,

    /// Global timeout in seconds for the entire run (0 = no timeout).
    /// Useful in CI/CD to prevent pipelines from hanging.
    #[arg(long, env = "REPARO_TIMEOUT", default_value = "0")]
    pub timeout: u64,

    /// Per-call timeout for Claude in seconds
    #[arg(long, env = "REPARO_CLAUDE_TIMEOUT", default_value = "300")]
    pub claude_timeout: u64,

    /// Resume a previously interrupted execution
    #[arg(long)]
    pub resume: bool,

    /// Path to a YAML config file (default: reparo.yaml in project root)
    #[arg(long)]
    pub config: Option<String>,

    /// Skip creating a pull request after fixes are applied
    #[arg(long, env = "REPARO_NO_PR", default_value = "false")]
    pub no_pr: bool,

    /// Skip permission prompts in Claude CLI (passes --dangerously-skip-permissions)
    #[arg(long, default_value = "false")]
    pub dangerously_skip_permissions: bool,

    /// Print the prompts sent to Claude (for debugging)
    #[arg(long, default_value = "false")]
    pub show_prompts: bool,

    /// Minimum project-wide test coverage (%) required before fixing issues (0 = disabled)
    #[arg(long, env = "REPARO_MIN_COVERAGE", default_value = "80")]
    pub min_coverage: f64,

    /// Minimum per-file test coverage (%) — files below this are boosted even if overall coverage is met (0 = disabled)
    #[arg(long, env = "REPARO_MIN_FILE_COVERAGE", default_value = "0")]
    pub min_file_coverage: f64,

    /// Skip the coverage boost step entirely
    #[arg(long, default_value = "false")]
    pub skip_coverage: bool,

    /// Skip the initial format-and-commit step
    #[arg(long, default_value = "false")]
    pub skip_format: bool,

    /// Number of test generation attempts for coverage (per issue)
    #[arg(long, env = "REPARO_COVERAGE_ATTEMPTS", default_value = "3")]
    pub coverage_attempts: u32,

    /// Maximum coverage rounds per file during boost (0 = unlimited while improving)
    #[arg(long, env = "REPARO_COVERAGE_ROUNDS", default_value = "3")]
    pub coverage_rounds: u32,

    /// Maximum file size (total lines) for coverage boost (0 = no limit) [default: 500]
    #[arg(long, env = "REPARO_MAX_BOOST_FILE_LINES", default_value = "500")]
    pub max_boost_file_lines: usize,

    /// Skip the final validation step (run full test suite after all fixes)
    #[arg(long, default_value = "false")]
    pub skip_final_validation: bool,

    /// Maximum repair attempts during final validation (all tests must pass)
    #[arg(long, env = "REPARO_FINAL_VALIDATION_ATTEMPTS", default_value = "5")]
    pub final_validation_attempts: u32,

    /// Skip the deduplication step after fixing issues
    #[arg(long, default_value = "false")]
    pub skip_dedup: bool,

    /// Maximum number of deduplication iterations (0 = unlimited)
    #[arg(long, env = "REPARO_MAX_DEDUP", default_value = "10")]
    pub max_dedup: usize,

    /// Skip the documentation quality step
    #[arg(long, default_value = "false")]
    pub skip_docs: bool,

    /// Skip the pact/contract testing step
    #[arg(long, default_value = "false")]
    pub skip_pact: bool,

    /// Reset personal config (~/.config/reparo/config.yaml) to defaults and exit
    #[arg(long, default_value = "false")]
    pub restore_personal_yaml: bool,

    /// Glob patterns to exclude from coverage boost (populated from YAML)
    #[arg(skip)]
    pub coverage_exclude: Vec<String>,

    /// Protected files (populated from YAML, not a CLI flag)
    #[arg(skip)]
    pub protected_files: Vec<String>,

    /// Commit message format (populated from YAML)
    #[arg(skip)]
    pub commit_format: String,

    /// Extra commit format variables (populated from YAML)
    #[arg(skip)]
    pub commit_vars: std::collections::HashMap<String, String>,

    /// Documentation configuration (populated from YAML)
    #[arg(skip)]
    pub documentation: DocumentationConfig,

    /// Pact/contract testing configuration (populated from YAML)
    #[arg(skip)]
    pub pact: PactConfig,
}

/// Validated, ready-to-use configuration.
/// Created by calling `Config::validate()`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ValidatedConfig {
    /// Canonicalized absolute path to the project
    pub path: PathBuf,
    pub sonar_project_id: String,
    pub sonar_url: String,
    pub sonar_token: String,
    /// Resolved branch name (from --branch or detected from git)
    pub branch: String,
    pub batch_size: usize,
    pub test_command: Option<String>,
    pub coverage_command: Option<String>,
    pub dry_run: bool,
    pub max_issues: usize,
    /// Process issues in reverse severity order (least severe first)
    pub reverse_severity: bool,
    pub log_format: String,
    pub test_timeout: u64,
    pub skip_scan: bool,
    /// Global timeout in seconds (0 = no timeout)
    pub timeout: u64,
    /// Per-call timeout for Claude in seconds (US-015)
    pub claude_timeout: u64,
    /// Resume from previous interrupted execution (US-017)
    pub resume: bool,
    /// Resolved scanner: kind + binary path
    pub scanner: Option<ScannerKind>,
    /// Project commands from YAML config (US-014)
    pub commands: ProjectCommands,
    /// Whether to create a PR after all fixes
    pub pr: bool,
    /// Whether to skip permission prompts in Claude CLI
    pub dangerously_skip_permissions: bool,
    /// Whether to print prompts sent to Claude
    pub show_prompts: bool,
    /// Minimum project-wide test coverage (%) required before fixing (0 = disabled)
    pub min_coverage: f64,
    /// Minimum per-file test coverage (%) — files below this are boosted individually (0 = disabled)
    pub min_file_coverage: f64,
    /// Skip the coverage boost step
    pub skip_coverage: bool,
    /// Skip the initial format-and-commit step
    pub skip_format: bool,
    /// Number of test generation attempts for coverage (per issue)
    pub coverage_attempts: u32,
    /// Maximum coverage rounds per file during boost (0 = unlimited while improving)
    pub coverage_rounds: u32,
    /// Maximum file size (total lines) for coverage boost (0 = no limit, default: 500)
    pub max_boost_file_lines: usize,
    /// Glob patterns to exclude from coverage boost (e.g., ["*.html", "**/generated/**"])
    pub coverage_exclude: Vec<String>,
    /// Skip the final validation step (full test suite after all fixes)
    pub skip_final_validation: bool,
    /// Maximum repair attempts during final validation (all tests must pass)
    pub final_validation_attempts: u32,
    /// Skip the deduplication step
    pub skip_dedup: bool,
    /// Maximum dedup iterations (0 = unlimited)
    pub max_dedup: usize,
    /// Files that Claude must never modify (reverted automatically after each fix).
    /// Matched case-insensitively against the basename of changed files.
    pub protected_files: Vec<String>,
    /// Commit message format template. Placeholders: {type}, {scope}, {message}, {issue_key}, {rule}, {file}
    /// Plus any custom vars from git.commit_vars.
    pub commit_format: String,
    /// Extra variables for commit format placeholders.
    pub commit_vars: std::collections::HashMap<String, String>,
    /// Skip the documentation quality step
    pub skip_docs: bool,
    /// Documentation quality configuration
    pub documentation: DocumentationConfig,
    /// Skip the pact/contract testing step
    pub skip_pact: bool,
    /// Pact/contract testing configuration
    pub pact: PactConfig,
    /// Resolved engine routing configuration for AI dispatch
    pub engine_routing: crate::engine::EngineRoutingConfig,
}

/// Resolved documentation configuration for runtime use.
#[derive(Debug, Clone, Default)]
pub struct DocumentationConfig {
    pub enabled: bool,
    pub style: String,
    pub standards: Vec<String>,
    pub scope: Vec<String>,
    pub rules: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_files: usize,
    pub required_elements: Vec<String>,
    pub docs_command: Option<String>,
}

/// Resolved pact/contract testing configuration for runtime use.
/// All sub-steps default to disabled.
#[derive(Debug, Clone, Default)]
pub struct PactConfig {
    pub enabled: bool,
    pub pact_dir: Option<String>,
    pub provider_name: Option<String>,
    pub consumer_name: Option<String>,
    pub broker_url: Option<String>,
    pub broker_token: Option<String>,
    pub check_contracts: bool,
    pub generate_tests: bool,
    pub verify_before_fix: bool,
    pub verify_after_fix: bool,
    pub verify_command: Option<String>,
    pub test_command: Option<String>,
    pub attempts: u32,
    pub api_patterns: Vec<String>,
}

impl PactConfig {
    /// Validate pact configuration and return warnings for potential misconfigurations.
    ///
    /// Called at startup to alert the user about missing commands or incomplete setup
    /// before processing any issues.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.enabled {
            return warnings;
        }

        // Verify command required when any verification step is enabled
        if (self.check_contracts || self.verify_before_fix || self.verify_after_fix)
            && self.verify_command.as_ref().map_or(true, |c| c.trim().is_empty())
        {
            warnings.push(
                "Pact verification steps enabled but 'verify_command' is not set — \
                 verification will be skipped"
                    .into(),
            );
        }

        // Test command strongly recommended when generating tests
        if self.generate_tests
            && self.test_command.as_ref().map_or(true, |c| c.trim().is_empty())
        {
            warnings.push(
                "Pact test generation enabled but 'test_command' is not set — \
                 generated tests won't be validated"
                    .into(),
            );
        }

        // Provider/consumer names improve prompt quality
        if self.provider_name.is_none() || self.consumer_name.is_none() {
            warnings.push(
                "Pact provider_name/consumer_name not set — \
                 using generic defaults in prompts"
                    .into(),
            );
        }

        // Broker fields are parsed but not yet used
        if self.broker_url.is_some() || self.broker_token.is_some() {
            warnings.push(
                "Pact broker_url/broker_token are configured but not yet supported — \
                 they will be ignored"
                    .into(),
            );
        }

        warnings
    }
}

/// Which scanner to use and the resolved binary path.
#[derive(Debug, Clone)]
pub enum ScannerKind {
    SonarScanner(PathBuf),
    Maven(PathBuf),
    Gradle(PathBuf),
}

impl Config {
    /// Validate all local parameters (path, git, args) and return a ValidatedConfig.
    /// This does NOT check SonarQube connectivity — that's a separate async step.
    pub fn validate(mut self) -> Result<ValidatedConfig> {
        // -- path --
        if !self.path.exists() {
            bail!(
                "Project path does not exist: {}",
                self.path.display()
            );
        }
        if !self.path.is_dir() {
            bail!(
                "Project path is not a directory: {}",
                self.path.display()
            );
        }

        let path = self
            .path
            .canonicalize()
            .with_context(|| format!("Cannot resolve project path: {}", self.path.display()))?;

        // -- source code presence --
        if !contains_source_files(&path) {
            bail!(
                "Project path does not appear to contain source code: {}",
                path.display()
            );
        }

        // -- git repo --
        if !is_git_repo(&path) {
            bail!(
                "Project path is not inside a git repository: {}. \
                 Reparo needs git to create branches and PRs.",
                path.display()
            );
        }

        // -- load personal config (~/.config/reparo/config.yaml) --
        let personal_config = crate::yaml_config::load_personal_config()?;
        crate::yaml_config::merge_personal_into_config(&mut self, &personal_config);

        // -- load project YAML config (US-014) --
        let yaml_config = crate::yaml_config::load_yaml_config(&path, self.config.as_deref())?;
        if let Some(ref yaml) = yaml_config {
            crate::yaml_config::merge_yaml_into_config(&mut self, yaml);
        }

        // -- sonar project id (checked after YAML merge) --
        if self.sonar_project_id.is_empty() {
            bail!("--sonar-project-id is required (or set SONAR_PROJECT_ID, or define sonar.project_id in reparo.yaml)");
        }

        // -- sonar url --
        if !self.sonar_url.starts_with("http://") && !self.sonar_url.starts_with("https://") {
            bail!(
                "Invalid --sonar-url '{}': must start with http:// or https://",
                self.sonar_url
            );
        }

        // -- branch --
        // Priority: CLI --branch > current checked-out branch > YAML git.branch > fail
        // The current branch wins over YAML so that re-running on a fix branch continues there.
        let branch = match self.branch {
            Some(ref b) if !b.is_empty() => b.clone(),
            _ => detect_current_branch(&path)?,
        };

        // -- log format --
        if self.log_format != "text" && self.log_format != "json" {
            bail!(
                "Invalid --log-format '{}': must be 'text' or 'json'",
                self.log_format
            );
        }

        // -- scanner resolution (US-002) --
        let scanner = if self.skip_scan {
            None
        } else {
            Some(resolve_scanner(&path, self.scanner_path.as_deref())?)
        };

        // -- resolve project commands (US-014) --
        let commands = crate::yaml_config::resolve_commands(
            yaml_config.as_ref(),
            &self.test_command,
            &self.coverage_command,
        );
        let cmd_warnings = crate::yaml_config::validate_commands(&commands, &path);
        for w in &cmd_warnings {
            tracing::warn!("{}", w);
        }

        let validated = ValidatedConfig {
            path,
            sonar_project_id: self.sonar_project_id,
            sonar_url: self.sonar_url.trim_end_matches('/').to_string(),
            sonar_token: self.sonar_token,
            branch,
            batch_size: self.batch_size,
            test_command: self.test_command,
            coverage_command: self.coverage_command,
            dry_run: self.dry_run,
            max_issues: self.max_issues,
            reverse_severity: self.reverse_severity,
            log_format: self.log_format,
            test_timeout: self.test_timeout,
            skip_scan: self.skip_scan,
            timeout: self.timeout,
            claude_timeout: self.claude_timeout,
            resume: self.resume,
            scanner,
            commands,
            pr: !self.no_pr,
            dangerously_skip_permissions: self.dangerously_skip_permissions,
            show_prompts: self.show_prompts,
            min_coverage: if self.skip_coverage { 0.0 } else { self.min_coverage },
            min_file_coverage: if self.skip_coverage { 0.0 } else { self.min_file_coverage },
            skip_coverage: self.skip_coverage,
            skip_format: self.skip_format,
            coverage_attempts: self.coverage_attempts,
            coverage_rounds: self.coverage_rounds,
            max_boost_file_lines: self.max_boost_file_lines,
            coverage_exclude: self.coverage_exclude.clone(),
            skip_final_validation: self.skip_final_validation,
            final_validation_attempts: self.final_validation_attempts,
            skip_dedup: self.skip_dedup,
            max_dedup: self.max_dedup,
            protected_files: self.protected_files,
            commit_format: if self.commit_format.is_empty() { "{type}({scope}): {message}".to_string() } else { self.commit_format },
            commit_vars: self.commit_vars,
            skip_docs: self.skip_docs,
            documentation: DocumentationConfig::default(),
            skip_pact: self.skip_pact,
            pact: self.pact,
            engine_routing: crate::engine::EngineRoutingConfig {
                engines: personal_config.engines.clone(),
                routing: personal_config.routing.clone(),
            },
        };

        // Validate that all routed engines are available
        crate::engine::validate_engines(&validated.engine_routing)?;

        validated.print_summary();
        Ok(validated)
    }
}

impl ValidatedConfig {
    fn print_summary(&self) {
        info!("Reparo configuration:");
        info!("  Project path:    {}", self.path.display());
        info!("  SonarQube URL:   {}", self.sonar_url);
        info!("  SonarQube ID:    {}", self.sonar_project_id);
        info!("  Base branch:     {}", self.branch);
        info!(
            "  Batch size:      {}",
            if self.batch_size == 0 {
                "all".to_string()
            } else {
                self.batch_size.to_string()
            }
        );
        if let Some(cmd) = &self.test_command {
            info!("  Test command:    {}", cmd);
        }
        if self.dry_run {
            info!("  Mode:            DRY RUN");
        }
        match &self.scanner {
            Some(s) => info!("  Scanner:         {}", s.display_name()),
            None => info!("  Scanner:         SKIPPED"),
        }
        if self.max_issues > 0 {
            info!("  Max issues:      {}", self.max_issues);
        }
        info!("  Claude timeout:  {}s", self.claude_timeout);
        info!("  Test timeout:    {}s", self.test_timeout);
        if self.timeout > 0 {
            info!("  Global timeout:  {}s", self.timeout);
        }
        if self.min_coverage > 0.0 {
            info!("  Min coverage:    {:.0}%", self.min_coverage);
        }
        if self.min_file_coverage > 0.0 {
            info!("  Min file cov:    {:.0}%", self.min_file_coverage);
        }
        self.commands.print_summary();
    }
}

impl ScannerKind {
    pub fn display_name(&self) -> String {
        match self {
            ScannerKind::SonarScanner(p) => format!("sonar-scanner ({})", p.display()),
            ScannerKind::Maven(p) => format!("maven sonar:sonar ({})", p.display()),
            ScannerKind::Gradle(p) => format!("gradle sonarqube ({})", p.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve which scanner to use.
///
/// Priority:
/// 1. Explicit `--scanner-path` (user must also tell us the kind via the binary name)
/// 2. Project build system (pom.xml → Maven, build.gradle → Gradle)
/// 3. Generic `sonar-scanner` in PATH
fn resolve_scanner(project_path: &Path, explicit: Option<&str>) -> Result<ScannerKind> {
    // 1. Explicit path
    if let Some(p) = explicit {
        let pb = PathBuf::from(p);
        if !pb.exists() {
            bail!("Scanner path does not exist: {}", p);
        }
        // Guess kind from binary name
        let name = pb
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.contains("mvn") || name.contains("maven") {
            return Ok(ScannerKind::Maven(pb));
        }
        if name.contains("gradle") {
            return Ok(ScannerKind::Gradle(pb));
        }
        return Ok(ScannerKind::SonarScanner(pb));
    }

    // 2. Detect from project build system
    if project_path.join("pom.xml").exists() {
        if let Ok(mvn) = which::which("mvn") {
            info!("Detected Maven project, will use 'mvn sonar:sonar'");
            return Ok(ScannerKind::Maven(mvn));
        }
    }

    if project_path.join("build.gradle").exists()
        || project_path.join("build.gradle.kts").exists()
    {
        // Try gradlew first (project-local wrapper), then system gradle
        let gradlew = project_path.join("gradlew");
        if gradlew.exists() {
            info!("Detected Gradle project, will use './gradlew sonarqube'");
            return Ok(ScannerKind::Gradle(gradlew));
        }
        if let Ok(gradle) = which::which("gradle") {
            info!("Detected Gradle project, will use 'gradle sonarqube'");
            return Ok(ScannerKind::Gradle(gradle));
        }
    }

    // 3. Generic sonar-scanner
    if let Ok(scanner) = which::which("sonar-scanner") {
        return Ok(ScannerKind::SonarScanner(scanner));
    }

    bail!(
        "No scanner found. Install sonar-scanner and add it to PATH, \
         or use --scanner-path to specify the scanner binary. \
         For Maven/Gradle projects, ensure mvn/gradle is in PATH."
    );
}

/// Check if the directory is inside a git repository.
fn is_git_repo(path: &Path) -> bool {
    std::process::Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Detect the current git branch.
fn detect_current_branch(path: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("Failed to detect current git branch")?;

    if !output.status.success() {
        bail!(
            "Cannot detect current branch in {}. Is it a git repository with at least one commit?",
            path.display()
        );
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        bail!(
            "Detached HEAD state in {}. Use --branch to specify a base branch.",
            path.display()
        );
    }

    Ok(branch)
}

/// Heuristic: does the directory contain source files?
/// Checks for common source extensions or build system markers.
fn contains_source_files(path: &Path) -> bool {
    // Build system markers — if any exist, it's a project
    let markers = [
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "package.json",
        "Cargo.toml",
        "go.mod",
        "setup.py",
        "pyproject.toml",
        "Gemfile",
        "Makefile",
        "CMakeLists.txt",
        "meson.build",
        "composer.json",
        "mix.exs",
        "build.sbt",
        "project.clj",
        "*.csproj",
        "*.sln",
        "sonar-project.properties",
    ];

    for marker in &markers {
        if marker.contains('*') {
            // Glob-style check
            if let Ok(mut entries) = glob::glob(&format!("{}/{}", path.display(), marker)) {
                if entries.next().is_some() {
                    return true;
                }
            }
        } else if path.join(marker).exists() {
            return true;
        }
    }

    // Fallback: check for common source extensions in top-level or src/
    let source_dirs = [path.to_path_buf(), path.join("src"), path.join("lib")];
    let extensions = [
        "java", "py", "rs", "go", "js", "ts", "rb", "c", "cpp", "cs", "kt", "scala", "php",
        "swift", "m", "h", "ex", "exs", "clj",
    ];

    for dir in &source_dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if extensions.contains(&ext_str.as_str()) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a Config with defaults, overriding only what's needed per test.
    fn base_config(path: PathBuf) -> Config {
        Config {
            path,
            sonar_project_id: "test".to_string(),
            sonar_url: "http://localhost:9000".to_string(),
            sonar_token: String::new(),
            branch: None,
            batch_size: 1,
            test_command: None,
            coverage_command: None,
            dry_run: false,
            max_issues: 0,
            reverse_severity: false,
            log_format: "text".to_string(),
            test_timeout: 600,
            skip_scan: true, // skip scan in tests (no scanner binary)
            scanner_path: None,
            timeout: 0,
            claude_timeout: 300,
            resume: false,
            config: None,
            no_pr: false,
            dangerously_skip_permissions: false,
            show_prompts: false,
            min_coverage: 80.0,
            min_file_coverage: 0.0,
            skip_coverage: false,
            skip_format: false,
            coverage_attempts: 3,
            coverage_rounds: 3,
            max_boost_file_lines: 500,
            coverage_exclude: vec![],
            skip_final_validation: false,
            final_validation_attempts: 5,
            skip_dedup: false,
            max_dedup: 10,
            protected_files: vec![],
            commit_format: "{type}({scope}): {message}".to_string(),
            commit_vars: std::collections::HashMap::new(),
            skip_docs: false,
            documentation: DocumentationConfig::default(),
            skip_pact: false,
            pact: PactConfig::default(),
            restore_personal_yaml: false,
        }
    }

    /// Create a temp dir with a git repo + a source file.
    fn git_project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.py"), "x = 1").unwrap();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["init"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["add", "."])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        tmp
    }

    // -- unit tests for helpers --

    #[test]
    fn test_contains_source_files_with_marker() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert!(contains_source_files(tmp.path()));
    }

    #[test]
    fn test_contains_source_files_with_source_ext() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.py"), "print('hi')").unwrap();
        assert!(contains_source_files(tmp.path()));
    }

    #[test]
    fn test_contains_source_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!contains_source_files(tmp.path()));
    }

    #[test]
    fn test_is_git_repo_false() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(tmp.path()));
    }

    // -- validation tests --

    #[test]
    fn test_validate_nonexistent_path() {
        let config = base_config(PathBuf::from("/nonexistent/path/that/does/not/exist"));
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_empty_sonar_id() {
        let tmp = git_project();
        let mut config = base_config(tmp.path().to_path_buf());
        config.sonar_project_id = String::new();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("sonar-project-id"));
    }

    #[test]
    fn test_validate_bad_sonar_url() {
        let tmp = git_project();
        let mut config = base_config(tmp.path().to_path_buf());
        config.sonar_url = "ftp://bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("http://"));
    }

    #[test]
    fn test_validate_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["init"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["commit", "--allow-empty", "-m", "init"])
            .output()
            .unwrap();

        let config = base_config(tmp.path().to_path_buf());
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("source code"));
    }

    #[test]
    fn test_validate_not_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.py"), "x = 1").unwrap();
        let config = base_config(tmp.path().to_path_buf());
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("git repository"));
    }

    #[test]
    fn test_validate_success_skip_scan() {
        let tmp = git_project();
        let mut config = base_config(tmp.path().to_path_buf());
        config.sonar_project_id = "my-project".to_string();
        config.sonar_url = "https://sonar.example.com".to_string();
        config.sonar_token = "tok_123".to_string();
        config.branch = Some("main".to_string());
        config.batch_size = 5;
        config.dry_run = true;
        config.max_issues = 10;
        config.log_format = "json".to_string();
        config.skip_scan = true;

        let vc = config.validate().unwrap();
        assert_eq!(vc.sonar_project_id, "my-project");
        assert_eq!(vc.sonar_url, "https://sonar.example.com");
        assert_eq!(vc.branch, "main");
        assert_eq!(vc.batch_size, 5);
        assert!(vc.dry_run);
        assert!(vc.path.is_absolute());
        assert!(vc.scanner.is_none()); // skip_scan → no scanner
    }

    #[test]
    fn test_validate_scanner_not_found() {
        let tmp = git_project();
        let mut config = base_config(tmp.path().to_path_buf());
        config.skip_scan = false;
        config.scanner_path = Some("/nonexistent/scanner".to_string());
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_resolve_scanner_maven_project() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        // Only works if mvn is installed — skip gracefully if not
        match resolve_scanner(tmp.path(), None) {
            Ok(ScannerKind::Maven(_)) => {} // expected
            Err(_) => {}                    // mvn not in PATH, that's ok
            Ok(other) => panic!("Expected Maven scanner, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_scanner_explicit_path() {
        // Use /bin/echo as a stand-in for a scanner binary
        let result = resolve_scanner(Path::new("/tmp"), Some("/bin/echo"));
        assert!(result.is_ok());
        if let Ok(ScannerKind::SonarScanner(p)) = result {
            assert_eq!(p, PathBuf::from("/bin/echo"));
        }
    }

    #[test]
    fn test_scanner_display_name() {
        let s = ScannerKind::SonarScanner(PathBuf::from("/usr/bin/sonar-scanner"));
        assert!(s.display_name().contains("sonar-scanner"));

        let m = ScannerKind::Maven(PathBuf::from("/usr/bin/mvn"));
        assert!(m.display_name().contains("maven"));

        let g = ScannerKind::Gradle(PathBuf::from("./gradlew"));
        assert!(g.display_name().contains("gradle"));
    }
}
