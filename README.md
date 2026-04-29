# Reparo

Automated SonarQube technical debt fixer powered by AI.

Reparo scans your SonarQube project, prioritizes issues by severity, ensures test coverage meets configurable thresholds, fixes each issue using AI, validates that tests still pass, verifies the fix with SonarQube, and optionally creates a pull request — all autonomously.

## Features

- **Multi-engine AI routing** — Route tasks to Claude, Gemini, or Aider based on complexity tiers
- **Coverage boost** — Automatically generate tests to reach project-wide and per-file coverage thresholds
- **Contract/pact testing** — Verify API contracts before and after each fix
- **Deduplication** — Refactor duplicated code after fixes
- **Final validation** — Full test suite run with auto-repair on failure
- **Documentation quality** — ISO 25000 / MDR compliance checks
- **Self-healing** — Automatic retry on build, test, lint, or SonarQube verification failures
- **Protected files** — Prevent AI from modifying lock files, configs, etc.
- **Custom commit formats** — Templated commit messages with placeholders
- **Resume support** — Pick up where you left off after interruptions
- **Personal config** — User-level defaults in `~/.config/reparo/config.yaml`
- **Sonar autofix fast-path** — OpenRewrite runs before the AI loop to resolve mechanical Sonar rules (S1128, S1481, S1068, S1118, S1161, S1124, S2293, …) with zero AI calls
- **Local linter autofix fast-path** — Per-issue `--fix` dispatch to clippy / eslint / ruff; AI is called only when the linter can't resolve it
- **Lint issue grouping** — Same-file / same-rule lint findings are batched into a single AI call
- **Maven speedups** — `mvnd` auto-detection, module-scoped targeted tests (`-pl :<module> -am`), `target/` preserved across worktree reuse
- **Smart rescan batching** — `--rescan-batch-size 0` defers all per-issue rescans to a single end-of-run verification
- **Adaptive parallelism** — `--parallel N` with auto-bump to 2 workers when queue > 20 issues

## Quick Start

```bash
# Build
cargo build --release

# Run on a project (SonarQube must be running)
./target/release/reparo \
  --path /path/to/your/project \
  --sonar-project-id my-project-key \
  --sonar-token $SONAR_TOKEN

# With a YAML config file (recommended)
./target/release/reparo \
  --path /path/to/your/project \
  --config ./reparo.yaml

# Dry run (analyze without fixing)
./target/release/reparo \
  --path ./my-project \
  --sonar-project-id my-project-key \
  --dry-run --skip-scan
```

## Prerequisites

- **Rust** 1.70+ (to build)
- **SonarQube** server (local or remote) with an existing project analysis
- **sonar-scanner** in PATH (or Maven/Gradle for Java projects)
- **At least one AI CLI** installed and authenticated:
  - **Claude CLI** (`claude -d` must work) — default engine
  - **Gemini CLI** (`gemini`) — optional
  - **Aider** (`aider`) — optional
- **GitHub CLI** (`gh`) installed and authenticated (for PR creation)
- **Git** repository with a remote configured

## How It Works

```
1. Validate config + check SonarQube connectivity + detect edition
2. Load personal config (~/.config/reparo/config.yaml)
   - Create with defaults if it doesn't exist
   - Warn if version mismatch with current binary
3. Create a single fix branch (fix/sonar-<timestamp>)
   a. Initial formatting (unless --skip-format):
      - Run the `format` command on the entire project
      - Commit formatting changes separately (before any fixes)
   b. Setup command (if defined):
      - Run `commands.setup` (e.g., npm install) to prepare the environment
4. Pre-flight checks:
   a. Build must pass
   b. Tests must pass
   c. Coverage boost (unless --skip-coverage):
      - Run coverage command and parse lcov report
      - Sort files by coverage ascending (least covered first)
      - For each file: multi-round loop (up to coverage_rounds per file):
        - Generate tests → verify only test files created → run tests → commit
        - Re-measure coverage → stop if threshold met or no improvement
      - Repeat until project-wide and per-file thresholds are met
5. Run coverage command, then initial SonarQube scan
6. Fix loop (until --max-issues reached or no issues left):
   a. Fetch fresh issues from SonarQube (most critical first)
   b. Skip non-coverable files (CSS/SCSS/HTML — no unit test coverage possible)
   c. Check line-level test coverage for affected code
   d. Generate unit tests if coverage < 100% (up to N retries)
   e. Contract/pact testing (unless --skip-pact):
      - Check if file involves API contracts
      - Generate contract tests if needed
      - Verify pact contracts before/after fix
   f. Clean → Fix via AI → Format → Build (with retry) → Test (with retry) → Lint (with auto-fix)
   g. If AI modifies test files: auto-revert test changes, keep source fix if tests still pass
   h. Re-scan with SonarQube to verify the specific issue is resolved (up to N retries)
   i. Commit if verified, revert if not
7. Deduplication (unless --skip-dedup):
   a. Fetch files with duplicated code from SonarQube (most duplicated first)
   b. Ensure 100% test coverage of duplicated ranges
   c. Ask AI to refactor and eliminate duplication
   d. Verify build + tests pass, no test files modified
   e. Re-scan with SonarQube to verify duplication is reduced
   f. Commit if verified, revert if not
8. Final validation (unless --skip-final-validation):
   a. Run the FULL test suite (all tests, not just per-issue)
   b. If any test fails, ask AI to fix source code (never test files)
   c. Iterate up to final_validation_attempts times (default: 5)
   d. Only accept when ALL tests pass in a single execution
9. Documentation quality (if documentation.enabled):
   a. Check code documentation against quality standards
   b. Generate or improve documentation as needed
10. Create PR (unless --no-pr)
11. Generate REPORT.md, TECHDEBT_CHANGELOG.md (committed on the fix branch)
```

