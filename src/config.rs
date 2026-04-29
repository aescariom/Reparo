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

    /// US-064: Minimum project-wide branch coverage (%) — tests are generated
    /// until both line AND branch thresholds are reached. 0 = disabled.
    /// Only active when the coverage report contains branch data (BRDA records).
    #[arg(long, env = "REPARO_MIN_BRANCH_COVERAGE", default_value = "0")]
    pub min_branch_coverage: f64,

    /// US-064: Minimum per-file branch coverage (%) — files below this are
    /// boosted even if line threshold is met. 0 = disabled.
    #[arg(long, env = "REPARO_MIN_FILE_BRANCH_COVERAGE", default_value = "0")]
    pub min_file_branch_coverage: f64,

    /// Skip the test overlap detection phase (Step 3a)
    #[arg(long, default_value = "false")]
    pub skip_overlap: bool,

    /// Include issues found in test code (`scopes=TEST` in the SonarQube API).
    ///
    /// By default reparo only processes `MAIN` scope issues — the same set
    /// the SonarQube web UI shows by default. SonarQube stores issues from
    /// test code separately and the web UI hides them unless you explicitly
    /// switch the scope filter to "All scopes" or "Tests". Running with this
    /// flag doubles-to-triples the issue count on typical projects.
    #[arg(long, env = "REPARO_INCLUDE_TEST_ISSUES", default_value = "false")]
    pub include_test_issues: bool,

    /// Additional file-path glob to exclude from processing. Repeatable.
    ///
    /// Applied on top of whatever is declared in the project's
    /// `sonar-project.properties` (`sonar.exclusions`, `sonar.test.exclusions`,
    /// `sonar.coverage.exclusions`, `sonar.cpd.exclusions`). Use this to
    /// exclude entire packages from reparo without touching the properties
    /// file — e.g. `--exclude 'src/main/java/com/legacy/**'`.
    ///
    /// Patterns use Ant-style globs:
    ///   `**` any number of path segments, `*` anything within a segment,
    ///   `?` a single character. Patterns without a leading `**/` are
    ///   auto-anchored to match at any depth.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub sonar_exclusions: Vec<String>,

    /// Skip the coverage boost step entirely
    #[arg(long, default_value = "false")]
    pub skip_coverage: bool,

    /// Skip the initial format-and-commit step
    #[arg(long, default_value = "false")]
    pub skip_format: bool,

    /// Allow running with uncommitted changes in the working tree. The pending
    /// changes are staged at startup and will be folded into the first commit
    /// the program creates (format, coverage, or fix). Use this to run Reparo
    /// on top of in-progress work without having to commit or stash first.
    #[arg(long, env = "REPARO_ALLOW_WIP", default_value = "false")]
    pub allow_wip: bool,

    /// Number of test generation attempts for coverage (per issue)
    #[arg(long, env = "REPARO_COVERAGE_ATTEMPTS", default_value = "3")]
    pub coverage_attempts: u32,

    /// Maximum coverage rounds per file during boost (0 = unlimited while improving)
    #[arg(long, env = "REPARO_COVERAGE_ROUNDS", default_value = "3")]
    pub coverage_rounds: u32,

    /// Files to process per wave before running the test suite once (default: 3).
    /// Larger values = fewer test runs but more wasted work if a wave fails.
    #[arg(long, alias = "coverage_wave_size", env = "REPARO_COVERAGE_WAVE_SIZE", default_value = "3")]
    pub coverage_wave_size: u32,

    /// Files per coverage boost commit (0 = same as coverage_wave_size, 1 = one commit per file)
    #[arg(long, env = "REPARO_COVERAGE_COMMIT_BATCH", default_value = "0")]
    pub coverage_commit_batch: u32,

    /// Issues per git commit during the fix step (1 = one commit per issue, 0 = one commit per branch).
    /// Analogous to --coverage-commit-batch but for issue fixes.
    #[arg(long, env = "REPARO_FIX_COMMIT_BATCH", default_value = "1")]
    pub fix_commit_batch: u32,

    /// Number of files to process in parallel during coverage boost (1 = sequential).
    /// Each parallel file gets its own git worktree for isolation.
    #[arg(long, env = "REPARO_COVERAGE_PARALLEL", default_value = "1")]
    pub coverage_parallel: u32,

    /// Number of issues to process in parallel using git worktrees (1 = sequential).
    /// Each parallel issue gets its own worktree, branch, and PR.
    /// Incompatible with batch_size > 1.
    #[arg(long, env = "REPARO_PARALLEL", default_value = "1")]
    pub parallel: u32,

    /// Stop coverage boost after N consecutive wave failures (0 = disabled)
    #[arg(long, env = "REPARO_MAX_BOOST_FAILURES", default_value = "5")]
    pub max_boost_failures: usize,

    /// Maximum lines to embed in a method chunk snippet for coverage boost (US-053).
    /// Methods with more lines use a compact representation (signature + uncovered context + close)
    /// and let Claude read the full file via tool calls.  0 = always embed full method.
    #[arg(long, env = "REPARO_CHUNK_SNIPPET_MAX_LINES", default_value = "80")]
    pub chunk_snippet_max_lines: usize,

    /// Directory (relative to project path) where the execution summary markdown
    /// is written at the end of each run. Defaults to `.reparo`.
    #[arg(long, env = "REPARO_EXECUTION_LOG_REPORT_DIR", default_value = ".reparo")]
    pub execution_log_report_dir: String,

    /// US-066: Enable compliance mode (baseline). When true, generated tests must
    /// include `@Reparo.*` trace blocks (purpose, requirement, testType, runId),
    /// Reparo validates their presence, and the execution log + compliance report
    /// include trazability metadata. Baseline compliance targets ISO 25010 (software
    /// quality), ISO/IEC 33020 (process assessment) and ENS Alto (Spanish national
    /// security framework) — industry-agnostic.
    ///
    /// Health-specific standards (MDR 2017/745, IEC 62304, IEC 62305, ISO 14971,
    /// Class A/B/C, MC/DC) require the additional `--health-mode` flag.
    #[arg(long, env = "REPARO_COMPLIANCE", default_value = "false")]
    pub compliance_enabled: bool,

    /// Enable medical/health-software compliance extensions on top of `--compliance`.
    ///
    /// When true, Reparo activates features specific to **medical device software**
    /// and safety-critical health systems:
    /// - **MDR 2017/745** Annex II technical documentation references
    /// - **IEC 62304** software safety classification (Class A/B/C) and unit
    ///   testing rigor per class
    /// - **IEC 62305** / **ISO 14971** risk control traceability
    /// - **MC/DC** (Modified Condition/Decision Coverage) tracking for Class C code
    /// - `@Reparo.riskClass` field in test trace blocks
    /// - Compliance report sections for IEC 62304 §5.5.4 and MDR Annex II §6.1(b)
    ///
    /// Implies `--compliance` (baseline). Projects that are NOT medical/health
    /// should leave this flag off — the industry-agnostic compliance features
    /// (trace blocks, traceability matrix, audit log, ISO 25010 quality testing)
    /// remain fully available via `--compliance` alone.
    #[arg(long, env = "REPARO_HEALTH_MODE", default_value = "false")]
    pub health_mode: bool,

    /// US-059: Force-skip the separate `test` invocation when the `coverage`
    /// command already runs tests internally (Maven, Gradle, pytest --cov, etc.).
    /// When absent, Reparo auto-detects based on the command strings.
    /// Use `--tests-in-coverage` to force on; `--no-tests-in-coverage` to force off.
    #[arg(long, env = "REPARO_TESTS_IN_COVERAGE")]
    pub tests_in_coverage: Option<bool>,

    /// Disable retry of failed wave files with error context in per-file fallback
    #[arg(long, default_value = "false")]
    pub skip_retry_failed_wave_files: bool,

    /// Skip the final validation step (run full test suite after all fixes)
    #[arg(long, default_value = "false")]
    pub skip_final_validation: bool,

    /// Disable running targeted tests (Surefire `-Dtest=`) before the full
    /// suite during per-fix validation. Maven-only; no-op for other runners.
    #[arg(long, default_value = "false")]
    pub skip_targeted_tests: bool,

    /// Run SonarQube rescan verification every N fixes instead of after each.
    /// Saves ~20s × (N-1) per batch in sequential mode. Default 1 (always rescan).
    ///
    /// Set to `0` to defer ALL per-issue rescans and run a single verification
    /// scan at the very end of the whole run (saves ~N rescans × ~30-60s each —
    /// the biggest single cut available for huge queues). The trade-off is
    /// that per-issue "still-open" AI retries won't fire during the run; issues
    /// Sonar still reports at the end are flagged for manual review.
    #[arg(long, env = "REPARO_RESCAN_BATCH_SIZE", default_value = "1")]
    pub rescan_batch_size: u32,

    /// Run the post-wave SonarQube verification scan every N waves instead of
    /// after every wave. Each scan costs ~20-30 s of scanner time + CE wait,
    /// so for a run with 88 waves the per-wave policy alone burns ~30-45 min
    /// just in rescans. Batching lets several waves accumulate before we pay
    /// that cost — the trade-off is that "still reported by Sonar" issues
    /// from wave K are only requeued for sequential retry when wave K+N runs
    /// its scan. Default 1 (every wave). Set to 3-5 for large projects.
    #[arg(long, env = "REPARO_WAVE_SCAN_BATCH", default_value = "1")]
    pub wave_scan_batch: u32,

    /// Use a lean Claude invocation for per-fix tasks: pass `--bare`
    /// (skips CLAUDE.md auto-load, auto-memory, hooks, plugins) and
    /// `--tools Read,Edit,Write` (no Grep/Glob/Bash). Cuts input tokens
    /// dramatically for focused single-file fixes. Default: true.
    #[arg(long, default_value = "false")]
    pub no_lean_ai: bool,

    /// Maximum repair attempts during final validation (all tests must pass)
    #[arg(long, env = "REPARO_FINAL_VALIDATION_ATTEMPTS", default_value = "5")]
    pub final_validation_attempts: u32,

    /// Skip the deduplication step after fixing issues
    #[arg(long, default_value = "false")]
    pub skip_dedup: bool,

    /// Skip the fix loop entirely (coverage boost and preflight still run)
    #[arg(long, default_value = "false")]
    pub skip_fixes: bool,

    /// Maximum number of deduplication iterations (0 = unlimited)
    #[arg(long, env = "REPARO_MAX_DEDUP", default_value = "10")]
    pub max_dedup: usize,

    /// Skip the documentation quality step
    #[arg(long, default_value = "false")]
    pub skip_docs: bool,

    /// Skip the pact/contract testing step
    #[arg(long, default_value = "false")]
    pub skip_pact: bool,

    /// Skip the pre-push rebase onto the latest base branch
    #[arg(long, default_value = "false")]
    pub skip_rebase: bool,

    /// Skip the local linter discovery phase (Step 3d). When enabled,
    /// Reparo runs the configured `commands.lint`, parses its output
    /// according to `commands.lint_format`, and folds the findings into
    /// the fix queue alongside SonarQube issues.
    #[arg(long, default_value = "false")]
    pub skip_linter_scan: bool,

    /// Run the linter's native autofix (e.g. `clippy --fix`, `eslint --fix`)
    /// before scanning for findings. Reduces the number of issues Reparo has
    /// to dispatch to an AI engine. Default: false.
    #[arg(long, env = "REPARO_LINTER_AUTOFIX", default_value = "false")]
    pub linter_autofix: bool,

    /// Maximum number of linter findings to queue (0 = no cap). Prevents a
    /// flood of low-severity smells from drowning out SonarQube issues.
    #[arg(long, env = "REPARO_MAX_LINTER_FINDINGS", default_value = "200")]
    pub max_linter_findings: u32,

    /// Disable the per-issue linter autofix fast-path. When enabled (default),
    /// `lint:*` findings first try the linter's own `--fix` on the affected
    /// file for the specific rule; only if that doesn't resolve the finding
    /// does Reparo invoke the AI engine. Saves ~30-60s per auto-fixable
    /// finding. Set this flag to opt out.
    #[arg(long, env = "REPARO_SKIP_LINTER_FASTPATH", default_value = "false")]
    pub skip_linter_fastpath: bool,

    /// Disable same-file / same-rule issue grouping. When enabled (default),
    /// Reparo buckets findings by `(file, rule)` and issues one AI call per
    /// bucket instead of one per finding. For queues with many repeated
    /// findings (e.g. 40 unused-import warnings in one file), this is a
    /// near-linear speedup.
    #[arg(long, env = "REPARO_SKIP_ISSUE_GROUPING", default_value = "false")]
    pub skip_issue_grouping: bool,

    /// Disable the Sonar autofix fast-path (OpenRewrite). When enabled
    /// (default), Reparo runs a single `mvn rewrite:run` before the AI
    /// fix loop with recipes mapped to common Sonar rules (S1128, S1481,
    /// S1068, S1118, S1161, S1124, S2293, …). Findings resolved this way
    /// skip the AI path entirely. Saves ~3 min per resolvable finding.
    #[arg(long, env = "REPARO_SKIP_AUTOFIX_SONAR", default_value = "false")]
    pub skip_autofix_sonar: bool,

    /// Maximum number of findings to bundle into a single AI call during
    /// issue grouping. Larger values = fewer AI calls but crowded prompts;
    /// 20 is a good default for one-line style fixes.
    #[arg(long, env = "REPARO_MAX_GROUP_SIZE", default_value = "20")]
    pub max_group_size: usize,

    /// Apply same-file/same-rule grouping to Sonar findings within a single
    /// wave (US-082). Hoy `skip_issue_grouping` solo afecta a `lint:*`
    /// findings porque la rescan mid-loop podría resucitar findings Sonar
    /// agrupados a nivel global. Dentro de un wave no hay rescan, así que
    /// es seguro. Default true; ponerlo a false para reproducir el
    /// comportamiento previo.
    #[arg(long, env = "REPARO_DISABLE_WAVE_BATCHING", default_value = "false")]
    pub disable_wave_batching: bool,

    /// Reset personal config (~/.config/reparo/config.yaml) to defaults and exit
    #[arg(long, default_value = "false")]
    pub restore_personal_yaml: bool,

    /// External issue/ticket reference to embed in commit messages via {ticket} placeholder
    /// (e.g., JIRA-123, #42, LINEAR-456). CLI-only — not configurable via YAML.
    #[arg(long)]
    pub commit_issue: Option<String>,

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

    /// Test generation configuration (populated from YAML, US-040)
    #[arg(skip)]
    pub test_generation: TestGenerationConfig,

    /// Pre-fix risk assessment configuration (populated from YAML)
    #[arg(skip)]
    pub risk_assessment: RiskAssessmentConfig,

    /// Skip `mvn clean` between successful sequential fixes. Default: true.
    /// Populated from YAML (no CLI flag).
    #[arg(skip = true)]
    pub skip_clean_when_safe: bool,

    /// Wave-sharding conflict depth: 0 = same file, 1 = same parent dir.
    /// Populated from YAML.
    #[arg(skip = 1usize)]
    pub wave_grouping_depth: usize,

    /// Rules to skip as NeedsReview. Populated from YAML.
    #[arg(skip)]
    pub rule_blocklist: Vec<String>,
    /// (rule, file-suffix) pairs to skip as NeedsReview. Populated from YAML.
    #[arg(skip)]
    pub hard_case_blocklist: Vec<(String, String)>,
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
    /// US-064: Minimum project-wide branch coverage (%). 0 = disabled.
    pub min_branch_coverage: f64,
    /// US-064: Minimum per-file branch coverage (%). 0 = disabled.
    pub min_file_branch_coverage: f64,
    /// Skip the test overlap detection phase
    pub skip_overlap: bool,
    /// Also process issues in test code (scopes=TEST in SonarQube). Default
    /// is MAIN-only, matching the SonarQube web UI's default issue view.
    pub include_test_issues: bool,
    /// File-path globs to exclude from processing. Merged at SonarClient
    /// construction time with patterns parsed from `sonar-project.properties`.
    pub sonar_exclusions: Vec<String>,
    /// Skip the coverage boost step
    pub skip_coverage: bool,
    /// Skip the initial format-and-commit step
    pub skip_format: bool,
    /// Allow starting with a dirty working tree — pending changes are staged at
    /// startup and absorbed into the first commit Reparo produces.
    pub allow_wip: bool,
    /// Number of test generation attempts for coverage (per issue)
    pub coverage_attempts: u32,
    /// Maximum coverage rounds per file during boost (0 = unlimited while improving)
    pub coverage_rounds: u32,
    /// Glob patterns to exclude from coverage boost (e.g., ["*.html", "**/generated/**"])
    pub coverage_exclude: Vec<String>,
    /// Files per wave before running the test suite once (default: 3)
    pub coverage_wave_size: u32,
    /// Files per coverage boost commit (resolved: never 0 at runtime)
    pub coverage_commit_batch: u32,
    /// Issues per git commit during the fix step (1 = one per issue, 0 = one per branch)
    pub fix_commit_batch: u32,
    /// Number of files to process in parallel during coverage boost (1 = sequential)
    pub coverage_parallel: u32,
    /// Number of issues to fix in parallel using git worktrees (1 = sequential)
    pub parallel: u32,
    /// Stop coverage boost after N consecutive wave failures (0 = disabled)
    pub max_boost_failures: usize,
    /// Max lines to embed in a method chunk snippet (0 = always embed full method)
    pub chunk_snippet_max_lines: usize,
    /// Directory (relative to project path) where execution summary markdown is written
    pub execution_log_report_dir: String,
    /// US-066: compliance mode (baseline — ISO 25010 / ENS Alto / generic traceability)
    pub compliance_enabled: bool,
    /// Health/medical mode extension on top of `compliance_enabled`: adds MDR /
    /// IEC 62304 / IEC 62305 / ISO 14971 features (riskClass, MC/DC, medical
    /// sections in compliance report). Implies `compliance_enabled`.
    pub health_mode: bool,
    /// Retry failed wave files with error context in per-file fallback
    pub retry_failed_wave_files: bool,
    /// Skip the final validation step (full test suite after all fixes)
    pub skip_final_validation: bool,
    /// Maximum repair attempts during final validation (all tests must pass)
    pub final_validation_attempts: u32,
    /// Run targeted tests (Surefire -Dtest=) before the full suite on per-fix
    /// validation. Full suite still runs for confirmation if targeted passes.
    /// Maven-only for now. Default: true.
    pub targeted_tests_first: bool,
    /// How often to run the SonarQube rescan verification step in fix_loop.
    /// 1 = rescan after every fix (most thorough). N = rescan only every Nth
    /// fix (saves ~20s × (N-1) per batch of N issues in sequential mode).
    /// Parallel mode is unaffected — each worker owns its own branch.
    pub rescan_batch_size: u32,
    /// How often to run the post-wave SonarQube verification scan. 1 = after
    /// every wave (original behavior). N = after every Nth wave or the final
    /// wave — whichever comes first. Saves ~20-30 s × (N-1) per batch of N
    /// waves when the scanner + CE wait dominates wave wall-clock.
    pub wave_scan_batch: u32,
    /// Use a lean Claude invocation for per-fix tasks (--bare + tool whitelist).
    /// Default: true (opt out via --no-lean-ai).
    pub lean_ai: bool,
    /// Skip the deduplication step
    pub skip_dedup: bool,
    /// Maximum dedup iterations (0 = unlimited)
    pub max_dedup: usize,
    /// Skip the fix loop entirely
    pub skip_fixes: bool,
    /// Files that Claude must never modify (reverted automatically after each fix).
    /// Matched case-insensitively against the basename of changed files.
    pub protected_files: Vec<String>,
    /// Commit message format template. Placeholders: {type}, {scope}, {message}, {issue_key}, {rule}, {file}, {ticket}
    /// Plus any custom vars from git.commit_vars.
    pub commit_format: String,
    /// Extra variables for commit format placeholders.
    pub commit_vars: std::collections::HashMap<String, String>,
    /// External issue/ticket reference (CLI-only). Available as {ticket} in commit format.
    pub commit_issue: Option<String>,
    /// Skip the documentation quality step
    pub skip_docs: bool,
    /// Documentation quality configuration
    pub documentation: DocumentationConfig,
    /// Skip the pact/contract testing step
    pub skip_pact: bool,
    /// Pact/contract testing configuration
    pub pact: PactConfig,
    /// Skip the pre-push rebase onto the latest base branch
    pub skip_rebase: bool,
    /// Skip the local linter discovery phase (Step 3d).
    pub skip_linter_scan: bool,
    /// Run the linter's native autofix before collecting findings.
    pub linter_autofix: bool,
    /// Maximum number of linter findings to queue (0 = no cap).
    pub max_linter_findings: u32,
    /// Disable the per-issue linter autofix fast-path (A1).
    pub skip_linter_fastpath: bool,
    /// Disable same-file/same-rule issue grouping (A3).
    pub skip_issue_grouping: bool,
    /// Maximum findings per batched AI call (A3).
    pub max_group_size: usize,
    /// Disable wave-level batching for Sonar findings (US-082).
    /// Default false ⇒ wave batching active.
    pub disable_wave_batching: bool,
    /// Disable Sonar autofix fast-path (OpenRewrite).
    pub skip_autofix_sonar: bool,
    /// Frozen baseline lcov snapshot path. Captured once before the fix loop
    /// (after preflight + coverage boost). All per-issue coverage checks
    /// read from this file rather than the per-worktree lcov, so fixes
    /// happening in parallel cannot contaminate each other's coverage view.
    /// `None` when no coverage report could be located.
    pub baseline_lcov_path: Option<PathBuf>,
    /// Resolved engine routing configuration for AI dispatch
    pub engine_routing: crate::engine::EngineRoutingConfig,
    /// Resolved test generation configuration for framework-aware prompts (US-040)
    pub test_generation: TestGenerationConfig,
    /// US-069/US-073: Resolved compliance configuration (IEC 62304, requirements traceability)
    pub compliance: crate::compliance::ComplianceConfig,
    /// Pre-fix risk assessment configuration
    pub risk_assessment: RiskAssessmentConfig,
    /// Skip `clean` between fixes when the previous fix succeeded (default: true).
    pub skip_clean_when_safe: bool,
    /// Depth for wave-sharding affinity: 0 = file-only (previous behavior),
    /// 1 = parent directory (default), 2 = grandparent.
    pub wave_grouping_depth: usize,
    /// Rule IDs to skip as NeedsReview without attempting a fix.
    pub rule_blocklist: Vec<String>,
    /// (rule, file-suffix) pairs to skip as NeedsReview without attempting a fix.
    /// Populated from `execution.hard_case_blocklist` in `reparo.yaml`.
    pub hard_case_blocklist: Vec<(String, String)>,
    /// Set to `true` by parallel workers that run inside a freshly-acquired
    /// git worktree. Fresh worktrees are guaranteed to be at a clean HEAD (the
    /// pool calls `git clean` + reset on release), so the per-fix `mvn clean`
    /// step is pure overhead — incremental compile is correct from a cold
    /// state. Skipping it saves ~1.5-2 s per issue × N issues per worker.
    pub fresh_worktree: bool,
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
///
/// When a `pact:` section is present in `reparo.yaml`, `configured` is set to true
/// and `enabled` defaults to true unless explicitly set to false. When the section
/// is missing entirely, `configured` is false — running the pact phase without
/// `--skip-pact` in that state is a hard error (see `validate()`).
#[derive(Debug, Clone)]
pub struct PactConfig {
    /// True when a `pact:` section was present in `reparo.yaml`. Distinguishes
    /// "not configured at all" from "configured but enabled=false".
    pub configured: bool,
    pub enabled: bool,
    pub pact_dir: Option<String>,
    pub provider_name: Option<String>,
    pub consumer_name: Option<String>,
    pub check_contracts: bool,
    pub generate_tests: bool,
    pub verify_before_fix: bool,
    pub verify_after_fix: bool,
    pub verify_command: Option<String>,
    pub test_command: Option<String>,
    pub attempts: u32,
    pub api_patterns: Vec<String>,
}

