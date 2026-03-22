# Reparo

Automated SonarQube technical debt fixer using Claude AI.

Reparo scans your SonarQube project, prioritizes issues by severity, ensures test coverage meets a configurable threshold, fixes each issue using `claude -d`, validates that tests still pass, verifies the fix with SonarQube, and optionally creates a pull request — all autonomously.

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
- **Claude CLI** installed and authenticated (`claude -d` must work)
- **GitHub CLI** (`gh`) installed and authenticated (for PR creation)
- **Git** repository with a remote configured

## How It Works

```
1. Validate config + check SonarQube connectivity + detect edition
2. Create a single fix branch (fix/sonar-<timestamp>)
   a. Initial formatting (unless --skip-format):
      - Run the `format` command on the entire project
      - Commit formatting changes separately (before any fixes)
   b. Setup command (if defined):
      - Run `commands.setup` (e.g., npm install) to prepare the environment
3. Pre-flight checks:
   a. Build must pass
   b. Tests must pass
   c. Coverage boost (if below --min-coverage threshold):
      - Run coverage command and parse lcov report
      - Sort files by coverage ascending (least covered first)
      - For each file: generate tests → verify only test files created → run tests → commit
      - Repeat until project-wide and per-file thresholds are met
4. Run coverage command, then initial SonarQube scan
5. Fix loop (until --max-issues reached or no issues left):
   a. Fetch fresh issues from SonarQube (most critical first)
   b. Skip non-coverable files (CSS/SCSS/HTML — no unit test coverage possible)
   c. Check line-level test coverage for affected code
   d. Generate unit tests if coverage < 100% (up to N retries)
   e. Clean → Fix via claude -d → Format → Build (with retry) → Test (with retry) → Lint (with auto-fix)
   f. If Claude modifies test files: auto-revert test changes, keep source fix if tests still pass
   g. Re-scan with SonarQube to verify the specific issue is resolved (up to N retries)
   h. Commit if verified, revert if not
6. Deduplication (unless --skip-dedup):
   a. Fetch files with duplicated code from SonarQube (most duplicated first)
   b. Ensure 100% test coverage of duplicated ranges
   c. Ask Claude to refactor and eliminate duplication
   d. Verify build + tests pass, no test files modified
   e. Re-scan with SonarQube to verify duplication is reduced
   f. Commit if verified, revert if not
7. Create PR (unless --no-pr)
8. Generate REPORT.md, TECHDEBT_CHANGELOG.md (committed on the fix branch)
```

## Configuration

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
  --min-coverage <PCT>             Minimum project-wide test coverage before fixing [default: 80]
  --min-file-coverage <PCT>        Minimum per-file test coverage [default: 0]
  --skip-coverage                  Skip the coverage boost step entirely
  --skip-format                    Skip the initial format-and-commit step
  --skip-dedup                     Skip the deduplication step after fixes
  --max-dedup <N>                  Max deduplication iterations (0 = unlimited) [default: 10]
  --coverage-attempts <N>          Test generation / fix retry attempts [default: 3]
  --no-pr                          Skip creating a pull request after fixes
  --dangerously-skip-permissions   Pass --dangerously-skip-permissions to Claude CLI
  --show-prompts                   Print prompts sent to Claude (for debugging)
  --log-format <text|json>         Log format [default: text]
  --test-timeout <SECS>            Per-test-run timeout [default: 600]
  --claude-timeout <SECS>          Per Claude call timeout [default: 300]
  --timeout <SECS>                 Global timeout (0 = none) [default: 0]
  --skip-scan                      Skip sonar-scanner, use existing analysis
  --scanner-path <PATH>            Scanner binary path (auto-detected)
  --config <PATH>                  YAML config file path
  --resume                         Resume a previously interrupted execution
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
| `REPARO_MAX_DEDUP` | `--max-dedup` |

### YAML Configuration (`reparo.yaml`)

Place a `reparo.yaml` (or custom named file via `--config`) in your project root for versioned, repeatable configuration. Supports `${ENV_VAR}` interpolation.

**Priority**: CLI flags > Environment variables > YAML > defaults

