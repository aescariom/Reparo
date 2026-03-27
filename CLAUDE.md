# CLAUDE.md — Reparo Project Guide

## What is this project?

Reparo is a Rust CLI tool that automates SonarQube technical debt fixing using `claude -d`. It scans a project for issues, prioritizes them by severity, ensures test coverage, fixes each issue, validates tests still pass, and creates pull requests.

## Build & Test

```bash
cargo build              # dev build
cargo build --release    # release build
cargo test               # run all 139 unit tests
```

Tests create temporary git repos via `tempfile` — they are self-contained and need no external services.

## Project Structure

```
src/
  main.rs           Entry point, logging, timeout, exit codes
  config.rs         CLI args (clap), validation, scanner detection
  yaml_config.rs    reparo.yaml loading, env interpolation, merging
  orchestrator.rs   Main workflow loop — the heart of the system
  sonar.rs          SonarQube API client (issues, coverage, rules, scanner)
  claude.rs         Claude CLI invocation with per-call timeout, prompt builders
  git.rs            Git operations (branch, commit, push, PR via gh)
  runner.rs         Test/build/lint execution, framework detection
  report.rs         REPORT.md, TECHDEBT_CHANGELOG.md, REVIEW_NEEDED.md
  retry.rs          Retry with exponential backoff for network/CLI calls
  state.rs          Execution state persistence for --resume support

US/                 User stories (US-001 through US-014)
test-project/       Small Python project with intentional SonarQube issues
```

## Architecture

The flow is sequential and single-threaded:

```
main.rs → Config::validate() → Orchestrator::run()
  Step 1: check_connection() [sonar.rs]
  Step 2: run_scanner() + wait_for_analysis() [sonar.rs]
  Step 3: fetch_issues() sorted by severity [sonar.rs]
  Step 4: for each batch of issues:
    create_branch [git.rs]
    for each issue:
      check_coverage [sonar.rs → CoverageResult]
      generate_tests_with_retry [claude.rs + runner.rs] (max 3 attempts)
      clean [runner.rs, if commands.clean set]
      run_claude fix [claude.rs]
      format [runner.rs, if commands.format set]
      build [runner.rs, if commands.build set — blocking]
      run tests [runner.rs — blocking]
      lint [runner.rs, if commands.lint set — non-blocking]
      commit [git.rs]
    create_batch_pr [git.rs → gh pr create]
    append_changelog [report.rs]
    checkout base branch [git.rs]
  Step 5: generate_report() [report.rs]
```

## Key Design Decisions

- **Tests must never be modified during a fix.** If Claude modifies test files, the fix is reverted immediately. If tests fail after a fix, the fix is reverted and logged in REVIEW_NEEDED.md.
- **Config priority**: CLI flags > ENV vars > YAML file > defaults.
- **Idempotency**: If a branch `fix/sonar-<key>` already exists, the issue is skipped.
- **Batch mode**: `--batch-size N` groups N fixes into one PR. `--batch-size 0` = all in one PR.
- **Commands in YAML** (`commands.build`, `commands.test`, etc.) are executed directly via `sh -c`, never through an LLM.

## Code Conventions

- Error handling: `anyhow::Result` everywhere, `bail!` for user-facing errors.
- Logging: `tracing` crate (`info!`, `warn!`, `error!`). JSON output via `--log-format json`.
- Tests: inline `#[cfg(test)] mod tests` in each module. Use `tempfile::tempdir()` for filesystem tests.
- No `unwrap()` on fallible operations in non-test code.
- String truncation must use `.chars().take(N)` — never byte-based slicing (UTF-8 safety).

## Module Responsibilities