## Multi-Engine AI Routing

Reparo supports multiple AI engines and routes tasks to the most appropriate one based on complexity tiers. This lets you use cheaper/faster models for simple fixes and more capable models for complex ones.

### Supported Engines

| Engine | CLI Command | Default Args | Use Case |
|--------|-------------|-------------|----------|
| **Claude** | `claude` | `-d --output-format text` | Default for all tiers |
| **Gemini** | `gemini` | — | Alternative for any tier |
| **Aider** | `aider` | `--yes-always --no-git` | Local models via Aider |

### Complexity Tiers

Tasks are automatically classified into 4 tiers based on the SonarQube rule and effort:

| Tier | Complexity | Default Engine | Examples |
|------|-----------|----------------|----------|
| **tier1** | Simple | Claude Haiku (low effort) | Unused imports, trivial fixes |
| **tier2** | Medium | Claude Sonnet (medium effort) | Moderate refactoring |
| **tier3** | Complex | Claude Opus (high effort) | Significant logic changes |
| **tier4** | Very complex | Claude Opus (max effort) | Deep refactoring, high cognitive complexity |

### Custom Routing Example

Route simple tasks to a local model via Aider, keep complex tasks on Claude:

```yaml
# In ~/.config/reparo/config.yaml (personal) or reparo.yaml (project)
engines:
  claude:
    command: "claude"
    args: ["-d", "--output-format", "text"]
    enabled: true
    prompt_flag: "-p"
  aider:
    command: "aider"
    args: ["--yes-always", "--no-git"]
    enabled: true
    prompt_flag: "--message"

routing:
  tier1:
    engine: "aider"
    model: "qwen-coder-30b"
  tier2:
    engine: "claude"
    model: "sonnet"
    effort: "medium"
  tier3:
    engine: "claude"
    model: "opus"
    effort: "high"
  tier4:
    engine: "claude"
    model: "opus"
    effort: "max"
```

Reparo validates at startup that all engines referenced in routing are enabled and available in PATH.

## Configuration

Reparo has a layered configuration system with clear priority:

```
CLI flags > Environment variables > Project YAML > Personal YAML > Defaults
```

### Personal Config (`~/.config/reparo/config.yaml`)

User-level defaults that apply across **all** projects. Contains engine routing, global timeouts, and personal preferences.

- **Auto-created**: If the file doesn't exist, Reparo creates it with sensible defaults on first run.
- **Auto-completed**: If the file exists but is missing fields, Reparo fills them in via `serde(default)`.
- **Version tracking**: The file stores the Reparo version that generated it. If it doesn't match the running binary, a warning is shown encouraging you to update.
- **Reset**: Use `--restore-personal-yaml` to restore defaults for the current version.

```bash
# Reset personal config to defaults
reparo --restore-personal-yaml
```

### Project Config (`reparo.yaml`)

Per-project configuration, typically checked into the repository. Supports `${ENV_VAR}` interpolation.

### CLI Flags