impl Default for PactConfig {
    fn default() -> Self {
        Self {
            configured: false,
            // Default to enabled when a section is present (opt-out via --skip-pact).
            // When `configured == false`, this value is irrelevant because validate()
            // bails before it is ever consulted.
            enabled: true,
            pact_dir: None,
            provider_name: None,
            consumer_name: None,
            check_contracts: false,
            generate_tests: false,
            verify_before_fix: false,
            verify_after_fix: false,
            verify_command: None,
            test_command: None,
            attempts: 3,
            api_patterns: Vec::new(),
        }
    }
}

impl PactConfig {
    /// Validate pact configuration. Hard-errors with `bail!` for missing
    /// required commands; logs soft issues via `tracing::warn!`.
    ///
    /// Must only be called when `skip_pact == false` — callers are expected to
    /// short-circuit when the user has opted out.
    pub fn validate(&self) -> Result<()> {
        // Missing section entirely — force the user to make an explicit choice.
        if !self.configured {
            bail!(
                "Pact phase is active but no `pact:` section was found in reparo.yaml. \
                 Add a `pact:` section (see README) or pass `--skip-pact` to skip this step."
            );
        }

        // Section present but the user explicitly disabled it — nothing to validate.
        if !self.enabled {
            tracing::info!("Pact section present but `enabled: false` — skipping pact phase");
            return Ok(());
        }

        // Verify command required when any verification step is enabled.
        if (self.check_contracts || self.verify_before_fix || self.verify_after_fix)
            && self.verify_command.as_ref().map_or(true, |c| c.trim().is_empty())
        {
            bail!(
                "Pact verification steps are enabled (check_contracts / verify_before_fix / \
                 verify_after_fix) but `pact.verify_command` is not set. Configure it in \
                 reparo.yaml or disable the verification sub-steps."
            );
        }

        // Test command required when generating contract tests — otherwise generated
        // output cannot be validated and silently-broken tests would be committed.
        if self.generate_tests
            && self.test_command.as_ref().map_or(true, |c| c.trim().is_empty())
        {
            bail!(
                "Pact `generate_tests` is enabled but `pact.test_command` is not set. \
                 Generated contract tests must be runnable to be validated."
            );
        }

        // Provider/consumer names improve prompt quality but are not mandatory.
        if self.provider_name.is_none() || self.consumer_name.is_none() {
            tracing::warn!(
                "Pact provider_name/consumer_name not set — Claude will use generic defaults \
                 in contract-test prompts"
            );
        }

        Ok(())
    }
}