```yaml
sonar:
  project_id: "com.example:my-project"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

git:
  branch: "develop"
  batch_size: 5

execution:
  max_issues: 20
  min_coverage: 80          # project-wide % test coverage before fixing (0 = disabled)
  min_file_coverage: 50     # per-file % — boost files individually below this (0 = disabled)
  format_on_start: true     # run formatter and commit before fixes (default: true)
  dedup_on_completion: true  # refactor duplicated code after fixes (default: true)
  max_dedup: 10             # max dedup iterations (0 = unlimited, default: 10)
  coverage_attempts: 3      # test gen / fix retry attempts per issue (default: 3)
  timeout: 3600             # global timeout in seconds
  test_timeout: 600         # per-test-run timeout
  claude_timeout: 600       # per Claude call timeout

# Project commands — executed directly, no LLM involved
commands:
  setup: "npm install"                        # run once before pre-flight (e.g., install deps)
  clean: "npm run build -- --delete-output-path"
  build: "npm run build"
  test: "npm test"
  coverage: "npm run test:coverage"
  format: "npx prettier --write ."
  lint: "npx eslint src --max-warnings=0"
```

**Command execution order** after each fix:

1. `clean` (if defined) — clean artifacts before each fix
2. `format` (if defined) — format code, changes included in commit
3. `build` (if defined) — compile, **retries with Claude on failure** (up to `coverage_attempts` times)
4. `test` (required) — run tests, **retries with Claude on failure** (up to `coverage_attempts` times)
5. `lint` (if defined) — static analysis, **auto-fixed by Claude** (up to `coverage_attempts` times)

#### Example: Angular/TypeScript project

```yaml
sonar:
  project_id: "my-angular-app"
  url: "${SONAR_URL}"
  token: "${SONAR_TOKEN}"

execution:
  max_issues: 10
  min_coverage: 60
  claude_timeout: 600

commands:
  setup: "npm install"
  clean: "npm run build -- --delete-output-path"
  build: "npm run build"
  test: "npm test"
  coverage: "npm run test:coverage"
  format: "npx prettier --write ."
  lint: "npx eslint src --max-warnings=0"
```

#### Example: Java/Maven project

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

#### Example: Node.js project

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

#### Example: Python project

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
   - Asks Claude to generate unit tests
   - **Strictly enforces**: only test files may be created/modified. If source code is touched, all changes are reverted
   - Runs tests — if they fail, reverts and moves to the next file
   - Commits passing tests and re-measures coverage
5. Stops processing "overall boost" files once the project-wide threshold is reached, but continues boosting files below the per-file threshold

**Configuration:**

```bash
# Default: 80% project-wide, no per-file threshold
reparo --path ./my-project --config ./reparo.yaml

# Custom thresholds
reparo --path ./my-project --config ./reparo.yaml --min-coverage 60 --min-file-coverage 30

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
```

## Self-Healing Fixes

Reparo includes automatic retry logic to maximize fix success rate:

- **Build failures**: If the build fails after a fix, Claude is asked to fix the compilation error (without touching tests). Retried up to `coverage_attempts` times.
- **Test failures**: If tests fail after a fix, Claude is asked to fix the code to make tests pass (without modifying test files). Retried up to `coverage_attempts` times.
- **Lint errors**: If the linter reports errors after a fix, Claude is asked to fix them automatically. Verified with build+test after each attempt.
- **Test file modifications**: If Claude modifies test files during a fix, those changes are automatically reverted. If the source fix still passes tests without the test modifications, the fix is accepted.
- **SonarQube verification failures**: If SonarQube still reports the issue after the fix, the fix is retried up to `coverage_attempts` times with additional context about what didn't work.

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

# Debug: show prompts sent to Claude
reparo --path ./my-project --config ./reparo.yaml --show-prompts
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

1. **Coverage boost phase**: Only test files may be created. If Claude modifies any source file, all changes are reverted immediately.
2. **Fix phase**: If Claude modifies test files during a fix, those test changes are automatically reverted. If the source fix still passes tests, the fix is accepted. Otherwise it's flagged as **NeedsReview**.
3. After every fix, the full test suite runs. If tests fail, Claude is asked to fix the code (up to N retries). If all retries fail, the fix is **reverted**.
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
  main.rs           Entry point, timeout handling, exit codes
  config.rs         CLI parsing, validation, scanner detection
  yaml_config.rs    YAML config loading, env interpolation, merging
  orchestrator.rs   Main workflow loop: format → coverage boost → fix loop → dedup
  sonar.rs          SonarQube API client (issues, coverage, duplications, rules, scanner, edition)
  claude.rs         Claude CLI integration, prompt builders (fix, test gen, dedup, build/lint repair)
  git.rs            Git operations, PR creation via gh
  runner.rs         Test/build/lint/coverage execution, lcov parsing, framework detection
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