```
OPTIONS:
  --path <PATH>                    Project path (required)
  --sonar-project-id <ID>          SonarQube project ID (or set in YAML: sonar.project_id)
  --sonar-url <URL>                SonarQube URL [default: http://localhost:9000]
  --sonar-token <TOKEN>            SonarQube auth token (or set in YAML/env)
  --branch <BRANCH>                Base branch [default: current branch]
  --batch-size <N>                 Issues per PR (0 = all in one) [default: 1]
  --test-command <CMD>             Test command (auto-detected if omitted)
  --coverage-command <CMD>         Coverage command (auto-detected if omitted)
  --dry-run                        Analyze without fixing
  --max-issues <N>                 Max issues to process (0 = all) [default: 0]
  --reverse-severity               Process least severe issues first (INFO → BLOCKER)
  --min-coverage <PCT>             Minimum project-wide test coverage before fixing [default: 80]
  --min-file-coverage <PCT>        Minimum per-file test coverage [default: 0]
  --coverage-attempts <N>          Test generation / fix retry attempts per issue [default: 3]
  --coverage-rounds <N>            Max rounds per file during coverage boost (0 = unlimited while improving) [default: 3]
  --final-validation-attempts <N>  Max repair attempts for final full-suite validation [default: 5]
  --max-dedup <N>                  Max deduplication iterations (0 = unlimited) [default: 10]
  --no-pr                          Skip creating a pull request after fixes
  --dangerously-skip-permissions   Pass --dangerously-skip-permissions to Claude CLI
  --show-prompts                   Print prompts sent to AI (for debugging)
  --log-format <text|json>         Log format [default: text]
  --test-timeout <SECS>            Per-test-run timeout [default: 600]
  --claude-timeout <SECS>          Per AI call timeout [default: 300]
  --timeout <SECS>                 Global timeout (0 = none) [default: 0]
  --skip-scan                      Skip sonar-scanner, use existing analysis
  --scanner-path <PATH>            Scanner binary path (auto-detected)
  --config <PATH>                  YAML config file path
  --resume                         Resume a previously interrupted execution
  --restore-personal-yaml          Reset personal config to defaults and exit

STEP FLAGS (each step can be enabled/disabled independently):
  --skip-format                    Skip the initial format-and-commit step
  --skip-coverage                  Skip the coverage boost step
  --skip-fixes                     Skip the fix loop (coverage boost and preflight still run)
  --skip-pact                      Skip the pact/contract testing step
  --skip-dedup                     Skip the deduplication step after fixes
  --skip-final-validation          Skip the final full test suite validation
  --skip-docs                      Skip the documentation quality step
  --skip-linter-scan               Skip the local linter discovery phase (Step 3d)
  --skip-linter-fastpath           Skip the per-issue linter autofix fast-path (A1)
  --skip-issue-grouping            Skip same-file/same-rule lint issue grouping (A3)
  --skip-autofix-sonar             Skip the Sonar autofix fast-path (OpenRewrite)

LATENCY OPTIMIZATION FLAGS:
  --parallel <N>                   Per-issue parallelism via git worktrees [default: 1; auto-2 when queue > 20]
  --coverage-parallel <N>          Files processed in parallel during coverage boost [default: 1]
  --coverage-wave-size <N>         Files per coverage-boost wave [default: 3]
  --coverage-commit-batch <N>      Files per coverage-boost commit (0 = wave size)
  --rescan-batch-size <N>          Sonar rescan every N fixes; 0 = single end-of-run scan [default: 1]
  --fix-commit-batch <N>           WIP fix commits to squash before pushing [default: 1]
  --max-group-size <N>             Max lint findings per batched AI call [default: 20]
  --targeted-tests-first / --skip-targeted-tests
                                   Run Surefire-filtered tests before the full suite (Maven only)
```

### Environment Variables

All parameters can be set via environment variables:

| Variable | Maps to |
|----------|---------|
| `SONAR_PROJECT_ID` | `--sonar-project-id` |
| `SONAR_URL` | `--sonar-url` |
| `SONAR_TOKEN` | `--sonar-token` |
| `REPARO_BATCH_SIZE` | `--batch-size` |
| `REPARO_TEST_COMMAND` | `--test-command` |
| `REPARO_COVERAGE_COMMAND` | `--coverage-command` |
| `REPARO_DRY_RUN` | `--dry-run` |
| `REPARO_MAX_ISSUES` | `--max-issues` |
| `REPARO_MIN_COVERAGE` | `--min-coverage` |
| `REPARO_MIN_FILE_COVERAGE` | `--min-file-coverage` |
| `REPARO_LOG_FORMAT` | `--log-format` |
| `REPARO_TEST_TIMEOUT` | `--test-timeout` |
| `REPARO_CLAUDE_TIMEOUT` | `--claude-timeout` |
| `REPARO_TIMEOUT` | `--timeout` |
| `REPARO_SKIP_SCAN` | `--skip-scan` |
| `REPARO_SCANNER_PATH` | `--scanner-path` |
| `REPARO_NO_PR` | `--no-pr` |
| `REPARO_COVERAGE_ATTEMPTS` | `--coverage-attempts` |
| `REPARO_COVERAGE_ROUNDS` | `--coverage-rounds` |
| `REPARO_FINAL_VALIDATION_ATTEMPTS` | `--final-validation-attempts` |
| `REPARO_MAX_DEDUP` | `--max-dedup` |
| `REPARO_PARALLEL` | `--parallel` |
| `REPARO_COVERAGE_PARALLEL` | `--coverage-parallel` |
| `REPARO_RESCAN_BATCH_SIZE` | `--rescan-batch-size` |
| `REPARO_SKIP_LINTER_FASTPATH` | `--skip-linter-fastpath` |
| `REPARO_SKIP_ISSUE_GROUPING` | `--skip-issue-grouping` |
| `REPARO_MAX_GROUP_SIZE` | `--max-group-size` |
| `REPARO_SKIP_AUTOFIX_SONAR` | `--skip-autofix-sonar` |
| `REPARO_LINTER_AUTOFIX` | `--linter-autofix` |
| `REPARO_MAX_LINTER_FINDINGS` | `--max-linter-findings` |
| `REPARO_ALLOW_WIP` | `--allow-wip` |