/// Resolved test generation configuration for framework-aware prompts (US-040).
#[derive(Debug, Clone)]
pub struct TestGenerationConfig {
    pub framework: Option<String>,
    pub mock_framework: Option<String>,
    pub assertion_library: Option<String>,
    pub avoid_spring_context: bool,
    pub custom_instructions: Option<String>,
    /// Resolved model/effort for each test-generation complexity band.
    pub tiers: TestGenTiers,
    /// When set, test files are written into this directory mirroring the source
    /// tree (e.g. "projects/lib/test/unit").  null = colocated (default).
    pub test_dir: Option<String>,
    /// Prefix to strip from source file paths before placing them under
    /// `test_dir`.  Auto-detected from the common prefix when absent.
    pub test_source_root: Option<String>,
    /// When set, consolidate sub-module source files into a single spec file
    /// by keeping only the first N dot-separated name segments (before the
    /// extension).  E.g. `test_spec_segments: 2` maps
    /// `calendar.component.datesRender.ts` → `calendar.component.spec.ts`.
    /// None = one spec file per source file (default).
    pub test_spec_segments: Option<usize>,
}

impl Default for TestGenerationConfig {
    fn default() -> Self {
        Self {
            framework: None,
            mock_framework: None,
            assertion_library: None,
            avoid_spring_context: false,
            custom_instructions: None,
            tiers: TestGenTiers::default(),
            test_dir: None,
            test_source_root: None,
            test_spec_segments: None,
        }
    }
}