| Module | Owns | Does NOT own |
|--------|------|-------------|
| `config.rs` | CLI parsing, validation, scanner detection | YAML parsing (that's `yaml_config.rs`) |
| `yaml_config.rs` | YAML loading, env interpolation, command resolution | Validation of non-YAML params |
| `orchestrator.rs` | Workflow orchestration, batch loop, process_issue | API calls, git commands, file I/O |
| `sonar.rs` | All SonarQube API interaction | Git, Claude, test execution |
| `claude.rs` | Running `claude` CLI, building prompts | Deciding what to fix or when to retry |
| `git.rs` | All git/gh commands | Deciding when to branch or commit |
| `runner.rs` | Executing shell commands (test, build, lint, format) | Deciding which commands to run |
| `report.rs` | Writing REPORT.md, CHANGELOG, REVIEW_NEEDED | Deciding what status an issue gets |

## Adding a New Feature

1. Check if there's a relevant US in `US/`. If not, create one.
2. Identify which module(s) are affected.
3. Add the implementation with tests in the same module.
4. Run `cargo test` — all 117+ tests must pass.
5. Run `cargo build` — no errors, only the `dead_code` warning for `ValidatedConfig` fields is acceptable.

## Common Tasks

**Add a new CLI flag**: Edit `Config` struct in `config.rs` (clap derive), add to `ValidatedConfig`, wire in `validate()`.

**Add a new project command**: Add field to `CommandsYaml` and `ProjectCommands` in `yaml_config.rs`, integrate in `orchestrator.rs::process_issue()`.

**Support a new test runner**: Add detection in `runner.rs::detect_test_command()` and `detect_coverage_command()`.

**Support a new scanner**: Add variant to `ScannerKind` in `config.rs`, add detection in `resolve_scanner()`, add execution branch in `sonar.rs::run_scanner()`.

**Change prompt strategy**: Edit `claude.rs::build_fix_prompt()` or `build_test_generation_prompt()`.

## Step Enable/Disable Reference

Every optional step can be enabled or disabled via CLI flags and/or YAML configuration. Default: all disabled except core workflow steps which default to enabled.

| Step | CLI flag | YAML field | Default | Notes |
|------|----------|------------|---------|-------|
| Initial formatting | `--skip-format` | `execution.format_on_start: false` | enabled | Format & commit before fixes |
| Coverage boost | `--skip-coverage` | `execution.coverage_boost: false` | enabled | Generate tests until min_coverage |
| Contract/pact testing | `--skip-pact` | `pact.enabled: true` | disabled | Must enable via YAML `pact.enabled` |
| Deduplication | `--skip-dedup` | `execution.dedup_on_completion: false` | enabled | Remove duplicate fixes |
| Final validation (tests) | `--skip-final-validation` | `execution.final_validation: false` | enabled | Run full suite after all fixes |
| Documentation quality | `--skip-docs` | `documentation.enabled: true` | disabled | Must enable via YAML |
| PR creation | `--no-pr` | — | enabled | Create PR via `gh` |

**Priority**: CLI flags > ENV vars > YAML > defaults.

**Pact sub-steps** (all default `false`, set in YAML `pact:` section):
- `check_contracts` — check if file is API-related
- `generate_tests` — generate contract tests
- `verify_before_fix` — verify contracts before applying fix
- `verify_after_fix` — verify contracts after applying fix

**YAML example** (all steps explicit):
```yaml
execution:
  format_on_start: true
  coverage_boost: true
  coverage_attempts: 3
  coverage_rounds: 3
  coverage_exclude: []
  final_validation: true
  final_validation_attempts: 5
  dedup_on_completion: true
  max_dedup: 10

pact:
  enabled: false
  pact_dir: "./pacts"
  check_contracts: false
  generate_tests: false
  verify_before_fix: false
  verify_after_fix: false

documentation:
  enabled: false
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All issues fixed, none found, or dry-run |
| 1 | Configuration or connectivity error |
| 2 | Partial success |
| 3 | Unexpected error |

## Dependencies

Core: `clap` (CLI), `reqwest` (HTTP), `serde`/`serde_json`/`serde_yaml` (serialization), `tokio` (async), `anyhow` (errors), `tracing` (logging), `chrono` (timestamps), `which` (binary detection), `glob` (file patterns), `regex` (env interpolation).

Dev only: `tempfile` (test fixtures).