### YAML Configuration (`reparo.yaml`)

Place a `reparo.yaml` (or custom named file via `--config`) in your project root for versioned, repeatable configuration. Supports `${ENV_VAR}` interpolation.

```yaml
sonar:
  project_id: "com.example:my-project"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

git:
  branch: "develop"
  batch_size: 5
  commit_format: "{type}({scope}): {message}"  # commit message template
  commit_vars:                                   # custom variables for commit format
    team: "platform"

execution:
  max_issues: 20
  reverse_severity: false       # true = process least severe first (default: false)
  min_coverage: 80              # project-wide % test coverage before fixing (0 = disabled)
  min_file_coverage: 50         # per-file % — boost files individually below this (0 = disabled)
  timeout: 3600                 # global timeout in seconds
  test_timeout: 600             # per-test-run timeout
  claude_timeout: 600           # per AI call timeout
  # Step switches (each step can be enabled/disabled independently)
  format_on_start: true         # run formatter and commit before fixes (default: true)
  coverage_boost: true          # run coverage boost step (default: true)
  coverage_attempts: 3          # test gen / fix retry attempts per issue (default: 3)
  coverage_rounds: 3            # max rounds per file during boost, 0 = unlimited while improving (default: 3)
  coverage_exclude:              # glob patterns — skip these files during coverage boost (default: none)
    - "*.html"
    - "**/generated/**"
  final_validation: true        # run full test suite after all fixes (default: true)
  final_validation_attempts: 5  # max repair attempts for final validation (default: 5)
  dedup_on_completion: true     # refactor duplicated code after fixes (default: true)
  max_dedup: 10                 # max dedup iterations (0 = unlimited, default: 10)

# AI engine routing (also configurable in personal config)
engines:
  claude:
    command: "claude"
    args: ["-d", "--output-format", "text"]
    enabled: true
    prompt_flag: "-p"
  gemini:
    command: "gemini"
    args: []
    enabled: false
    prompt_flag: "-p"
  aider:
    command: "aider"
    args: ["--yes-always", "--no-git"]
    enabled: false
    prompt_flag: "--message"

routing:
  tier1:
    engine: "claude"
    model: "haiku"
    effort: "low"
  tier2:
    engine: "claude"
    model: "sonnet"
    effort: "medium"
  tier3:
    engine: "claude"
    model: "opus"
    effort: "high"
  tier4:
    engine: "claude"
    model: "opus"
    effort: "max"

# Contract/pact testing (presence of the section enables the phase;
# use `--skip-pact` to opt out, or `enabled: false` to keep config but disable)
pact:
  enabled: true                 # defaults to true when the section is present
  pact_dir: "./pacts"           # path to pact files (can be shared across projects)
  provider_name: "MyAPI"        # provider name (improves prompt quality)
  consumer_name: "MyFrontend"   # consumer name (improves prompt quality)
  verify_command: "npm run test:pact:verify"  # required if any verify_* step is enabled
  test_command:   "npm run test:pact"         # required if generate_tests is enabled
  check_contracts: false        # check if file involves API contracts
  generate_tests: false         # generate contract tests for API files
  verify_before_fix: false      # verify contracts pass before applying fix
  verify_after_fix: false       # verify contracts still pass after fix

# Documentation quality (default: disabled)
documentation:
  enabled: false                # enable documentation quality checks

# Protected files — AI will never modify these (matched by basename)
protected_files:
  - "package-lock.json"
  - "yarn.lock"

# Project commands — executed directly, no AI involved
commands:
  setup: "npm install"                        # run once before pre-flight (e.g., install deps)
  clean: "npm run build -- --delete-output-path"
  build: "npm run build"
  test: "npm test"
  coverage: "npm run test:coverage"
  format: "npx prettier --write ."
  lint: "npx eslint src --max-warnings=0"
```

### Step Enable/Disable Reference

Every optional step can be controlled via CLI flags and/or YAML. CLI flags always take priority over YAML.

| Step | CLI flag | YAML field | Default |
|------|----------|------------|---------|
| Initial formatting | `--skip-format` | `execution.format_on_start: false` | enabled |
| Coverage boost | `--skip-coverage` | `execution.coverage_boost: false` | enabled |
| Fix loop | `--skip-fixes` | — | enabled |
| Contract/pact testing | `--skip-pact` | `pact:` section present | enabled when section present (hard error if missing) |
| Local linter scan | `--skip-linter-scan` | `execution.linter_scan: false` | enabled when `commands.lint` set |
| Per-issue linter autofix (A1) | `--skip-linter-fastpath` | — | enabled for `lint:*` findings |
| Lint issue grouping (A3) | `--skip-issue-grouping` | — | enabled |
| Sonar autofix (OpenRewrite) | `--skip-autofix-sonar` | — | enabled for Maven projects |
| Deduplication | `--skip-dedup` | `execution.dedup_on_completion: false` | enabled |
| Final validation (tests) | `--skip-final-validation` | `execution.final_validation: false` | enabled |
| Documentation quality | `--skip-docs` | `documentation.enabled: true` | disabled |
| PR creation | `--no-pr` | — | enabled |