/// Pre-fix risk assessment configuration.
///
/// Controls whether Reparo checks if a fix is safe to apply in isolation
/// before attempting it. Issues assessed as high-risk (cross-cutting impact)
/// are skipped and logged to REVIEW_NEEDED.md with a clear explanation.
#[derive(Debug, Clone)]
pub struct RiskAssessmentConfig {
    /// Enable risk assessment (default: false).
    pub enabled: bool,
    /// Use Claude (haiku, low effort) for AI-based risk assessment when static
    /// patterns don't match. Default: false (static patterns only).
    pub ai_assessment: bool,
    /// Skip the fix if risk >= this level.
    /// "high" (default): only skip clearly cross-cutting issues.
    /// "medium": also skip borderline cases.
    pub skip_threshold: String,
}

impl RiskAssessmentConfig {
    /// Returns the `RiskLevel` corresponding to `skip_threshold`.
    pub fn skip_threshold_level(&self) -> crate::orchestrator::risk_assessment::RiskLevel {
        match self.skip_threshold.to_lowercase().as_str() {
            "medium" => crate::orchestrator::risk_assessment::RiskLevel::Medium,
            _ => crate::orchestrator::risk_assessment::RiskLevel::High,
        }
    }
}

impl Default for RiskAssessmentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ai_assessment: false,
            skip_threshold: "high".to_string(),
        }
    }
}