**Command execution order** after each fix:

1. `clean` (if defined) — clean artifacts before each fix *(skipped for `lint:*` findings)*
2. `format` (if defined) — format code, changes included in commit *(skipped for `lint:*` findings)*
3. `build` (if defined) — compile, **retries with AI on failure** (up to `coverage_attempts` times) *(skipped for `lint:*` findings; incremental compile in the test phase catches regressions)*
4. `test` (required) — run tests, **retries with AI on failure** (up to `coverage_attempts` times)
5. `lint` (if defined) — static analysis, **auto-fixed by AI** (up to `coverage_attempts` times)

## Latency Optimizations

Reparo stacks several AI-free or AI-light fast-paths to keep wall-clock down on large queues. Each runs automatically unless its skip flag is set.

### Sonar autofix via OpenRewrite (Maven only)

Before the AI fix loop, Reparo scans the queue for Sonar rule keys mapped to OpenRewrite recipes (16 mapped today: `S1128` unused imports, `S1481`/`S1068` unused local/field, `S1118` utility-class constructor, `S1161` missing `@Override`, `S1124` modifier order, `S2293` diamond operator, `S1155` isEmpty, `S1488` inline return, `S1066`/`S1125`/`S1126` boolean simplification, `S1905` unnecessary cast, `S6201` instanceof pattern, `S1165` final field, `S2864` entrySet, …). A single `mvn rewrite:run` with the union of matching recipes resolves dozens to hundreds of findings in ~60-120s. Files that OpenRewrite touches are accepted as mechanically fixed and skip the AI path. The full GAV is invoked — no pom.xml edits required. Disable with `--skip-autofix-sonar`.

### Per-issue linter autofix fast-path (clippy / eslint / ruff)

For `lint:<format>:<rule>` findings produced by the local linter scan, Reparo first runs the linter's own `--fix` scoped to the rule + file. If the file changes, the AI call is skipped. Typical savings: ~30-60s per auto-fixable finding → ~1-5s. Disable with `--skip-linter-fastpath`.

### Lint issue grouping

When the same `lint:<format>:<rule>` fires N times in one file, Reparo batches them into a single AI call (representative issue with all line ranges enumerated in the prompt). Caps at `--max-group-size` (default 20). Restricted to lint findings today so the Sonar re-fetch path stays simple. Disable with `--skip-issue-grouping`.

### Maven-specific speedups

- **`mvnd` auto-detection**: if `mvnd` is on `PATH`, Reparo rewrites `mvn …` test commands to `mvnd …` (2-3× faster cold builds). `./mvnw` invocations are preserved — if the project pinned a wrapper, Reparo respects it.
- **Module-scoped targeted tests**: Reparo derives the enclosing Maven module of the changed file and injects `-pl :<module> -am` alongside the Surefire `-Dtest=` filter. Skipped for single-module projects.
- **Clean/build/format skip for `lint:*` findings**: these linter-origin edits don't need a clean lifecycle; the test phase's incremental compile catches any regression.

### Rescan batching

`--rescan-batch-size N` controls how often the SonarQube rescan runs:
- `N = 1` (default): rescan after every fix — strict verification, slowest.
- `N > 1`: rescan every N fixes — amortizes ~30-60s per batch.
- `N = 0`: defer all per-issue rescans; one rescan at the very end of the run diffs against the original queue and downgrades any still-open issue to `NeedsReview`. Largest single saving on long queues.

`lint:*` findings never trigger a Sonar rescan (they're synthetic and Sonar doesn't track them).

### Parallelism

`--parallel N` runs issues concurrently in git worktrees. Reparo auto-bumps `N` to 2 when the queue > 20 issues unless `REPARO_PARALLEL` is explicitly set. For Maven projects the worktree pool preserves `target/`, `node_modules/`, `.gradle/`, `.venv/` across reuses — every issue on a reused worktree gets incremental compile instead of cold start.

### Trivial-fix heuristics

- **Trivial full-suite skip**: when targeted tests pass on a `lint:*` finding or a MINOR/INFO single-file change ≤ 5 lines, Reparo defers the full suite to the final-validation step even with `batch-size=1`.
- **Repair-loop cap**: 1 repair attempt instead of `coverage_attempts` for `lint:*` and MINOR/INFO non-coverage-dependent rules.
- **Haiku routing**: `lint:*` at MINOR severity routes to `haiku:low` (guarded by regression test).

### Pre-queue dedup

Overlapping findings in the same `(file, rule)` are collapsed before the queue is built. A finding whose `text_range` is fully contained in another of the same rule is dropped (the outer finding carries the fix).

### End-of-run Sonar verification (`--rescan-batch-size 0` only)

After the fix loop completes, a single scan + `wait_for_analysis` + `fetch_issues` compares remaining open issues against what we marked Fixed. Any still-open issue gets retroactively marked `NeedsReview` with a note that manual verification is required.

### Pending work

See [`US/US-080-pending-latency-improvements.md`](US/US-080-pending-latency-improvements.md) for the list of deferred items (B3 sequential pipelining, ESLint v9 flat-config fallback, batched native prompt for groups, Sonar-rule grouping, Checkstyle/SpotBugs/PMD parsers, OpenRewrite recipe map validation, Gradle autofix-sonar, preflight-tolerate flag).

## Coverage Boost

Before fixing any SonarQube issues, Reparo can automatically generate tests to bring coverage up to configurable thresholds — both project-wide and per-file.

**Two thresholds:**

- `--min-coverage` (default 80%): Minimum **project-wide** coverage. Files are boosted starting from the least covered until the overall % is met.
- `--min-file-coverage` (default 0 = disabled): Minimum **per-file** coverage. Even if the overall threshold is met, individual files below this % are still boosted.

This means you can enforce, for example, "80% overall and no file below 50%".

**How it works:**

1. Runs the `coverage` command to generate an lcov report
2. Parses per-file coverage from the lcov report
3. Sorts files by coverage ascending (least covered first — most efficient for boosting overall %)
4. For each file that needs boosting (overall too low OR file below per-file threshold):
   - **Multi-round loop** controlled by `coverage_rounds` (default: 3):
     - Round 1: asks AI to generate unit tests for uncovered lines
     - Round 2+: uses a retry prompt with previous test output, targeting lines still uncovered
     - Each round: verify only test files were created → run tests → commit if passing → re-measure coverage
   - **Stops when**: the per-file threshold is met, max rounds exhausted, or (in unlimited mode) coverage stops improving
   - **Strictly enforces**: only test files may be created/modified. If source code is touched, all changes are reverted
5. Stops processing "overall boost" files once the project-wide threshold is reached, but continues boosting files below the per-file threshold

**Coverage rounds (`--coverage-rounds`):**

Controls how many full generate-test-measure rounds are attempted per file:

| Value | Behavior |
|-------|----------|
| `3` (default) | Up to 3 rounds per file |
| `N > 0` | Up to N rounds per file |
| `0` | Unlimited — keeps generating tests as long as coverage improves between rounds (safety cap: 50 rounds) |

This is separate from `--coverage-attempts`, which controls retry attempts for test generation within the per-issue fix loop.

**Configuration:**

```bash
# Default: 80% project-wide, no per-file threshold, 3 rounds per file
reparo --path ./my-project --config ./reparo.yaml

# Custom thresholds
reparo --path ./my-project --config ./reparo.yaml --min-coverage 60 --min-file-coverage 30

# Unlimited rounds — keep generating until coverage stops improving
reparo --path ./my-project --config ./reparo.yaml --coverage-rounds 0

# More rounds per file for stubborn coverage gaps
reparo --path ./my-project --config ./reparo.yaml --coverage-rounds 10

# Per-file only (no overall requirement)
reparo --path ./my-project --config ./reparo.yaml --min-coverage 0 --min-file-coverage 50

# Disable coverage boost entirely
reparo --path ./my-project --config ./reparo.yaml --skip-coverage
```

In YAML:

```yaml
execution:
  min_coverage: 60          # project-wide threshold (0 = disabled)
  min_file_coverage: 50     # per-file threshold (0 = disabled)
  coverage_rounds: 3        # rounds per file (0 = unlimited while improving)
  coverage_exclude:           # skip these file patterns during boost (default: none)
    - "*.html"
    - "**/generated/**"
```

**File exclusions (`coverage_exclude`):**

No file formats are excluded by default — what's coverable varies by project (e.g., Angular templates have Istanbul coverage). Use glob patterns to skip files that shouldn't receive generated tests:

```yaml
execution:
  coverage_exclude:
    - "*.html"            # Angular templates
    - "*.htm"
    - "**/generated/**"   # auto-generated code
    - "**/mocks/**"       # mock files
    - "**/*.module.ts"    # Angular modules (no logic to test)
```

## Contract/Pact Testing

Reparo can verify API contracts (pacts) during the fix process, ensuring that fixes don't break API integrations between services. This is disabled by default and must be explicitly enabled.

**How it works:**

1. Before/after each fix, Reparo checks if the affected file involves API contracts
2. If the file is API-related, contract tests are generated or verified against existing pacts
3. Pact verification must pass for the fix to be accepted

**Key features:**

- **Shared pact directory**: The `pact_dir` can point to a path outside the project (e.g., `/shared/pacts/`), allowing multiple projects to share the same contract definitions
- **Granular sub-steps**: Each phase (check, generate, verify before, verify after) can be enabled independently
- **Works with frontend and backend**: API detection is file-based, so it works regardless of whether the project is a consumer (frontend) or provider (backend)

**Configuration:**