/// Model/effort pair for a single complexity band.
#[derive(Debug, Clone)]
pub struct TierSpec {
    pub model: String,
    pub effort: String,
}

impl TierSpec {
    pub fn new(model: &str, effort: &str) -> Self {
        Self { model: model.to_string(), effort: effort.to_string() }
    }
}

/// Resolved test-generation tiers: one `TierSpec` per complexity band.
///
/// Defaults use haiku for trivial chunks and escalate through sonnet to opus
/// for the most complex methods.
#[derive(Debug, Clone)]
pub struct TestGenTiers {
    pub trivial: TierSpec,
    pub low: TierSpec,
    pub medium: TierSpec,
    pub high: TierSpec,
    pub complex: TierSpec,
}

impl Default for TestGenTiers {
    fn default() -> Self {
        Self {
            trivial:  TierSpec::new("haiku", "low"),
            low:      TierSpec::new("sonnet", "low"),
            medium:   TierSpec::new("sonnet", "medium"),
            high:     TierSpec::new("sonnet", "high"),
            complex:  TierSpec::new("opus", "high"),
        }
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

        // -- resolve compliance config (US-069/US-073) --
        let compliance = {
            let yaml_compliance = yaml_config.as_ref()
                .map(|y| &y.compliance)
                .cloned()
                .unwrap_or_default();
            match crate::yaml_config::resolve_compliance_config(&yaml_compliance, &path) {
                Ok(mut c) => {
                    // health_mode enables the risk class column in the matrix
                    c.include_risk_class_column = self.health_mode;
                    c
                }
                Err(e) => {
                    tracing::warn!("compliance config error: {} — using defaults", e);
                    crate::compliance::ComplianceConfig::default()
                }
            }
        };

        // -- resolve project commands (US-014) --
        let mut commands = crate::yaml_config::resolve_commands(
            yaml_config.as_ref(),
            &self.test_command,
            &self.coverage_command,
        );
        // US-059: CLI override takes precedence over YAML and auto-detection.
        if let Some(v) = self.tests_in_coverage {
            commands.tests_embedded_in_coverage = v;
        }
        if commands.tests_embedded_in_coverage {
            tracing::info!(
                "US-059: tests_embedded_in_coverage=true — the `test` command will be skipped \
                 during coverage boost (coverage command already runs the test suite)"
            );
        }
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
            min_branch_coverage: if self.skip_coverage { 0.0 } else { self.min_branch_coverage },
            min_file_branch_coverage: if self.skip_coverage { 0.0 } else { self.min_file_branch_coverage },
            skip_overlap: self.skip_overlap,
            include_test_issues: self.include_test_issues,
            sonar_exclusions: self.sonar_exclusions.clone(),
            skip_coverage: self.skip_coverage,
            skip_format: self.skip_format,
            allow_wip: self.allow_wip,
            coverage_attempts: self.coverage_attempts,
            coverage_rounds: self.coverage_rounds,
            coverage_exclude: self.coverage_exclude.clone(),
            coverage_wave_size: std::cmp::max(1, self.coverage_wave_size),
            coverage_commit_batch: if self.coverage_commit_batch == 0 {
                std::cmp::max(1, self.coverage_wave_size)
            } else {
                self.coverage_commit_batch
            },
            fix_commit_batch: self.fix_commit_batch,
            coverage_parallel: std::cmp::max(1, self.coverage_parallel),
            parallel: std::cmp::max(1, self.parallel),
            max_boost_failures: self.max_boost_failures,
            chunk_snippet_max_lines: self.chunk_snippet_max_lines,
            execution_log_report_dir: self.execution_log_report_dir.clone(),
            // --health-mode implies --compliance (baseline is a prerequisite)
            compliance_enabled: self.compliance_enabled || self.health_mode,
            health_mode: self.health_mode,
            retry_failed_wave_files: !self.skip_retry_failed_wave_files,
            skip_final_validation: self.skip_final_validation,
            final_validation_attempts: self.final_validation_attempts,
            targeted_tests_first: !self.skip_targeted_tests,
            // 0 is meaningful: defer all rescans to a single end-of-run scan.
            rescan_batch_size: self.rescan_batch_size,
            wave_scan_batch: self.wave_scan_batch.max(1),
            lean_ai: !self.no_lean_ai,

            skip_dedup: self.skip_dedup,
            max_dedup: self.max_dedup,
            skip_fixes: self.skip_fixes,
            protected_files: self.protected_files,
            commit_format: if self.commit_format.is_empty() { "{type}({scope}): {message}".to_string() } else { self.commit_format },
            commit_vars: self.commit_vars,
            commit_issue: self.commit_issue,
            skip_docs: self.skip_docs,
            documentation: DocumentationConfig::default(),
            skip_pact: self.skip_pact,
            skip_rebase: self.skip_rebase,
            skip_linter_scan: self.skip_linter_scan,
            linter_autofix: self.linter_autofix,
            max_linter_findings: self.max_linter_findings,
            skip_linter_fastpath: self.skip_linter_fastpath,
            skip_issue_grouping: self.skip_issue_grouping,
            max_group_size: self.max_group_size,
            disable_wave_batching: self.disable_wave_batching,
            skip_autofix_sonar: self.skip_autofix_sonar,
            baseline_lcov_path: None,
            pact: self.pact,
            engine_routing: crate::engine::EngineRoutingConfig {
                engines: personal_config.engines.clone(),
                routing: personal_config.routing.clone(),
            },
            test_generation: self.test_generation,
            compliance,
            risk_assessment: self.risk_assessment,
            skip_clean_when_safe: self.skip_clean_when_safe,
            wave_grouping_depth: self.wave_grouping_depth,
            rule_blocklist: self.rule_blocklist,
            hard_case_blocklist: self.hard_case_blocklist,
            fresh_worktree: false,
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
        if self.parallel > 1 {
            info!("  Parallel:        {} worktrees", self.parallel);
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
            min_branch_coverage: 0.0,
            min_file_branch_coverage: 0.0,
            skip_overlap: false,
            include_test_issues: false,
            sonar_exclusions: vec![],
            skip_coverage: false,
            skip_format: false,
            allow_wip: false,
            coverage_attempts: 3,
            coverage_rounds: 3,
            coverage_exclude: vec![],
            coverage_wave_size: 3,
            coverage_commit_batch: 0,
            fix_commit_batch: 1,
            coverage_parallel: 1,
            max_boost_failures: 5,
            chunk_snippet_max_lines: 80,
            execution_log_report_dir: ".reparo".to_string(),
            tests_in_coverage: None,
            compliance_enabled: false,
            health_mode: false,
            skip_retry_failed_wave_files: false,
            skip_final_validation: false,
            skip_targeted_tests: false,
            rescan_batch_size: 1,
            wave_scan_batch: 1,
            no_lean_ai: false,
            final_validation_attempts: 5,
            skip_dedup: false,
            max_dedup: 10,
            skip_fixes: false,
            protected_files: vec![],
            commit_format: "{type}({scope}): {message}".to_string(),
            commit_vars: std::collections::HashMap::new(),
            skip_docs: false,
            documentation: DocumentationConfig::default(),
            skip_pact: false,
            skip_rebase: false,
            skip_linter_scan: false,
            linter_autofix: false,
            max_linter_findings: 200,
            skip_linter_fastpath: false,
            skip_issue_grouping: false,
            max_group_size: 20,
            disable_wave_batching: false,
            skip_autofix_sonar: false,
            pact: PactConfig::default(),
            restore_personal_yaml: false,
            commit_issue: None,
            test_generation: TestGenerationConfig::default(),
            risk_assessment: RiskAssessmentConfig::default(),
            parallel: 1,
            skip_clean_when_safe: true,
            wave_grouping_depth: 1,
            rule_blocklist: Vec::new(),
            hard_case_blocklist: Vec::new(),
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

    #[test]
    fn test_max_boost_failures_default() {
        let config = Config::parse_from([
            "reparo", "--path", ".",
            "--sonar-url", "http://localhost:9000",
            "--sonar-token", "tok",
            "--sonar-project-id", "proj",
        ]);
        assert_eq!(config.max_boost_failures, 5);
    }

    #[test]
    fn test_max_boost_failures_custom() {
        let config = Config::parse_from([
            "reparo", "--path", ".",
            "--sonar-url", "http://localhost:9000",
            "--sonar-token", "tok",
            "--sonar-project-id", "proj",
            "--max-boost-failures", "10",
        ]);
        assert_eq!(config.max_boost_failures, 10);
    }

    #[test]
    fn test_max_boost_failures_disabled() {
        let config = Config::parse_from([
            "reparo", "--path", ".",
            "--sonar-url", "http://localhost:9000",
            "--sonar-token", "tok",
            "--sonar-project-id", "proj",
            "--max-boost-failures", "0",
        ]);
        assert_eq!(config.max_boost_failures, 0);
    }

    #[test]
    fn test_retry_failed_wave_files_default() {
        let config = Config::parse_from([
            "reparo", "--path", ".",
            "--sonar-url", "http://localhost:9000",
            "--sonar-token", "tok",
            "--sonar-project-id", "proj",
        ]);
        assert!(!config.skip_retry_failed_wave_files);
    }

    #[test]
    fn test_retry_failed_wave_files_disabled() {
        let config = Config::parse_from([
            "reparo", "--path", ".",
            "--sonar-url", "http://localhost:9000",
            "--sonar-token", "tok",
            "--sonar-project-id", "proj",
            "--skip-retry-failed-wave-files",
        ]);
        assert!(config.skip_retry_failed_wave_files);
    }
}