```yaml
pact:
  enabled: true
  pact_dir: "/shared/pacts/my-service"   # can be outside the project
  provider_name: "MyAPI"
  consumer_name: "MyFrontend"
  verify_command: "npm run test:pact:verify"
  test_command:   "npm run test:pact"
  check_contracts: true       # detect API-related files
  generate_tests: true        # generate contract tests
  verify_before_fix: true     # contracts must pass before fix
  verify_after_fix: true      # contracts must still pass after fix
```

> If a `pact:` section is present, the phase runs by default. Omit the section and pass
> `--skip-pact`, or keep the section with `enabled: false`, to skip it. Running the phase
> without a `pact:` section is a hard error — Reparo exits before touching any code.

Or disable entirely via CLI:

```bash
reparo --path ./my-project --config ./reparo.yaml --skip-pact
```

## Final Validation

After all individual fixes are applied, Reparo runs a final validation step that executes the **full test suite** in a single run. Individual per-issue tests passing is not sufficient — this step ensures no cross-issue regressions exist.

**How it works:**

1. Run the entire test suite (not per-issue — all tests at once)
2. If any test fails, ask AI to fix the source code (never test files)
3. Iterate up to `final_validation_attempts` times (default: 5)
4. Only accept the batch when **all tests pass in a single execution**
5. Commit any accumulated repair fixes together

**Configuration:**

```yaml
execution:
  final_validation: true        # enable/disable (default: true)
  final_validation_attempts: 5  # max repair iterations (default: 5)
```

Or via CLI:

```bash
# Skip final validation
reparo --path ./my-project --config ./reparo.yaml --skip-final-validation

# Increase repair attempts
reparo --path ./my-project --config ./reparo.yaml --final-validation-attempts 10
```

## Self-Healing Fixes

Reparo includes automatic retry logic to maximize fix success rate:

- **Build failures**: If the build fails after a fix, AI is asked to fix the compilation error (without touching tests). Retried up to `coverage_attempts` times.
- **Test failures**: If tests fail after a fix, AI is asked to fix the code to make tests pass (without modifying test files). Retried up to `coverage_attempts` times.
- **Lint errors**: If the linter reports errors after a fix, AI is asked to fix them automatically. Verified with build+test after each attempt.
- **Test file modifications**: If AI modifies test files during a fix, those changes are automatically reverted. If the source fix still passes tests without the test modifications, the fix is accepted.
- **SonarQube verification failures**: If SonarQube still reports the issue after the fix, the fix is retried up to `coverage_attempts` times with additional context about what didn't work.

## Project Examples

#### Angular/TypeScript

```yaml
sonar:
  project_id: "my-angular-app"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

execution:
  max_issues: 10
  min_coverage: 60
  claude_timeout: 600
  final_validation: true
  final_validation_attempts: 5

pact:
  enabled: true
  pact_dir: "/shared/pacts/my-angular-app"
  verify_after_fix: true

commands:
  setup: "npm install"
  clean: "npm run build -- --delete-output-path"
  build: "npm run build"
  test: "npm test"
  coverage: "npm run test:coverage"
  format: "npx prettier --write ."
  lint: "npx eslint src --max-warnings=0"
```

#### Java/Maven

```yaml
sonar:
  project_id: "com.example:my-service"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

commands:
  clean: "mvn clean"
  build: "mvn compile -DskipTests"
  test: "mvn test"
  coverage: "mvn verify -Pcoverage"
  format: "mvn spotless:apply"
  lint: "mvn checkstyle:check"
```

#### Node.js

```yaml
sonar:
  project_id: "my-frontend"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

commands:
  setup: "npm ci"
  build: "npm run build"
  test: "npm test"
  coverage: "npx jest --coverage"
  format: "npx prettier --write ."
  lint: "npx eslint ."
```

#### Python

```yaml
sonar:
  project_id: "my-service"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

commands:
  test: "python -m pytest"
  coverage: "python -m pytest --cov --cov-report=xml"
  format: "black ."
  lint: "ruff check ."
```

#### Multi-Engine (local model for simple tasks)

```yaml
sonar:
  project_id: "my-project"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

engines:
  claude:
    command: "claude"
    args: ["-d", "--output-format", "text"]
    enabled: true
    prompt_flag: "-p"
  aider:
    command: "aider"
    args: ["--yes-always", "--no-git"]
    enabled: true
    prompt_flag: "--message"

routing:
  tier1:
    engine: "aider"
    model: "qwen-coder-30b"
  tier2:
    engine: "aider"
    model: "qwen-coder-30b"
  tier3:
    engine: "claude"
    model: "opus"
    effort: "high"
  tier4:
    engine: "claude"
    model: "opus"
    effort: "max"

commands:
  test: "npm test"
  coverage: "npx jest --coverage"
```

## Usage Examples

```bash
# Fix up to 5 most critical issues, no PR
reparo --path ./my-project --config ./reparo.yaml --no-pr --max-issues 5

# Fix all issues, create PR
reparo --path ./my-project --config ./reparo.yaml

# Skip coverage boost, fix issues directly
reparo --path ./my-project --config ./reparo.yaml --skip-coverage

# Use YAML config with auto-granted Claude permissions
reparo --path ./my-project --config ./reparo.yaml --dangerously-skip-permissions

# Fix everything in a single PR with 90% coverage threshold
reparo --path ./my-project --config ./reparo.yaml --batch-size 0 --min-coverage 90

# Skip scanner, use existing analysis
reparo --path ./my-project --config ./reparo.yaml --skip-scan

# Dry run with JSON logs
reparo --path ./my-project --config ./reparo.yaml --dry-run --log-format json

# With global timeout (CI/CD)
reparo --path ./my-project --config ./reparo.yaml --timeout 1800

# Debug: show prompts sent to AI
reparo --path ./my-project --config ./reparo.yaml --show-prompts

# Resume an interrupted run
reparo --path ./my-project --config ./reparo.yaml --resume

# Reset personal config to defaults
reparo --restore-personal-yaml
```

## CI/CD Integration

### GitHub Actions

```yaml
name: Fix Tech Debt
on:
  schedule:
    - cron: '0 2 * * 1'  # Weekly Monday 2am
  workflow_dispatch:

jobs:
  fix-debt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Reparo
        run: cargo install --path /path/to/reparo

      - name: Fix technical debt
        env:
          SONAR_URL: ${{ secrets.SONAR_URL }}
          SONAR_TOKEN: ${{ secrets.SONAR_TOKEN }}
          SONAR_PROJECT_ID: ${{ github.repository }}
          REPARO_MAX_ISSUES: "10"
          REPARO_MIN_COVERAGE: "80"
          REPARO_TIMEOUT: "1800"
          REPARO_LOG_FORMAT: "json"
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
        run: reparo --path . --dangerously-skip-permissions
```

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All issues fixed (or none found, or dry-run) |
| 1 | Configuration or connectivity error |
| 2 | Partial success (some fixed, some failed) |
| 3 | Unexpected error |

## Generated Files

| File | Description |
|------|-------------|
| `REPORT.md` | Executive summary: fixed, failed, review needed, statistics by severity/type |
| `TECHDEBT_CHANGELOG.md` | Incremental log of every change attempted (appended, never overwritten) |
| `REVIEW_NEEDED.md` | Issues that need manual review (e.g., fix would break tests or modified test files) |

## How Test Validation Works

Reparo applies strict rules to protect existing code:

1. **Coverage boost phase**: Each file gets up to `coverage_rounds` attempts to reach the threshold. Only test files may be created. If AI modifies any source file, all changes are reverted immediately. In unlimited mode (`coverage_rounds: 0`), rounds continue as long as coverage improves.
2. **Fix phase**: If AI modifies test files during a fix, those test changes are automatically reverted. If the source fix still passes tests, the fix is accepted. Otherwise it's flagged as **NeedsReview**.
3. After every fix, the full test suite runs. If tests fail, AI is asked to fix the code (up to N retries). If all retries fail, the fix is **reverted**.
4. After every fix, SonarQube is re-scanned to **verify the specific issue is resolved**. If not, the fix can be retried up to N times.

## SonarQube Edition Support

Reparo auto-detects the SonarQube edition via the `/api/navigation/global` endpoint:

- **Community Edition**: Branch analysis parameter (`sonar.branch.name`) is automatically omitted
- **Developer/Enterprise Edition**: Full branch analysis support

## Scanner Auto-Detection

Reparo detects the appropriate scanner based on project files:

| File | Scanner |
|------|---------|
| `pom.xml` | `mvn sonar:sonar` |
| `build.gradle` / `build.gradle.kts` | `gradle sonarqube` (prefers `./gradlew`) |
| Other | `sonar-scanner` from PATH |

Override with `--scanner-path` or `sonar.scanner_path` in YAML.

## Architecture

```
src/
  main.rs           Entry point, timeout handling, exit codes, personal config init
  config.rs         CLI parsing, validation, scanner detection
  yaml_config.rs    YAML config loading, env interpolation, merging, personal config
  engine.rs         Multi-engine AI abstraction (Claude, Gemini, Aider), tier routing
  orchestrator.rs   Main workflow loop: format → coverage → pact → fix → dedup → final validation
  sonar.rs          SonarQube API client (issues, coverage, duplications, rules, scanner, edition)
  claude.rs         Claude CLI integration, prompt builders, tier classification
  git.rs            Git operations, PR creation via gh
  runner.rs         Test/build/lint/coverage execution, lcov parsing, framework detection
  pact.rs           Pact/contract testing: API detection, contract test generation, verification
  report.rs         REPORT.md, TECHDEBT_CHANGELOG.md, REVIEW_NEEDED.md
  retry.rs          Retry with exponential backoff for network/CLI calls
  state.rs          Execution state persistence for --resume support
```

## Development

```bash
# Run tests
cargo test

# Build release
cargo build --release

# Run with verbose logging
RUST_LOG=reparo=info cargo run -- --path ./test-project --sonar-project-id test --skip-scan --dry-run
```

## License

MIT
