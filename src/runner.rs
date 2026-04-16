use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tracing::{info, warn};

/// Fallback timeout for `run_shell_command`, which has no explicit timeout
/// parameter. Chosen to comfortably exceed realistic build/format/coverage
/// runs while still killing genuinely stuck processes (e.g., a deadlocked
/// JVM, an `npm install` waiting on stdin).
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 3600;

/// Run `sh -c <command>` with a hard timeout, draining stdout/stderr in
/// background threads so the child never blocks on a full pipe buffer and
/// descendant processes that inherit the pipe fds can't deadlock the parent.
///
/// On timeout the child is killed and we return (false, "<timeout>\n<partial>").
/// A `timeout_secs == 0` disables the timeout (used for tests that want the
/// default `Command::output()` semantics).
fn run_with_timeout(
    project_path: &Path,
    command: &str,
    label: &str,
    timeout_secs: u64,
) -> Result<(bool, String)> {
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    let mut child = Command::new("sh")
        .current_dir(project_path)
        .args(["-c", command])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn {} command: {}", label, command))?;

    fn drain(pipe: Option<impl Read + Send + 'static>) -> Option<mpsc::Receiver<Vec<u8>>> {
        let mut pipe = pipe?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        Some(rx)
    }

    let stdout_rx = drain(child.stdout.take());
    let stderr_rx = drain(child.stderr.take());

    let started = Instant::now();
    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(started + Duration::from_secs(timeout_secs))
    };

    // Heartbeat: emit an info log every N seconds so long-running subprocesses
    // (mvn test, npm install, claude) don't look like hangs. Users watching
    // the tail of the log can then tell "still running" apart from "stuck".
    const HEARTBEAT_SECS: u64 = 60;
    let mut next_heartbeat = started + Duration::from_secs(HEARTBEAT_SECS);

    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                let now = Instant::now();
                if now >= next_heartbeat {
                    info!(
                        "{} command still running after {}s: {}",
                        label,
                        started.elapsed().as_secs(),
                        command
                    );
                    next_heartbeat = now + Duration::from_secs(HEARTBEAT_SECS);
                }
                if let Some(d) = deadline {
                    if now >= d {
                        warn!(
                            "{} command exceeded {}s timeout — killing process: {}",
                            label, timeout_secs, command
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to wait on {} command", label));
            }
        }
    };

    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| -> String {
        rx.and_then(|r| r.recv_timeout(Duration::from_secs(2)).ok())
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default()
    };
    let stdout = collect(stdout_rx);
    let stderr = collect(stderr_rx);
    let combined = format!("{}\n{}", stdout, stderr);

    match status {
        Some(s) => Ok((s.success(), combined)),
        None => Ok((
            false,
            format!("[reparo: killed after {}s timeout]\n{}", timeout_secs, combined),
        )),
    }
}

/// Detect the test command for a project based on build files present
pub fn detect_test_command(project_path: &Path) -> Option<String> {
    let checks = [
        ("pom.xml", "mvn test"),
        ("build.gradle", "gradle test"),
        ("build.gradle.kts", "gradle test"),
        ("package.json", "npm test"),
        ("Cargo.toml", "cargo test"),
        ("setup.py", "python -m pytest"),
        ("pyproject.toml", "python -m pytest"),
        ("requirements.txt", "python -m pytest"),
        ("Gemfile", "bundle exec rspec"),
        ("go.mod", "go test ./..."),
        ("Makefile", "make test"),
    ];

    for (file, cmd) in &checks {
        if project_path.join(file).exists() {
            info!("Detected test command: {} (found {})", cmd, file);
            return Some(cmd.to_string());
        }
    }

    None
}

/// Detect the coverage command for a project
pub fn detect_coverage_command(project_path: &Path) -> Option<String> {
    // Check for Angular projects — determine if using Jest or Karma
    if project_path.join("angular.json").exists() {
        // Jest-based Angular projects have jest.config.js/ts
        if project_path.join("jest.config.js").exists()
            || project_path.join("jest.config.ts").exists()
            || project_path.join("jest.config.mjs").exists()
        {
            return Some("npx jest --coverage".to_string());
        }
        // Karma-based Angular projects (default ng test)
        return Some("npm test -- --code-coverage --no-watch".to_string());
    }

    let checks = [
        ("pom.xml", "mvn verify -Pcoverage"),
        ("build.gradle", "gradle jacocoTestReport"),
        ("build.gradle.kts", "gradle jacocoTestReport"),
        ("package.json", "npx jest --coverage"),
        ("Cargo.toml", "cargo tarpaulin --out Xml"),
        ("setup.py", "python -m pytest --cov --cov-report=xml"),
        ("pyproject.toml", "python -m pytest --cov --cov-report=xml"),
        ("go.mod", "go test -coverprofile=coverage.out ./..."),
    ];

    for (file, cmd) in &checks {
        if project_path.join(file).exists() {
            return Some(cmd.to_string());
        }
    }

    None
}

/// Run the test suite and return (success, output).
///
/// `timeout_secs` hard-kills the process if it hasn't exited within the window.
/// Pass 0 to disable (tests only).
pub fn run_tests(project_path: &Path, test_command: &str, timeout_secs: u64) -> Result<(bool, String)> {
    info!("Running tests: {}", test_command);

    if test_command.is_empty() {
        anyhow::bail!("Empty test command");
    }

    run_with_timeout(project_path, test_command, "test", timeout_secs)
}

/// Result of running targeted tests (a subset matched by a filter).
#[derive(Debug)]
pub struct TargetedTestResult {
    pub success: bool,
    pub output: String,
    /// True when the test runner actually executed at least one test.
    /// When false, the caller must fall back to the full test suite because
    /// "0 matches" is not the same as "0 failures".
    pub ran_any: bool,
}

/// Run a targeted subset of the test suite, limited to tests matching
/// `filter`. Currently supports Maven Surefire (`-Dtest=<filter>`); for
/// other runners, returns Ok(ran_any=false) so the caller falls back to
/// the full suite. The filter already encodes AST-derived class patterns
/// (see `derive_surefire_filter`).
pub fn run_targeted_tests(
    project_path: &Path,
    test_command: &str,
    filter: &str,
) -> Result<TargetedTestResult> {
    if test_command.is_empty() || filter.is_empty() {
        anyhow::bail!("Empty test command or filter");
    }

    let lower = test_command.to_lowercase();
    let targeted_command = if lower.contains("mvn") || lower.contains("./mvnw") {
        inject_or_merge_surefire_filter(test_command, filter)
    } else {
        // Unsupported runner — signal fallback
        return Ok(TargetedTestResult {
            success: false,
            output: String::new(),
            ran_any: false,
        });
    };

    info!("Running targeted tests: {}", targeted_command);
    let (success, combined) = run_with_timeout(
        project_path,
        &targeted_command,
        "targeted test",
        DEFAULT_SHELL_TIMEOUT_SECS,
    )?;

    // Heuristic: Surefire emits "Tests run: 0" and exits 0 when the filter
    // matches nothing ("No tests were executed!"). Treat that as ran_any=false.
    let ran_any = !combined.contains("No tests were executed")
        && !combined.contains("Tests run: 0");

    Ok(TargetedTestResult {
        success,
        output: combined,
        ran_any,
    })
}

/// Inject or merge a Surefire `-Dtest=` filter into an existing Maven command.
/// Preserves existing negative filters (e.g. `-Dtest=!Excluded`) by ANDing:
/// `-Dtest=<new>,!Excluded`.
fn inject_or_merge_surefire_filter(test_command: &str, new_filter: &str) -> String {
    // If there's an existing -Dtest=, replace its positive part but keep negatives
    if let Some(idx) = test_command.find("-Dtest=") {
        let before = &test_command[..idx];
        let after_start = idx + "-Dtest=".len();
        let rest = &test_command[after_start..];
        // The value ends at the next whitespace (assumed unquoted in practice)
        let value_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let value = &rest[..value_end];
        let tail = &rest[value_end..];
        let preserved_negatives: Vec<&str> = value.split(',').filter(|s| s.starts_with('!')).collect();
        let mut merged = new_filter.to_string();
        for neg in preserved_negatives {
            merged.push(',');
            merged.push_str(neg);
        }
        return format!("{}-Dtest={}{}", before, merged, tail);
    }
    format!("{} -Dtest={}", test_command, new_filter)
}

/// Derive a Surefire `-Dtest=` filter value from a list of changed source files.
///
/// For each Java source like `src/main/java/.../Foo.java` produces the patterns
/// `FooTest,Foo*Test,*FooTest` so a variety of test-naming conventions match.
/// Returns `None` when no supported source files are present (caller should
/// fall back to the full suite).
pub fn derive_surefire_filter(changed_files: &[String]) -> Option<String> {
    let mut patterns: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in changed_files {
        if !(path.ends_with(".java") || path.ends_with(".kt")) {
            continue;
        }
        let file_name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str())?;
        let lower = path.to_lowercase();
        let is_test_file = lower.contains("/test/") || lower.contains("/tests/");
        if is_test_file {
            // Issue is in the test file itself — target that class directly
            // so we don't fall back to running the entire suite. This is the
            // common case for rules like java:S5810, java:S1130, java:S1128
            // on test code.
            if seen.insert(file_name.to_string()) {
                patterns.push(file_name.to_string());
            }
        } else {
            for pat in [
                file_name.to_string(),
                format!("{}Test", file_name),
                format!("{}*Test", file_name),
                format!("*{}Test", file_name),
                format!("{}Tests", file_name),
                format!("{}IT", file_name),
            ] {
                if seen.insert(pat.clone()) {
                    patterns.push(pat);
                }
            }
        }
    }
    if patterns.is_empty() {
        None
    } else {
        Some(patterns.join(","))
    }
}

/// US-059: Detect test failures in output of a combined test+coverage command.
///
/// Returns `Some(summary)` when a failure pattern is found in the output of a
/// command that runs tests as a side effect (Maven Surefire, Gradle, pytest,
/// Jest/Vitest, go test). Returns `None` when no failure signature is detected.
///
/// The caller should treat `None` + non-zero exit code as a test failure too;
/// this function only flags the cases where the output *explicitly* names
/// failing tests, so we can short-circuit without running a separate `test` command.
pub fn detect_test_failures_in_output(output: &str) -> Option<String> {
    // Maven Surefire: "Tests run: X, Failures: Y, Errors: Z" where Y+Z > 0
    // or "BUILD FAILURE" followed by failing test listing
    if let Some(caps) = regex::Regex::new(
        r"Tests run:\s*\d+,\s*Failures:\s*(\d+),\s*Errors:\s*(\d+)"
    )
    .ok()
    .and_then(|re| re.captures(output))
    {
        let failures: u32 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        let errors: u32 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        if failures > 0 || errors > 0 {
            return Some(format!("Maven: {} failures, {} errors", failures, errors));
        }
    }

    // Gradle: "X tests completed, Y failed"
    if let Some(caps) = regex::Regex::new(r"(\d+)\s+tests?\s+completed,\s*(\d+)\s+failed")
        .ok()
        .and_then(|re| re.captures(output))
    {
        let failed: u32 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        if failed > 0 {
            return Some(format!("Gradle: {} failed", failed));
        }
    }

    // pytest: "===== X failed, Y passed in Zs ====="
    if let Some(caps) = regex::Regex::new(r"={3,}\s*(\d+)\s+failed(?:,\s*\d+\s+passed)?")
        .ok()
        .and_then(|re| re.captures(output))
    {
        let failed: u32 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        if failed > 0 {
            return Some(format!("pytest: {} failed", failed));
        }
    }

    // Jest / Vitest: "Tests: X failed, Y passed" or "FAIL <file>"
    if let Some(caps) = regex::Regex::new(r"Tests:\s*(\d+)\s+failed")
        .ok()
        .and_then(|re| re.captures(output))
    {
        let failed: u32 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        if failed > 0 {
            return Some(format!("Jest/Vitest: {} failed", failed));
        }
    }

    // go test: "FAIL\t<package>"
    if regex::Regex::new(r"^FAIL\s+\S+").ok().is_some_and(|re| re.is_match(output))
        || output.contains("--- FAIL:")
    {
        return Some("go test: FAIL".to_string());
    }

    // Generic "BUILD FAILURE" marker from Maven: covers compilation errors too,
    // but we emit a more specific reason when available
    if output.contains("BUILD FAILURE") {
        return Some("Maven BUILD FAILURE".to_string());
    }

    None
}

/// Run tests with coverage reporting.
///
/// `timeout_secs` hard-kills the process if it hasn't exited within the window.
/// Pass 0 to disable (tests only).
pub fn run_coverage(project_path: &Path, coverage_command: &str, timeout_secs: u64) -> Result<(bool, String)> {
    info!("Running coverage: {}", coverage_command);

    if coverage_command.is_empty() {
        anyhow::bail!("Empty coverage command");
    }

    run_with_timeout(project_path, coverage_command, "coverage", timeout_secs)
}

/// Run an arbitrary shell command in the project directory (US-014).
/// Uses `sh -c` to support pipes and redirections.
/// Returns (success, combined stdout+stderr).
///
/// Enforces `DEFAULT_SHELL_TIMEOUT_SECS` so a wedged build/format/coverage
/// command (deadlocked JVM, process waiting on stdin, hung test runner)
/// can't stall the entire workflow.
pub fn run_shell_command(project_path: &Path, command: &str, label: &str) -> Result<(bool, String)> {
    info!("Running {}: {}", label, command);

    let (success, combined) =
        run_with_timeout(project_path, command, label, DEFAULT_SHELL_TIMEOUT_SECS)?;

    if success {
        info!("{} succeeded", label);
    } else {
        tracing::warn!("{} failed", label);
    }

    Ok((success, combined))
}

/// Find existing test files in the project to use as examples
pub fn find_test_examples(project_path: &Path) -> Vec<String> {
    let mut examples = Vec::new();

    // Common test file patterns
    let patterns = [
        "**/*Test.java",
        "**/*_test.go",
        "**/*_test.py",
        "**/test_*.py",
        "**/*.test.ts",
        "**/*.test.js",
        "**/*.spec.ts",
        "**/*.spec.js",
        "**/tests/*.rs",
    ];

    for pattern in &patterns {
        if let Ok(paths) = glob::glob(&format!("{}/{}", project_path.display(), pattern)) {
            for entry in paths.flatten() {
                if let Ok(content) = std::fs::read_to_string(&entry) {
                    // Only take first ~20 lines as example — enough to convey style/patterns
                    let snippet: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
                    let rel_path = entry.strip_prefix(project_path).unwrap_or(&entry);
                    examples.push(format!("// File: {}\n{}", rel_path.display(), snippet));
                    if examples.len() >= 2 {
                        return examples;
                    }
                }
            }
        }
    }

    examples
}

/// Find existing test files in the project, limited to `max_files` examples with `max_lines` per file (US-054).
///
/// Use instead of `find_test_examples` when framework context is already present and fewer
/// examples suffice to convey project style.
pub fn find_test_examples_limited(project_path: &Path, max_files: usize, max_lines: usize) -> Vec<String> {
    let patterns = [
        "**/*Test.java",
        "**/*_test.go",
        "**/*_test.py",
        "**/test_*.py",
        "**/*.test.ts",
        "**/*.test.js",
        "**/*.spec.ts",
        "**/*.spec.js",
        "**/tests/*.rs",
    ];

    let mut examples = Vec::new();
    for pattern in &patterns {
        if let Ok(paths) = glob::glob(&format!("{}/{}", project_path.display(), pattern)) {
            for entry in paths.flatten() {
                if let Ok(content) = std::fs::read_to_string(&entry) {
                    let snippet: String = content.lines().take(max_lines).collect::<Vec<_>>().join("\n");
                    let rel_path = entry.strip_prefix(project_path).unwrap_or(&entry);
                    examples.push(format!("// File: {}\n{}", rel_path.display(), snippet));
                    if examples.len() >= max_files {
                        return examples;
                    }
                }
            }
        }
    }
    examples
}

/// Detect test dependencies from build files (pom.xml / build.gradle) for framework-aware prompts (US-040).
///
/// Returns a human-readable description of the detected test stack,
/// e.g. "JUnit 5, Mockito, AssertJ". Returns empty string if nothing detected.
pub fn detect_test_dependencies(project_path: &Path) -> String {
    let mut deps: Vec<&str> = Vec::new();

    // Try pom.xml first
    if let Ok(content) = std::fs::read_to_string(project_path.join("pom.xml")) {
        let content_lower = content.to_lowercase();
        // JUnit version detection
        if content_lower.contains("junit-jupiter") || content_lower.contains("junit-bom") {
            deps.push("JUnit 5 (junit-jupiter)");
        } else if content_lower.contains("junit</artifactid") || content_lower.contains("junit:junit") {
            deps.push("JUnit 4");
        }
        // Mockito
        if content_lower.contains("mockito-core") || content_lower.contains("mockito-junit-jupiter") {
            if content_lower.contains("mockito-junit-jupiter") {
                deps.push("Mockito with mockito-junit-jupiter (@ExtendWith(MockitoExtension.class))");
            } else {
                deps.push("Mockito");
            }
        }
        // Spring
        if content_lower.contains("spring-boot-starter-test") {
            deps.push("Spring Boot Test (spring-boot-starter-test)");
        } else if content_lower.contains("spring-test") {
            deps.push("Spring Test");
        }
        // Assertion libraries
        if content_lower.contains("assertj-core") {
            deps.push("AssertJ (assertThat style assertions)");
        }
        if content_lower.contains("hamcrest") {
            deps.push("Hamcrest");
        }
        return deps.join(", ");
    }

    // Try build.gradle / build.gradle.kts
    for gradle_file in &["build.gradle", "build.gradle.kts"] {
        if let Ok(content) = std::fs::read_to_string(project_path.join(gradle_file)) {
            let content_lower = content.to_lowercase();
            if content_lower.contains("junit-jupiter") || content_lower.contains("junit-bom") {
                deps.push("JUnit 5 (junit-jupiter)");
            } else if content_lower.contains("junit:junit") {
                deps.push("JUnit 4");
            }
            if content_lower.contains("mockito") {
                deps.push("Mockito");
            }
            if content_lower.contains("spring-boot-starter-test") {
                deps.push("Spring Boot Test");
            }
            if content_lower.contains("assertj") {
                deps.push("AssertJ");
            }
            return deps.join(", ");
        }
    }

    // NestJS — detected before Angular (also uses angular.json-like config in some setups)
    if project_path.join("nest-cli.json").exists() {
        deps.push("NestJS (Jest)");
        if let Ok(pkg) = std::fs::read_to_string(project_path.join("package.json")) {
            if pkg.contains("supertest") { deps.push("Supertest"); }
        }
        return deps.join(", ");
    }

    // Angular projects — detected before generic package.json to give richer context
    if project_path.join("angular.json").exists() {
        let is_jest = is_jest_project(project_path);
        deps.push(if is_jest { "Angular (Jest)" } else { "Angular (Karma/Jasmine)" });
        if let Ok(pkg) = std::fs::read_to_string(project_path.join("package.json")) {
            if pkg.contains("@testing-library/angular") { deps.push("Angular Testing Library"); }
            if pkg.contains("ng-mocks") { deps.push("ng-mocks"); }
        }
        return deps.join(", ");
    }

    // PHP — Laravel or Symfony
    if let Ok(composer) = std::fs::read_to_string(project_path.join("composer.json")) {
        if composer.contains("laravel/framework") {
            deps.push("Laravel (PHPUnit)");
            if composer.contains("mockery/mockery") { deps.push("Mockery"); }
            if composer.contains("fakerphp/faker") || composer.contains("fzaninotto/faker") {
                deps.push("Faker");
            }
            return deps.join(", ");
        }
        if composer.contains("symfony/framework-bundle") {
            deps.push("Symfony (PHPUnit)");
            return deps.join(", ");
        }
        // Generic PHP project
        if composer.contains("phpunit/phpunit") {
            deps.push("PHPUnit");
            return deps.join(", ");
        }
    }

    // Ruby — Gemfile
    if let Ok(gemfile) = std::fs::read_to_string(project_path.join("Gemfile")) {
        let gl = gemfile.to_lowercase();
        if gl.contains("rspec-rails") || gl.contains("rspec-core") {
            deps.push("RSpec");
            if gl.contains("factory_bot") { deps.push("FactoryBot"); }
            if gl.contains("shoulda-matchers") { deps.push("Shoulda Matchers"); }
            if gl.contains("capybara") { deps.push("Capybara"); }
        } else if gl.contains("minitest") {
            deps.push("Minitest");
        }
        return deps.join(", ");
    }

    // JS/TS projects — package.json with framework-aware detection
    if let Ok(content) = std::fs::read_to_string(project_path.join("package.json")) {
        // Next.js — check before generic React
        if is_nextjs_project(project_path) {
            deps.push("Next.js");
            if content.contains("@testing-library/react") { deps.push("React Testing Library"); }
            if content.contains("\"jest\"") || content.contains("jest-environment") {
                deps.push("Jest");
            }
            return deps.join(", ");
        }
        // Vue
        if content.contains("\"vue\"") || content.contains("\"@vue/core\"") {
            if content.contains("vitest") {
                deps.push("Vue (Vitest)");
            } else {
                deps.push("Vue (Jest)");
            }
            if content.contains("@vue/test-utils") { deps.push("Vue Test Utils"); }
            return deps.join(", ");
        }
        // React (generic SPA/library)
        if content.contains("\"react\"") {
            if content.contains("vitest") { deps.push("React (Vitest)"); }
            else { deps.push("React (Jest)"); }
            if content.contains("@testing-library/react") { deps.push("React Testing Library"); }
            if content.contains("@testing-library/user-event") { deps.push("userEvent"); }
            if content.contains("msw") { deps.push("MSW"); }
            return deps.join(", ");
        }
        // Generic JS/TS
        let content_lower = content.to_lowercase();
        if content_lower.contains("jest") { deps.push("Jest"); }
        if content_lower.contains("mocha") { deps.push("Mocha"); }
        if content_lower.contains("vitest") { deps.push("Vitest"); }
        if content_lower.contains("sinon") { deps.push("Sinon"); }
        if content_lower.contains("chai") { deps.push("Chai"); }
        return deps.join(", ");
    }

    String::new()
}

// ── Project-type detection helpers ──────────────────────────────────────────

fn is_jest_project(project_path: &Path) -> bool {
    project_path.join("jest.config.js").exists()
        || project_path.join("jest.config.ts").exists()
        || project_path.join("jest.config.mjs").exists()
}

fn is_nextjs_project(project_path: &Path) -> bool {
    project_path.join("next.config.js").exists()
        || project_path.join("next.config.ts").exists()
        || project_path.join("next.config.mjs").exists()
}

fn is_laravel_project(project_path: &Path) -> bool {
    std::fs::read_to_string(project_path.join("composer.json"))
        .map(|c| c.contains("laravel/framework"))
        .unwrap_or(false)
}

fn is_symfony_project(project_path: &Path) -> bool {
    std::fs::read_to_string(project_path.join("composer.json"))
        .map(|c| c.contains("symfony/framework-bundle"))
        .unwrap_or(false)
}

/// Classify an Angular TypeScript source file and return testing guidance.
///
/// Detects the Angular decorator (@Component, @Injectable, @Pipe, @Directive, Guard)
/// and returns specific instructions that prevent the most common TestBed failures.
/// Returns a non-empty string only for Angular projects (angular.json present).
fn classify_angular_file(file_path: &str, content: &str, is_jest: bool) -> String {
    // Skip spec/test files — no guidance needed for those
    if file_path.ends_with(".spec.ts") || file_path.ends_with(".test.ts") {
        return String::new();
    }

    let has_component  = content.contains("@Component(") || content.contains("@Component ({");
    let has_injectable = content.contains("@Injectable(") || content.contains("@Injectable({");
    let has_pipe       = content.contains("@Pipe(") || content.contains("@Pipe({");
    let has_directive  = content.contains("@Directive(") || content.contains("@Directive({");
    let has_guard      = content.contains("CanActivate") || content.contains("CanDeactivate")
                      || content.contains("CanLoad") || content.contains("CanMatch");
    let has_resolver   = content.contains("implements Resolve") || content.contains("Resolve<");
    let has_http       = content.contains("HttpClient");
    let has_router     = content.contains("Router") || content.contains("ActivatedRoute");
    let has_forms      = content.contains("FormBuilder") || content.contains("ReactiveFormsModule")
                      || content.contains("FormsModule");

    if has_component {
        let mut lines = vec![
            "Angular @Component. Use TestBed.configureTestingModule({ declarations: [ComponentUnderTest], imports: [...], providers: [...] }).compileComponents() in beforeEach.".to_string(),
            "Call fixture.detectChanges() after setup to trigger initial change detection — tests that skip this cover 0 branches.".to_string(),
        ];
        if has_http {
            lines.push("Import HttpClientTestingModule; inject HttpTestingController to flush/verify HTTP requests.".to_string());
        }
        if has_router {
            lines.push("Import RouterTestingModule for Router/ActivatedRoute dependencies.".to_string());
        }
        if has_forms {
            lines.push("Import ReactiveFormsModule or FormsModule as needed.".to_string());
        }
        if is_jest {
            lines.push("Use async/await or fakeAsync/tick for async operations.".to_string());
        } else {
            lines.push("Use fakeAsync/tick for timers and synchronous-style async; waitForAsync for promise-based setup.".to_string());
        }
        return lines.join(" ");
    }

    if has_pipe {
        return "Angular @Pipe. Instantiate directly: const pipe = new MyPipe(); call pipe.transform(input, ...args). No TestBed needed unless the pipe has @Inject dependencies.".to_string();
    }

    if has_directive {
        return "Angular @Directive. Declare a minimal host component in the test file: @Component({ template: '<div appDirective></div>' }) class HostComponent {}. Use TestBed with declarations: [HostComponent, DirectiveUnderTest].".to_string();
    }

    if has_guard || has_resolver {
        return "Angular Guard/Resolver. Instantiate with mocked dependencies; mock ActivatedRouteSnapshot and RouterStateSnapshot as plain objects with the fields your guard reads. Test the return value (boolean/UrlTree/Observable).".to_string();
    }

    if has_injectable {
        if has_http {
            return "Angular @Injectable service with HttpClient. Use TestBed.configureTestingModule({ imports: [HttpClientTestingModule], providers: [ServiceUnderTest] }); inject ServiceUnderTest and HttpTestingController. Call httpMock.expectOne(url).flush(data) to simulate responses.".to_string();
        }
        return "Angular @Injectable service. Prefer instantiating directly: new MyService(mockDep1, mockDep2). Use jest.fn() or jasmine.createSpy() for dependencies. No TestBed needed unless the service uses Angular lifecycle hooks.".to_string();
    }

    // Plain TypeScript utility/model in an Angular project
    "Plain TypeScript class in Angular project — no Angular testing infrastructure needed. Instantiate with new and test methods directly.".to_string()
}

fn classify_nestjs_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with(".spec.ts") || file_path.ends_with(".test.ts") {
        return String::new();
    }
    let has_controller = content.contains("@Controller(") || content.contains("@Controller({");
    let has_injectable = content.contains("@Injectable(") || content.contains("@Injectable({");
    let has_module = content.contains("@Module(") || content.contains("@Module({");
    let has_http = content.contains("HttpService") || content.contains("HttpModule");

    if has_controller {
        return "NestJS @Controller. Use Test.createTestingModule({ controllers: [ControllerUnderTest], providers: [{ provide: ServiceDep, useValue: mockService }] }).compile(); get the instance via moduleRef.get(ControllerUnderTest). Use supertest for HTTP-level tests.".to_string();
    }
    if has_injectable {
        if has_http {
            return "NestJS @Injectable with HttpService. Provide { provide: HttpService, useValue: { get: jest.fn(), post: jest.fn() } } in the testing module.".to_string();
        }
        return "NestJS @Injectable service. Use Test.createTestingModule({ providers: [ServiceUnderTest, { provide: Dep, useValue: mockDep }] }).compile(); get the instance via moduleRef.get(ServiceUnderTest). Never call new ServiceUnderTest() directly — NestJS DI must wire dependencies.".to_string();
    }
    if has_module {
        return "NestJS @Module. Test that the module compiles: await Test.createTestingModule({ imports: [ModuleUnderTest] }).compile() should not throw.".to_string();
    }
    // Plain TS utility inside a NestJS project
    "Plain TypeScript class in NestJS project. Instantiate with new and test methods directly — no testing module needed.".to_string()
}

fn classify_nextjs_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with(".spec.ts") || file_path.ends_with(".test.ts")
        || file_path.ends_with(".spec.tsx") || file_path.ends_with(".test.tsx")
    {
        return String::new();
    }
    // App Router API route: app/**/route.ts
    let is_app_api = (file_path.contains("/app/") || file_path.starts_with("app/"))
        && file_path.ends_with("route.ts");
    // Pages Router API route: pages/api/**
    let is_pages_api = file_path.contains("pages/api/") || file_path.contains("pages\\api\\");
    // Server component (no 'use client' directive)
    let is_server_component = !content.contains("'use client'") && !content.contains("\"use client\"")
        && (file_path.contains("/app/") || file_path.starts_with("app/"));
    // App Router navigation vs Pages Router
    let uses_app_router = content.contains("next/navigation");
    let uses_pages_router = content.contains("next/router");

    if is_app_api {
        return "Next.js App Router API route. Import the handler function directly and call it with a mocked Request object: new Request('http://localhost/api/...', { method: 'GET' }). Assert on the Response object.".to_string();
    }
    if is_pages_api {
        return "Next.js Pages Router API route. Mock NextApiRequest as { method: 'GET', query: {}, body: {} } and NextApiResponse as { status: jest.fn().mockReturnThis(), json: jest.fn() }. Call the handler directly.".to_string();
    }
    if is_server_component {
        return "Next.js Server Component. Render with React's renderToString or @testing-library/react render. No router mocking required for server components.".to_string();
    }
    let mut lines = vec![
        "Next.js component. Use @testing-library/react: render(<Component />) and screen.getBy* queries.".to_string(),
    ];
    if uses_app_router {
        lines.push("Mock next/navigation: jest.mock('next/navigation', () => ({ useRouter: () => ({ push: jest.fn(), replace: jest.fn() }), usePathname: () => '/', useSearchParams: () => new URLSearchParams() })).".to_string());
    } else if uses_pages_router {
        lines.push("Mock next/router: jest.mock('next/router', () => ({ useRouter: () => ({ push: jest.fn(), pathname: '/', query: {}, asPath: '/' }) })).".to_string());
    }
    lines.join(" ")
}

fn classify_react_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with(".spec.tsx") || file_path.ends_with(".test.tsx")
        || file_path.ends_with(".spec.ts") || file_path.ends_with(".test.ts")
        || file_path.ends_with(".spec.jsx") || file_path.ends_with(".test.jsx")
    {
        return String::new();
    }
    let has_hooks = content.contains("useState") || content.contains("useEffect")
        || content.contains("useReducer") || content.contains("useContext");
    let has_router = content.contains("useNavigate") || content.contains("useLocation")
        || content.contains("BrowserRouter") || content.contains("MemoryRouter");

    let mut lines = vec![
        "React component. Use @testing-library/react: render(<Component />) and screen.getByRole/getByText/getByLabelText for queries. Prefer getByRole over getByTestId.".to_string(),
    ];
    if has_hooks {
        lines.push("Wrap state-updating interactions in act() or use userEvent from @testing-library/user-event which wraps in act() automatically.".to_string());
    }
    if has_router {
        lines.push("Wrap the component in MemoryRouter (react-router-dom) for route-dependent tests.".to_string());
    }
    lines.join(" ")
}

fn classify_vue_file(content: &str) -> String {
    let is_composition = content.contains("setup()") || content.contains("<script setup");
    let has_provide_inject = content.contains("provide(") || content.contains("inject(")
        || content.contains("provide:") || content.contains("inject:");
    let has_http = content.contains("axios") || content.contains("fetch(");

    let strategy = if is_composition {
        "Vue Composition API component."
    } else {
        "Vue Options API component."
    };
    let mut lines = vec![
        format!("{} Use @vue/test-utils: mount(Component, {{ props: {{...}} }}) for integration tests, shallowMount for unit tests (stubs child components).", strategy),
    ];
    if has_provide_inject {
        lines.push("Provide injected values: mount(Component, { global: { provide: { key: mockValue } } }).".to_string());
    }
    if has_http {
        lines.push("Mock HTTP calls with jest.mock('axios') or jest.spyOn(globalThis, 'fetch').".to_string());
    }
    lines.push("Assert DOM: wrapper.find('selector').exists(), wrapper.text(). Trigger events: await wrapper.trigger('click').".to_string());
    lines.join(" ")
}

fn classify_laravel_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with("Test.php") || file_path.contains("/tests/") {
        return String::new();
    }
    let is_controller = content.contains("extends Controller")
        || content.contains("class") && content.contains("Controller");
    let is_model = content.contains("extends Model") || content.contains("HasFactory");
    let is_job = content.contains("implements ShouldQueue") || content.contains("extends Job");
    let is_command = content.contains("extends Command");
    let is_middleware = content.contains("handle(Request") && content.contains("Closure");

    if is_controller {
        return "Laravel Controller. Extend Tests\\TestCase. Use $this->get('/route'), $this->postJson('/route', [...]) for HTTP tests. Use actingAs($user) for authenticated routes. Assert with assertStatus(), assertJson(), assertRedirect().".to_string();
    }
    if is_model {
        return "Laravel Eloquent Model. Use RefreshDatabase trait to reset DB state between tests. Use model factories: ModelClass::factory()->create([...]) instead of raw DB inserts. Test relationships, scopes, and accessors/mutators.".to_string();
    }
    if is_job {
        return "Laravel Job. Use Bus::fake() before dispatching. Assert with Bus::assertDispatched(JobClass::class, fn($job) => $job->property === $value). Test the handle() method directly for unit coverage.".to_string();
    }
    if is_command {
        return "Laravel Artisan Command. Use $this->artisan('command:name', ['--option' => 'value'])->assertExitCode(0). Test the handle() method unit-style by mocking dependencies.".to_string();
    }
    if is_middleware {
        return "Laravel Middleware. Test via HTTP: $this->withoutMiddleware()->get('/route') for isolation, or let middleware run and assert on the response. Unit-test handle() directly by passing a mock Request and Closure.".to_string();
    }
    // Service, helper, or value object
    "Laravel service/helper class. Use PHPUnit directly — instantiate with new or use $this->mock(Dep::class) for dependencies. No HTTP testing infrastructure needed.".to_string()
}

fn classify_symfony_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with("Test.php") || file_path.contains("/tests/") {
        return String::new();
    }
    let is_controller = content.contains("AbstractController") || content.contains("#[Route(")
        || content.contains("@Route(");
    let is_entity = content.contains("#[ORM\\Entity") || content.contains("@ORM\\Entity");
    let has_constructor_injection = content.contains("public function __construct(")
        && (content.contains("private") || content.contains("readonly"));

    if is_controller {
        return "Symfony Controller. Extend WebTestCase. Use static::createClient()->request('GET', '/path') for HTTP tests. Assert with $this->assertResponseIsSuccessful(), $this->assertResponseStatusCodeSame(200).".to_string();
    }
    if is_entity {
        return "Symfony/Doctrine Entity. Use plain PHPUnit TestCase — test constructors, getters, setters, and any domain logic. No Symfony kernel needed for entity unit tests.".to_string();
    }
    if has_constructor_injection {
        return "Symfony Service. Extend KernelTestCase. Get the service via static::getContainer()->get(ServiceClass::class). For unit tests, mock constructor dependencies with $this->createMock(DepClass::class).".to_string();
    }
    "Symfony class. Use plain PHPUnit TestCase for utilities/value objects. Use KernelTestCase only when Symfony DI is required.".to_string()
}

fn classify_python_file(file_path: &str, content: &str, project_path: &Path) -> String {
    if file_path.contains("test_") || file_path.ends_with("_test.py") {
        return String::new();
    }
    let is_django = project_path.join("manage.py").exists();
    let is_flask = content.contains("from flask") || content.contains("import flask");
    let is_django_view = is_django && (content.contains("def get(") || content.contains("def post(")
        || content.contains("HttpResponse") || content.contains("JsonResponse")
        || content.contains("APIView") || content.contains("ViewSet"));
    let is_django_model = is_django && content.contains("models.Model");
    let is_django_form = is_django && (content.contains("forms.Form") || content.contains("forms.ModelForm"));

    if is_django_view {
        return "Django view/ViewSet. Use from django.test import TestCase and self.client.get('/path/') or self.client.post('/path/', data). Use APIClient from rest_framework.test for DRF views. setUp() creates test data.".to_string();
    }
    if is_django_model {
        return "Django Model. Use from django.test import TestCase. Create instances in setUp() or directly in test methods. Use self.assertEqual, self.assertRaises. Tests automatically run in a transaction that is rolled back.".to_string();
    }
    if is_django_form {
        return "Django Form. Instantiate the form with data dict: form = MyForm(data={'field': 'value'}). Assert form.is_valid() is True/False and check form.errors for invalid cases.".to_string();
    }
    if is_flask {
        return "Flask view/route. Use app.test_client() to get a test client. Call client.get('/path') and client.post('/path', json={...}). Assert response.status_code and response.get_json().".to_string();
    }
    // Plain Python
    "Plain Python module. Use pytest — test functions named test_*. Use unittest.mock.patch or pytest-mock's mocker fixture for mocking. Prefer pytest.raises(ExcType) for exception testing.".to_string()
}

fn classify_ruby_file(file_path: &str, content: &str) -> String {
    if file_path.ends_with("_spec.rb") || file_path.contains("/spec/") {
        return String::new();
    }
    let is_model = file_path.contains("app/models/") || content.contains("ApplicationRecord")
        || content.contains("ActiveRecord::Base");
    let is_controller = file_path.contains("app/controllers/")
        || content.contains("ApplicationController") || content.contains("ActionController");
    let is_service = file_path.contains("app/services/") || file_path.contains("app/interactors/");

    if is_model {
        return "Rails Model. Use RSpec type: :model. Use FactoryBot.create(:model_name) for test data. Test validations with expect(record).to be_valid / be_invalid. Test associations with shoulda-matchers: it { is_expected.to have_many(:items) }.".to_string();
    }
    if is_controller {
        return "Rails Controller. Use RSpec type: :controller or type: :request. For request specs: get '/path', headers: {...}; expect(response).to have_http_status(:ok). For controller specs: get :action, params: {...}. Use Devise helpers for authentication.".to_string();
    }
    if is_service {
        return "Rails Service Object / PORO. Use plain RSpec describe without type tag. Instantiate directly: described_class.new(arg1, arg2).call. Use instance_double for dependencies.".to_string();
    }
    "Ruby class. Use plain RSpec describe. Instantiate with described_class.new. Use let for shared setup.".to_string()
}

/// Extract Spring/JPA/Kotlin annotation-based guidance for Java and Kotlin files.
fn classify_java_kotlin_file(content: &str) -> String {
    let has_rest_controller = content.contains("@RestController") || content.contains("@Controller");
    let has_service = content.contains("@Service") || content.contains("@Component");
    let has_repository = content.contains("@Repository");
    let has_entity = content.contains("@Entity") || content.contains("@Table");
    let has_configuration = content.contains("@Configuration") || content.contains("@Bean");

    if has_rest_controller {
        return "This is a @RestController. Use @WebMvcTest or plain Mockito with MockMvc for testing. Do NOT use @SpringBootTest.".to_string();
    }
    if has_service {
        return "This is a @Service/@Component. Use @ExtendWith(MockitoExtension.class) with @Mock and @InjectMocks. Do NOT use @SpringBootTest.".to_string();
    }
    if has_repository {
        return "This is a @Repository. Use @ExtendWith(MockitoExtension.class) to mock dependencies. Do NOT use @SpringBootTest.".to_string();
    }
    if has_entity {
        return "This is a JPA @Entity. Generate simple unit tests with JUnit 5 only — test constructors, getters, setters, equals/hashCode. Do NOT use @SpringBootTest.".to_string();
    }
    if has_configuration {
        return "This is a @Configuration class. Test the @Bean methods in isolation with JUnit 5 and Mockito if needed. Do NOT use @SpringBootTest.".to_string();
    }

    let is_enum = content.contains("public enum ") || content.contains("enum class ");
    let is_record = content.contains("public record ");
    let is_interface = content.contains("public interface ");
    let is_plain = !content.contains("@Autowired") && !content.contains("@Inject");

    if is_enum {
        return "This is an enum. Generate simple unit tests with JUnit 5 only — test values, valueOf, any custom methods. Do NOT use Spring or Mockito.".to_string();
    }
    if is_record {
        return "This is a record/DTO. Generate simple unit tests with JUnit 5 only — test constructor, accessors, equals/hashCode. Do NOT use Spring or Mockito.".to_string();
    }
    if is_interface {
        return String::new();
    }
    if is_plain {
        return "This is a POJO/DTO class. Generate simple unit tests with JUnit 5 only — test constructors, getters, setters. Do NOT use @SpringBootTest.".to_string();
    }

    String::new()
}

/// Classify a source file to produce framework-specific testing guidance for the AI prompt (US-040).
///
/// Dispatches by file extension and project-level marker files (angular.json, nest-cli.json,
/// next.config.*, composer.json, Gemfile) to a per-framework classifier. Each classifier
/// returns a short, actionable string that is injected directly into the test-generation prompt.
/// Returns empty string when no specific guidance is available.
pub fn classify_source_file(file_path: &str, project_path: &Path) -> String {
    let full_path = project_path.join(file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    // TypeScript / JavaScript — dispatch by framework marker
    if file_path.ends_with(".ts") || file_path.ends_with(".tsx")
        || file_path.ends_with(".js") || file_path.ends_with(".jsx")
    {
        if project_path.join("angular.json").exists() {
            return classify_angular_file(file_path, &content, is_jest_project(project_path));
        }
        if project_path.join("nest-cli.json").exists() {
            return classify_nestjs_file(file_path, &content);
        }
        if is_nextjs_project(project_path) {
            return classify_nextjs_file(file_path, &content);
        }
        // Generic React — TSX/JSX files or TS/JS with a React import
        if file_path.ends_with(".tsx") || file_path.ends_with(".jsx")
            || content.contains("from 'react'") || content.contains("from \"react\"")
        {
            return classify_react_file(file_path, &content);
        }
        return String::new();
    }

    if file_path.ends_with(".vue") {
        return classify_vue_file(&content);
    }

    if file_path.ends_with(".php") {
        if is_laravel_project(project_path) {
            return classify_laravel_file(file_path, &content);
        }
        if is_symfony_project(project_path) {
            return classify_symfony_file(file_path, &content);
        }
        return String::new();
    }

    if file_path.ends_with(".py") {
        return classify_python_file(file_path, &content, project_path);
    }

    if file_path.ends_with(".rb") {
        return classify_ruby_file(file_path, &content);
    }

    if file_path.ends_with(".java") || file_path.ends_with(".kt") {
        return classify_java_kotlin_file(&content);
    }

    String::new()
}

/// Derive the expected test package from a Java/Kotlin source file path (US-040).
///
/// E.g., "src/main/java/com/example/service/MyService.java" → "com.example.service"
pub fn derive_test_package(file_path: &str) -> Option<String> {
    // Look for src/main/java/ or src/main/kotlin/ prefix
    let markers = ["src/main/java/", "src/main/kotlin/"];
    for marker in &markers {
        if let Some(idx) = file_path.find(marker) {
            let after = &file_path[idx + marker.len()..];
            // Remove filename to get directory path, then convert / to .
            if let Some(last_slash) = after.rfind('/') {
                let package = after[..last_slash].replace('/', ".");
                if !package.is_empty() {
                    return Some(package);
                }
            }
        }
    }
    None
}

/// Result of checking local coverage for a file's line range.
pub struct LocalCoverageResult {
    /// Lines in the range that are covered (hit count > 0).
    pub covered: Vec<u32>,
    /// Lines in the range that are uncovered (hit count == 0).
    pub uncovered: Vec<u32>,
    /// Coverage percentage for the range.
    pub coverage_pct: f64,
    /// Whether all coverable lines in the range are covered.
    pub fully_covered: bool,
}

/// Locate the coverage report in the project. Checks common paths for lcov, JaCoCo XML, and Cobertura XML.
pub fn find_lcov_report(project_path: &Path) -> Option<std::path::PathBuf> {
    find_lcov_report_with_hint(project_path, None)
}

/// Find the coverage report, optionally using an explicit path from config.
/// Emits `warn!` when the hint path is configured but missing, and when no
/// report is found at all.  Use [`find_lcov_report_quietly`] when absence is
/// expected (e.g. deleting a stale report before running coverage).
pub fn find_lcov_report_with_hint(project_path: &Path, coverage_report_hint: Option<&str>) -> Option<std::path::PathBuf> {
    find_lcov_report_impl(project_path, coverage_report_hint, true)
}

/// Like [`find_lcov_report_with_hint`] but never emits warnings.
/// Use this when the absence of a report is expected (e.g. cleaning up before
/// a coverage run, or probing for a report that may not exist yet).
pub fn find_lcov_report_quietly(project_path: &Path, coverage_report_hint: Option<&str>) -> Option<std::path::PathBuf> {
    find_lcov_report_impl(project_path, coverage_report_hint, false)
}

fn find_lcov_report_impl(project_path: &Path, coverage_report_hint: Option<&str>, emit_warnings: bool) -> Option<std::path::PathBuf> {
    // 1. Try explicit path from config
    if let Some(hint) = coverage_report_hint {
        let hint_path = Path::new(hint);
        let abs = if hint_path.is_absolute() {
            hint_path.to_path_buf()
        } else {
            project_path.join(hint)
        };
        if abs.exists() {
            info!("Found coverage report (from config): {}", abs.display());
            return Some(abs);
        }
        if emit_warnings {
            warn!(
                "commands.coverage_report '{}' not found (resolved: {})",
                hint,
                abs.display()
            );
        }
    }

    // 2. Auto-detect from well-known paths
    let candidates = [
        // lcov format
        "coverage/lcov.info",
        "coverage/lcov-report/lcov.info",
        "lcov.info",
        "build/reports/lcov.info",
        // JaCoCo XML (Maven + Gradle)
        "target/site/jacoco/jacoco.xml",
        "target/jacoco/jacoco.xml",
        "build/reports/jacoco/test/jacocoTestReport.xml",
        "build/reports/jacoco/jacocoTestReport.xml",
        // Cobertura XML (Python, Go, etc.)
        "coverage.xml",
        "build/reports/cobertura/coverage.xml",
    ];
    for candidate in &candidates {
        let path = project_path.join(candidate);
        if path.exists() {
            info!("Found coverage report: {}", path.display());
            return Some(path);
        }
    }

    // 3. Glob fallback: Angular/Karma writes coverage/<app-name>/lcov.info;
    //    other tools may use similarly nested paths under coverage/.
    let glob_patterns = ["coverage/**/lcov.info", "coverage/**/lcov-report/lcov.info"];
    for pattern in &glob_patterns {
        let full_pattern = project_path.join(pattern);
        if let Some(pattern_str) = full_pattern.to_str() {
            if let Ok(mut entries) = glob::glob(pattern_str) {
                if let Some(Ok(found)) = entries.next() {
                    info!("Found coverage report (glob): {}", found.display());
                    return Some(found);
                }
            }
        }
    }

    if emit_warnings {
        warn!(
            "No coverage report found. Searched paths: {}",
            candidates.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    None
}

/// Parse a coverage report and check coverage for a specific file and line range.
///
/// Auto-detects lcov, JaCoCo XML, and Cobertura XML by content.
/// Returns `None` if the file is not found in the report.
pub fn check_local_coverage(
    report_path: &Path,
    source_file: &str,
    start_line: u32,
    end_line: u32,
) -> Option<LocalCoverageResult> {
    let content = std::fs::read_to_string(report_path).ok()?;

    let line_hits = if content.trim_start().starts_with('<') {
        if content.contains("<report ") || content.contains("<report>") {
            parse_jacoco_line_hits(&content, source_file)
        } else if content.contains("<coverage ") || content.contains("<coverage>") {
            parse_cobertura_line_hits(&content, source_file)
        } else {
            None
        }
    } else {
        parse_lcov_line_hits(&content, source_file)
    };

    let line_hits = line_hits?;
    if line_hits.is_empty() {
        return None;
    }

    // Check coverage for the requested line range
    let mut covered = Vec::new();
    let mut uncovered = Vec::new();

    for line_num in start_line..=end_line {
        if let Some(&hits) = line_hits.get(&line_num) {
            if hits > 0 {
                covered.push(line_num);
            } else {
                uncovered.push(line_num);
            }
        }
        // Lines not in DA are non-executable (comments, braces, etc.) — skip them
    }

    let total = covered.len() + uncovered.len();
    let coverage_pct = if total == 0 {
        0.0
    } else {
        (covered.len() as f64 / total as f64) * 100.0
    };

    let fully_covered = if total == 0 {
        false // No coverable lines found — treat as not covered
    } else {
        uncovered.is_empty()
    };

    info!(
        "Local coverage for {}:{}-{}: {:.1}% ({}/{} lines covered)",
        source_file, start_line, end_line, coverage_pct, covered.len(), total
    );

    Some(LocalCoverageResult {
        covered,
        uncovered,
        coverage_pct,
        fully_covered,
    })
}

/// Per-file coverage info parsed from coverage reports (lcov, JaCoCo XML, Cobertura XML).
#[derive(Debug, Clone)]
pub struct FileCoverage {
    /// Source file path as reported in the coverage report.
    pub file: String,
    /// Number of coverable lines (DA entries).
    pub total_lines: u64,
    /// Number of covered lines (hit count > 0).
    pub covered_lines: u64,
    /// Line coverage percentage.
    pub coverage_pct: f64,
    /// Line numbers that are coverable but not yet hit (hit count == 0).
    pub uncovered_lines: Vec<u32>,
    /// US-064: total branches in this file (BRDA records). 0 if lcov has no branch data.
    pub total_branches: u64,
    /// US-064: covered branches (BRDA with taken > 0).
    pub covered_branches: u64,
    /// US-064: branch coverage percentage. Defaults to 100.0 when total_branches == 0
    /// so the filter is effectively disabled for reports without branch data.
    pub branch_coverage_pct: f64,
    /// US-064: line numbers with at least one uncovered branch.  Used to prioritize
    /// files that have good line coverage but poor branch coverage.
    pub uncovered_branch_lines: Vec<u32>,
}

impl Default for FileCoverage {
    fn default() -> Self {
        Self {
            file: String::new(),
            total_lines: 0,
            covered_lines: 0,
            coverage_pct: 0.0,
            uncovered_lines: Vec::new(),
            total_branches: 0,
            covered_branches: 0,
            branch_coverage_pct: 100.0,
            uncovered_branch_lines: Vec::new(),
        }
    }
}

/// Parse a coverage report and return per-file coverage, sorted ascending by coverage %.
/// Supports lcov, JaCoCo XML, and Cobertura XML formats (auto-detected by extension/content).
///
/// Returns an empty vec if the file cannot be read.
pub fn per_file_lcov_coverage(report_path: &Path) -> Vec<FileCoverage> {
    let content = match std::fs::read_to_string(report_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let ext = report_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "xml" {
        if content.contains("<report ") || content.contains("<report>") {
            return per_file_jacoco_xml_coverage(&content);
        } else if content.contains("<coverage ") || content.contains("<coverage>") {
            return per_file_cobertura_xml_coverage(&content);
        }
        tracing::warn!("Unknown XML coverage format in {}", report_path.display());
        return Vec::new();
    }

    per_file_lcov_coverage_from_content(&content)
}

/// Parse lcov-format content and return per-file coverage.
fn per_file_lcov_coverage_from_content(content: &str) -> Vec<FileCoverage> {
    let mut results: Vec<FileCoverage> = Vec::new();
    let mut current_file: Option<String> = None;
    let mut total: u64 = 0;
    let mut covered: u64 = 0;
    let mut uncovered_lines: Vec<u32> = Vec::new();
    // US-064: branch coverage state, reset per file
    let mut total_branches: u64 = 0;
    let mut covered_branches: u64 = 0;
    let mut uncovered_branch_lines: std::collections::BTreeSet<u32> =
        std::collections::BTreeSet::new();

    for line in content.lines() {
        if line.starts_with("SF:") {
            current_file = Some(line[3..].to_string());
            total = 0;
            covered = 0;
            uncovered_lines = Vec::new();
            total_branches = 0;
            covered_branches = 0;
            uncovered_branch_lines = std::collections::BTreeSet::new();
        } else if line.starts_with("DA:") {
            let parts: Vec<&str> = line[3..].splitn(2, ',').collect();
            if parts.len() == 2 {
                if let (Ok(line_num), Ok(hits)) = (parts[0].parse::<u32>(), parts[1].parse::<u64>()) {
                    total += 1;
                    if hits > 0 {
                        covered += 1;
                    } else {
                        uncovered_lines.push(line_num);
                    }
                }
            }
        } else if line.starts_with("BRDA:") {
            // US-064: Format: BRDA:<line>,<block>,<branch>,<taken>
            // `taken` is either a number of hits or "-" when not instrumented.
            let parts: Vec<&str> = line[5..].split(',').collect();
            if parts.len() == 4 {
                if let Ok(line_num) = parts[0].parse::<u32>() {
                    total_branches += 1;
                    let taken_hits: u64 = parts[3].parse::<u64>().unwrap_or(0);
                    if parts[3] != "-" && taken_hits > 0 {
                        covered_branches += 1;
                    } else {
                        uncovered_branch_lines.insert(line_num);
                    }
                }
            }
        } else if line == "end_of_record" {
            if let Some(ref file) = current_file {
                let pct = if total == 0 { 0.0 } else { (covered as f64 / total as f64) * 100.0 };
                let branch_pct = if total_branches == 0 {
                    // No branch data → don't let the filter reject this file.
                    100.0
                } else {
                    (covered_branches as f64 / total_branches as f64) * 100.0
                };
                results.push(FileCoverage {
                    file: file.clone(),
                    total_lines: total,
                    covered_lines: covered,
                    coverage_pct: pct,
                    uncovered_lines: uncovered_lines.clone(),
                    total_branches,
                    covered_branches,
                    branch_coverage_pct: branch_pct,
                    uncovered_branch_lines: uncovered_branch_lines.iter().copied().collect(),
                });
            }
            current_file = None;
        }
    }

    // Sort ascending by coverage percentage (least covered first)
    results.sort_by(|a, b| a.coverage_pct.partial_cmp(&b.coverage_pct).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Parse JaCoCo XML and return per-file coverage.
///
/// JaCoCo XML has `<package name="com/example"><sourcefile name="Foo.java"><line nr="N" mi="M" ci="C"/></sourcefile></package>`.
/// We count each `<line>` as a coverable line: covered if `ci > 0`.
fn per_file_jacoco_xml_coverage(content: &str) -> Vec<FileCoverage> {
    let mut results: Vec<FileCoverage> = Vec::new();
    let mut current_package = String::new();
    let mut current_file: Option<String> = None;
    let mut total: u64 = 0;
    let mut covered: u64 = 0;
    let mut uncovered_lines: Vec<u32> = Vec::new();

    // Normalize minified XML: ensure each tag starts on its own line
    let normalized = content.replace('<', "\n<");
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<package ") {
            if let Some(name) = extract_xml_attr(trimmed, "name") {
                current_package = name.replace('/', "/");
            }
        } else if trimmed.starts_with("<sourcefile ") {
            if let Some(name) = extract_xml_attr(trimmed, "name") {
                current_file = Some(format!("{}/{}", current_package, name));
                total = 0;
                covered = 0;
                uncovered_lines = Vec::new();
            }
        } else if trimmed.starts_with("<line ") && current_file.is_some() {
            // JaCoCo <line nr="N" mi="M" ci="C" mb="M" cb="C"/>
            let ci = extract_xml_attr(trimmed, "ci")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let mi = extract_xml_attr(trimmed, "mi")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            // Only count lines that have instructions (ci + mi > 0)
            if ci + mi > 0 {
                total += 1;
                if ci > 0 {
                    covered += 1;
                } else if let Some(nr) = extract_xml_attr(trimmed, "nr").and_then(|v| v.parse::<u32>().ok()) {
                    uncovered_lines.push(nr);
                }
            }
        } else if trimmed.starts_with("</sourcefile>") {
            if let Some(ref file) = current_file {
                if total > 0 {
                    let pct = (covered as f64 / total as f64) * 100.0;
                    results.push(FileCoverage {
                        file: file.clone(),
                        total_lines: total,
                        covered_lines: covered,
                        coverage_pct: pct,
                        uncovered_lines: uncovered_lines.clone(),
                        ..Default::default()
                    });
                }
            }
            current_file = None;
        }
    }

    results.sort_by(|a, b| a.coverage_pct.partial_cmp(&b.coverage_pct).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Parse Cobertura XML and return per-file coverage.
///
/// Cobertura XML: `<class filename="src/foo.py"><lines><line number="N" hits="H"/></lines></class>`.
fn per_file_cobertura_xml_coverage(content: &str) -> Vec<FileCoverage> {
    let mut results: Vec<FileCoverage> = Vec::new();
    let mut current_file: Option<String> = None;
    let mut total: u64 = 0;
    let mut covered: u64 = 0;
    let mut uncovered_lines: Vec<u32> = Vec::new();

    // Normalize minified XML: ensure each tag starts on its own line
    let normalized = content.replace('<', "\n<");
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<class ") {
            if let Some(filename) = extract_xml_attr(trimmed, "filename") {
                current_file = Some(filename);
                total = 0;
                covered = 0;
                uncovered_lines = Vec::new();
            }
        } else if trimmed.starts_with("<line ") && current_file.is_some() {
            if let Some(hits_str) = extract_xml_attr(trimmed, "hits") {
                if let Ok(hits) = hits_str.parse::<u64>() {
                    total += 1;
                    if hits > 0 {
                        covered += 1;
                    } else if let Some(num) = extract_xml_attr(trimmed, "number").and_then(|v| v.parse::<u32>().ok()) {
                        uncovered_lines.push(num);
                    }
                }
            }
        } else if trimmed.starts_with("</class>") {
            if let Some(ref file) = current_file {
                if total > 0 {
                    let pct = (covered as f64 / total as f64) * 100.0;
                    results.push(FileCoverage {
                        file: file.clone(),
                        total_lines: total,
                        covered_lines: covered,
                        coverage_pct: pct,
                        uncovered_lines: uncovered_lines.clone(),
                        ..Default::default()
                    });
                }
            }
            current_file = None;
        }
    }

    results.sort_by(|a, b| a.coverage_pct.partial_cmp(&b.coverage_pct).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Parse lcov content and return per-line hit counts for `source_file`.
/// Matches when either path is a suffix of the other.
fn parse_lcov_line_hits(content: &str, source_file: &str) -> Option<std::collections::HashMap<u32, u64>> {
    let mut in_target = false;
    let mut found = false;
    let mut hits: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    for line in content.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            in_target = path.ends_with(source_file) || source_file.ends_with(path);
            if in_target {
                found = true;
                hits.clear();
            }
        } else if line == "end_of_record" {
            if in_target {
                break;
            }
        } else if in_target {
            if let Some(rest) = line.strip_prefix("DA:") {
                let mut parts = rest.splitn(2, ',');
                if let (Some(ln), Some(hc)) = (parts.next(), parts.next()) {
                    if let (Ok(line_num), Ok(h)) = (ln.parse::<u32>(), hc.parse::<u64>()) {
                        hits.insert(line_num, h);
                    }
                }
            }
        }
    }

    if !found { None } else { Some(hits) }
}

/// Parse JaCoCo XML and return per-line hit counts for `source_file`.
/// JaCoCo reports paths as `<package name="com/x"><sourcefile name="Foo.java">`;
/// the full path is `com/x/Foo.java`. We match if either the caller's path
/// ends with the JaCoCo path or vice versa (tolerates `src/main/java/` prefix).
fn parse_jacoco_line_hits(content: &str, source_file: &str) -> Option<std::collections::HashMap<u32, u64>> {
    let mut current_package = String::new();
    let mut in_target = false;
    let mut found = false;
    let mut hits: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    let normalized = content.replace('<', "\n<");
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<package ") {
            if let Some(name) = extract_xml_attr(trimmed, "name") {
                current_package = name;
            }
        } else if trimmed.starts_with("<sourcefile ") {
            if let Some(name) = extract_xml_attr(trimmed, "name") {
                let full = if current_package.is_empty() {
                    name
                } else {
                    format!("{}/{}", current_package, name)
                };
                in_target = full.ends_with(source_file) || source_file.ends_with(&full);
                if in_target {
                    found = true;
                    hits.clear();
                }
            }
        } else if trimmed.starts_with("</sourcefile>") {
            if in_target {
                break;
            }
        } else if in_target && trimmed.starts_with("<line ") {
            let ci = extract_xml_attr(trimmed, "ci")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let mi = extract_xml_attr(trimmed, "mi")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            if ci + mi > 0 {
                if let Some(nr) = extract_xml_attr(trimmed, "nr").and_then(|v| v.parse::<u32>().ok()) {
                    hits.insert(nr, ci);
                }
            }
        }
    }

    if !found { None } else { Some(hits) }
}

/// Parse Cobertura XML and return per-line hit counts for `source_file`.
fn parse_cobertura_line_hits(content: &str, source_file: &str) -> Option<std::collections::HashMap<u32, u64>> {
    let mut in_target = false;
    let mut found = false;
    let mut hits: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    let normalized = content.replace('<', "\n<");
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<class ") {
            if let Some(filename) = extract_xml_attr(trimmed, "filename") {
                in_target = filename.ends_with(source_file) || source_file.ends_with(&filename);
                if in_target {
                    found = true;
                    hits.clear();
                }
            }
        } else if trimmed.starts_with("</class>") {
            if in_target {
                break;
            }
        } else if in_target && trimmed.starts_with("<line ") {
            if let (Some(num), Some(hc)) = (
                extract_xml_attr(trimmed, "number").and_then(|v| v.parse::<u32>().ok()),
                extract_xml_attr(trimmed, "hits").and_then(|v| v.parse::<u64>().ok()),
            ) {
                hits.insert(num, hc);
            }
        }
    }

    if !found { None } else { Some(hits) }
}

/// Extract the value of an XML attribute from a tag string.
/// E.g., `extract_xml_attr(r#"<line nr="10" ci="5"/>"#, "ci")` → `Some("5")`
fn extract_xml_attr(tag: &str, attr: &str) -> Option<String> {
    let search = format!("{}=\"", attr);
    let start = tag.find(&search)? + search.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Parse a coverage report and compute the overall project-wide line coverage percentage.
/// Supports lcov, JaCoCo XML, and Cobertura XML formats.
///
/// Returns `None` if the file cannot be read or contains no coverage data.
pub fn overall_lcov_coverage(report_path: &Path) -> Option<f64> {
    let file_coverages = per_file_lcov_coverage(report_path);
    if file_coverages.is_empty() {
        return None;
    }

    let total: u64 = file_coverages.iter().map(|fc| fc.total_lines).sum();
    let covered: u64 = file_coverages.iter().map(|fc| fc.covered_lines).sum();

    if total == 0 {
        return None;
    }

    Some((covered as f64 / total as f64) * 100.0)
}

/// Extract a useful error summary from build/test output.
///
/// Searches for common error patterns (Maven `[ERROR]`, Gradle `FAILED`,
/// compiler errors, exceptions) and returns context lines around them.
/// Falls back to the tail of the output when no recognizable pattern is found.
pub fn extract_error_summary(output: &str, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output.to_string();
    }

    // Error patterns ordered by specificity
    let patterns: &[&str] = &[
        // Maven
        "[ERROR]",
        "BUILD FAILURE",
        "Compilation failure",
        "compilation error",
        // Gradle
        "> Task ",
        "FAILED",
        // General compiler / runtime
        "error:",
        "error[",
        "FAIL",
        "Exception",
        "panic",
        "cannot find symbol",
        "does not exist",
    ];

    let lines: Vec<&str> = output.lines().collect();

    // Collect indices of lines matching any error pattern (case-insensitive for general patterns)
    let mut error_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for &pat in patterns {
            if pat == pat.to_uppercase().as_str() {
                // Exact-case patterns: [ERROR], BUILD FAILURE, FAILED, FAIL, Exception
                if line.contains(pat) {
                    error_indices.push(i);
                    break;
                }
            } else {
                // Case-insensitive patterns
                if line.to_lowercase().contains(&pat.to_lowercase()) {
                    error_indices.push(i);
                    break;
                }
            }
        }
    }

    if error_indices.is_empty() {
        // Fallback: return the tail of the output
        let tail: String = output.chars().skip(output.chars().count() - max_chars).collect();
        return format!("...{}", tail);
    }

    // Deduplicate and collect context around error lines (1 line before, 2 after)
    let mut selected: Vec<usize> = Vec::new();
    for &idx in &error_indices {
        let start = idx.saturating_sub(1);
        let end = (idx + 3).min(lines.len());
        for i in start..end {
            selected.push(i);
        }
    }
    selected.sort_unstable();
    selected.dedup();

    // Build result from selected lines, respecting max_chars
    let mut result = String::new();
    let mut prev_idx: Option<usize> = None;
    for &i in &selected {
        if let Some(prev) = prev_idx {
            if i > prev + 1 {
                result.push_str("\n  ...\n");
            }
        }
        let line = lines[i];
        if result.chars().count() + line.chars().count() + 1 > max_chars {
            if result.is_empty() {
                // At least include a partial first line
                let remaining = max_chars.saturating_sub(4);
                let partial: String = line.chars().take(remaining).collect();
                result.push_str(&partial);
                result.push_str("...");
            }
            break;
        }
        result.push_str(line);
        result.push('\n');
        prev_idx = Some(i);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -- derive_surefire_filter --

    #[test]
    fn derive_surefire_filter_java() {
        let files = vec!["src/main/java/com/example/Foo.java".to_string()];
        let filter = derive_surefire_filter(&files).unwrap();
        assert!(filter.contains("Foo"));
        assert!(filter.contains("FooTest"));
        assert!(filter.contains("FooTests"));
        assert!(filter.contains("*FooTest"));
    }

    #[test]
    fn derive_surefire_filter_multiple_files() {
        let files = vec![
            "src/main/java/Foo.java".to_string(),
            "src/main/java/Bar.java".to_string(),
        ];
        let filter = derive_surefire_filter(&files).unwrap();
        assert!(filter.contains("FooTest"));
        assert!(filter.contains("BarTest"));
    }

    #[test]
    fn derive_surefire_filter_uses_test_class_name_directly() {
        let files = vec![
            "src/test/java/FooTest.java".to_string(),
            "src/tests/BarTest.java".to_string(),
        ];
        // A fix inside a test file should target that class directly so we
        // don't fall back to the full suite.
        let filter = derive_surefire_filter(&files).unwrap();
        assert!(filter.contains("FooTest"), "got: {}", filter);
        assert!(filter.contains("BarTest"), "got: {}", filter);
        // No wildcard patterns for test-file inputs
        assert!(!filter.contains("*"), "got: {}", filter);
    }

    #[test]
    fn derive_surefire_filter_non_java_returns_none() {
        let files = vec!["src/main/python/foo.py".to_string()];
        assert!(derive_surefire_filter(&files).is_none());
    }

    #[test]
    fn derive_surefire_filter_empty() {
        assert!(derive_surefire_filter(&[]).is_none());
    }

    #[test]
    fn derive_surefire_filter_kotlin() {
        let files = vec!["src/main/kotlin/Foo.kt".to_string()];
        let filter = derive_surefire_filter(&files).unwrap();
        assert!(filter.contains("FooTest"));
    }

    // -- inject_or_merge_surefire_filter --

    #[test]
    fn inject_surefire_filter_new() {
        let cmd = "mvn test -Pjar";
        let out = inject_or_merge_surefire_filter(cmd, "FooTest,BarTest");
        assert!(out.contains("-Dtest=FooTest,BarTest"));
        assert!(out.contains("-Pjar"));
    }

    #[test]
    fn inject_surefire_filter_preserves_negatives() {
        // Existing `-Dtest=!H2DatabaseTest` should be preserved and ANDed
        let cmd = "mvn test -Pjar -Dtest=!com.h2test.H2DatabaseTest";
        let out = inject_or_merge_surefire_filter(cmd, "FooTest,BarTest");
        assert!(out.contains("-Dtest=FooTest,BarTest,!com.h2test.H2DatabaseTest"), "got: {}", out);
    }

    #[test]
    fn inject_surefire_filter_replaces_existing_positive() {
        let cmd = "mvn test -Dtest=OldTest";
        let out = inject_or_merge_surefire_filter(cmd, "NewTest");
        assert!(out.contains("-Dtest=NewTest"), "got: {}", out);
        assert!(!out.contains("OldTest"), "old positive retained: {}", out);
    }

    #[test]
    fn inject_surefire_filter_with_env_prefix() {
        let cmd = "export JAVA_HOME=$(/usr/libexec/java_home -v 21) && mvn test -Pjar";
        let out = inject_or_merge_surefire_filter(cmd, "FooTest");
        assert!(out.contains("export JAVA_HOME"), "env lost: {}", out);
        assert!(out.contains("-Dtest=FooTest"));
    }

    #[test]
    fn test_detect_test_command_python() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "[project]").unwrap();
        let cmd = detect_test_command(tmp.path());
        assert_eq!(cmd, Some("python -m pytest".to_string()));
    }

    #[test]
    fn test_detect_test_command_node() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let cmd = detect_test_command(tmp.path());
        assert_eq!(cmd, Some("npm test".to_string()));
    }

    #[test]
    fn test_detect_test_command_rust() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let cmd = detect_test_command(tmp.path());
        assert_eq!(cmd, Some("cargo test".to_string()));
    }

    #[test]
    fn test_detect_test_command_maven() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        let cmd = detect_test_command(tmp.path());
        assert_eq!(cmd, Some("mvn test".to_string()));
    }

    #[test]
    fn test_detect_test_command_none() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = detect_test_command(tmp.path());
        assert_eq!(cmd, None);
    }

    #[test]
    fn test_detect_test_dependencies_junit5_mockito() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("pom.xml"),
            r#"<project>
  <dependencies>
    <dependency>
      <artifactId>junit-jupiter</artifactId>
    </dependency>
    <dependency>
      <artifactId>mockito-junit-jupiter</artifactId>
    </dependency>
    <dependency>
      <artifactId>assertj-core</artifactId>
    </dependency>
  </dependencies>
</project>"#,
        )
        .unwrap();
        let result = detect_test_dependencies(tmp.path());
        assert!(result.contains("JUnit 5"), "expected JUnit 5 in: {result}");
        assert!(result.contains("Mockito"), "expected Mockito in: {result}");
        assert!(result.contains("AssertJ"), "expected AssertJ in: {result}");
    }

    #[test]
    fn test_detect_test_dependencies_spring_boot() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("pom.xml"),
            r#"<project>
  <dependencies>
    <dependency>
      <artifactId>spring-boot-starter-test</artifactId>
    </dependency>
  </dependencies>
</project>"#,
        )
        .unwrap();
        let result = detect_test_dependencies(tmp.path());
        assert!(result.contains("Spring Boot Test"), "expected Spring Boot Test in: {result}");
    }

    #[test]
    fn test_detect_test_dependencies_empty_project() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_test_dependencies(tmp.path());
        assert!(result.is_empty(), "expected empty string for project with no build file, got: {result}");
    }

    #[test]
    fn test_classify_source_file_service() {
        let tmp = tempfile::tempdir().unwrap();
        let java_path = tmp.path().join("MyService.java");
        fs::write(
            &java_path,
            "@Service\npublic class MyService {\n    void doWork() {}\n}\n",
        )
        .unwrap();
        let result = classify_source_file(java_path.to_str().unwrap(), tmp.path());
        assert!(result.contains("@InjectMocks") || result.contains("Mockito"), "expected Mockito guidance for @Service, got: {result}");
    }

    #[test]
    fn test_classify_source_file_enum() {
        let tmp = tempfile::tempdir().unwrap();
        let java_path = tmp.path().join("SortType.java");
        fs::write(
            &java_path,
            "public enum SortType { ASC, DESC }\n",
        )
        .unwrap();
        let result = classify_source_file(java_path.to_str().unwrap(), tmp.path());
        assert!(result.contains("JUnit 5") || result.contains("enum"), "expected JUnit 5 guidance for enum, got: {result}");
        assert!(!result.contains("SpringBootTest"), "enum should not suggest SpringBootTest, got: {result}");
    }

    #[test]
    fn test_classify_source_file_python_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let py_path = tmp.path().join("calc.py");
        fs::write(&py_path, "def add(a, b): return a + b\n").unwrap();
        let result = classify_source_file(py_path.to_str().unwrap(), tmp.path());
        // Plain Python files (no Django/Flask) get pytest guidance
        assert!(result.contains("pytest"), "expected pytest guidance for plain Python, got: {result}");
    }

    #[test]
    fn test_classify_source_file_unsupported_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("style.css");
        fs::write(&path, "body { margin: 0; }\n").unwrap();
        let result = classify_source_file(path.to_str().unwrap(), tmp.path());
        assert!(result.is_empty(), "unsupported extension should return empty, got: {result}");
    }

    #[test]
    fn test_derive_test_package_maven_layout() {
        let result = derive_test_package("src/main/java/com/example/service/MyService.java");
        assert_eq!(result, Some("com.example.service".to_string()));
    }

    #[test]
    fn test_derive_test_package_no_match() {
        let result = derive_test_package("src/calculator.py");
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_coverage_command_python() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "[project]").unwrap();
        let cmd = detect_coverage_command(tmp.path());
        assert!(cmd.unwrap().contains("--cov"));
    }

    #[test]
    fn test_find_test_examples_with_python_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let tests_dir = tmp.path().join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        fs::write(
            tests_dir.join("test_foo.py"),
            "import pytest\n\ndef test_bar():\n    assert 1 == 1\n",
        )
        .unwrap();
        let examples = find_test_examples(tmp.path());
        assert_eq!(examples.len(), 1);
        assert!(examples[0].contains("test_foo.py"));
        assert!(examples[0].contains("import pytest"));
    }

    #[test]
    fn test_find_test_examples_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let examples = find_test_examples(tmp.path());
        assert!(examples.is_empty());
    }

    #[test]
    fn test_find_test_examples_max_two() {
        let tmp = tempfile::tempdir().unwrap();
        let tests_dir = tmp.path().join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        for i in 0..5 {
            fs::write(
                tests_dir.join(format!("test_{}.py", i)),
                format!("def test_{}(): pass\n", i),
            )
            .unwrap();
        }
        let examples = find_test_examples(tmp.path());
        assert!(examples.len() <= 2, "Should return at most 2 examples");
    }

    #[test]
    fn test_find_test_examples_limited_max_files_and_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let tests_dir = tmp.path().join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        // Create 5 test files, each with 20 lines
        for i in 0..5 {
            let content = (0..20).map(|l| format!("line_{}\n", l)).collect::<String>();
            fs::write(tests_dir.join(format!("test_{}.py", i)), content).unwrap();
        }
        let examples = find_test_examples_limited(tmp.path(), 1, 12);
        assert_eq!(examples.len(), 1, "Should return at most 1 example");
        let line_count = examples[0].lines().count();
        // "// File: ..." header + 12 content lines = 13 lines max
        assert!(line_count <= 13, "Example should have ≤ 12 content lines, got {}", line_count);
    }

    // -- US-059: detect_test_failures_in_output --

    #[test]
    fn detect_failures_maven_surefire() {
        let out = "[INFO] Tests run: 42, Failures: 3, Errors: 1, Skipped: 0";
        let r = detect_test_failures_in_output(out).unwrap();
        assert!(r.contains("3 failures"));
        assert!(r.contains("1 errors"));
    }

    #[test]
    fn detect_failures_maven_all_pass() {
        let out = "Tests run: 42, Failures: 0, Errors: 0, Skipped: 0";
        assert!(detect_test_failures_in_output(out).is_none());
    }

    #[test]
    fn detect_failures_gradle() {
        let out = "17 tests completed, 2 failed";
        let r = detect_test_failures_in_output(out).unwrap();
        assert!(r.contains("Gradle"));
        assert!(r.contains("2 failed"));
    }

    #[test]
    fn detect_failures_pytest() {
        let out = "========= 5 failed, 20 passed in 3.42s =========";
        let r = detect_test_failures_in_output(out).unwrap();
        assert!(r.contains("pytest"));
        assert!(r.contains("5 failed"));
    }

    #[test]
    fn detect_failures_jest() {
        let out = "Test Suites: 1 failed, 2 passed\nTests:       3 failed, 45 passed";
        let r = detect_test_failures_in_output(out).unwrap();
        assert!(r.contains("Jest"));
    }

    #[test]
    fn detect_failures_go_test() {
        let out = "--- FAIL: TestFoo (0.01s)\n    foo_test.go:42";
        assert!(detect_test_failures_in_output(out).is_some());
    }

    #[test]
    fn detect_failures_no_match() {
        let out = "random output with no test framework patterns";
        assert!(detect_test_failures_in_output(out).is_none());
    }

    #[test]
    fn test_run_tests_simple_command() {
        // Use `true` as a command that always succeeds
        let tmp = tempfile::tempdir().unwrap();
        let (success, _output) = run_tests(tmp.path(), "true", 30).unwrap();
        assert!(success);
    }

    #[test]
    fn test_run_tests_failing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (success, _output) = run_tests(tmp.path(), "false", 30).unwrap();
        assert!(!success);
    }

    #[test]
    fn test_detect_coverage_command_angular() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("angular.json"), "{}").unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let cmd = detect_coverage_command(tmp.path());
        assert_eq!(cmd, Some("npm test -- --code-coverage --no-watch".to_string()));
    }

    #[test]
    fn test_find_lcov_report_found() {
        let tmp = tempfile::tempdir().unwrap();
        let cov_dir = tmp.path().join("coverage");
        fs::create_dir_all(&cov_dir).unwrap();
        fs::write(cov_dir.join("lcov.info"), "SF:test.ts\nend_of_record\n").unwrap();
        let result = find_lcov_report(tmp.path());
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("coverage/lcov.info"));
    }

    #[test]
    fn test_find_lcov_report_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_lcov_report(tmp.path()).is_none());
    }

    #[test]
    fn test_detect_test_dependencies_angular_karma() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("angular.json"), "{}").unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"devDependencies":{"karma":"~6"}}"#).unwrap();
        let deps = detect_test_dependencies(tmp.path());
        assert!(deps.contains("Angular"), "should detect Angular: {}", deps);
        assert!(deps.contains("Karma"), "should detect Karma runner: {}", deps);
        assert!(!deps.contains("Jest"), "should not say Jest: {}", deps);
    }

    #[test]
    fn test_detect_test_dependencies_angular_jest() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("angular.json"), "{}").unwrap();
        fs::write(tmp.path().join("jest.config.js"), "module.exports = {};").unwrap();
        let deps = detect_test_dependencies(tmp.path());
        assert!(deps.contains("Angular"), "should detect Angular: {}", deps);
        assert!(deps.contains("Jest"), "should detect Jest runner: {}", deps);
    }

    #[test]
    fn test_detect_test_dependencies_angular_testing_library() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("angular.json"), "{}").unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"devDependencies":{"@testing-library/angular":"^14"}}"#).unwrap();
        let deps = detect_test_dependencies(tmp.path());
        assert!(deps.contains("Angular Testing Library"), "should detect testing-library: {}", deps);
    }

    #[test]
    fn test_classify_angular_component_karma() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.component.ts",
            "@Component({ selector: 'app-foo' }) export class FooComponent {}",
            false,
        );
        assert!(guidance.contains("TestBed"), "should mention TestBed: {}", guidance);
        assert!(guidance.contains("detectChanges"), "should mention detectChanges: {}", guidance);
        assert!(guidance.contains("fakeAsync"), "Karma path should suggest fakeAsync: {}", guidance);
    }

    #[test]
    fn test_classify_angular_component_jest() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.component.ts",
            "@Component({ selector: 'app-foo' }) export class FooComponent {}",
            true,
        );
        assert!(guidance.contains("async/await"), "Jest path should suggest async/await: {}", guidance);
    }

    #[test]
    fn test_classify_angular_component_with_http() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.component.ts",
            "@Component({}) export class FooComponent { constructor(private http: HttpClient) {} }",
            false,
        );
        assert!(guidance.contains("HttpClientTestingModule"), "should mention HttpClientTestingModule: {}", guidance);
    }

    #[test]
    fn test_classify_angular_service_simple() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.service.ts",
            "@Injectable({ providedIn: 'root' }) export class FooService {}",
            false,
        );
        assert!(guidance.contains("new MyService"), "simple service should suggest direct instantiation: {}", guidance);
        assert!(!guidance.contains("TestBed.createComponent"), "simple service should not require TestBed.createComponent: {}", guidance);
    }

    #[test]
    fn test_classify_angular_service_with_http() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.service.ts",
            "@Injectable({}) export class FooService { constructor(private http: HttpClient) {} }",
            false,
        );
        assert!(guidance.contains("HttpClientTestingModule"), "http service should mention HttpClientTestingModule: {}", guidance);
        assert!(guidance.contains("HttpTestingController"), "http service should mention HttpTestingController: {}", guidance);
    }

    #[test]
    fn test_classify_angular_pipe() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.pipe.ts",
            "@Pipe({ name: 'foo' }) export class FooPipe implements PipeTransform { transform(v: string) { return v; } }",
            false,
        );
        assert!(guidance.contains("transform"), "pipe guidance should mention transform: {}", guidance);
        assert!(guidance.contains("new"), "pipe guidance should suggest direct instantiation: {}", guidance);
        assert!(!guidance.contains("TestBed.createComponent"), "pipe should not require TestBed.createComponent: {}", guidance);
    }

    #[test]
    fn test_classify_angular_directive() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.directive.ts",
            "@Directive({ selector: '[appFoo]' }) export class FooDirective {}",
            false,
        );
        assert!(guidance.contains("HostComponent"), "directive should suggest host component: {}", guidance);
        assert!(guidance.contains("declarations"), "directive should mention declarations: {}", guidance);
    }

    #[test]
    fn test_classify_angular_guard() {
        let guidance = classify_angular_file(
            "src/app/foo/foo.guard.ts",
            "export class FooGuard implements CanActivate { canActivate() { return true; } }",
            false,
        );
        assert!(guidance.contains("ActivatedRouteSnapshot"), "guard should mention ActivatedRouteSnapshot: {}", guidance);
    }

    #[test]
    fn test_classify_angular_skips_spec_files() {
        let guidance = classify_angular_file("src/app/foo/foo.component.spec.ts", "@Component({}) class FooComponent {}", false);
        assert!(guidance.is_empty(), "spec files should return empty: {}", guidance);
    }

    #[test]
    fn test_classify_source_file_angular_component() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("angular.json"), "{}").unwrap();
        let src_dir = tmp.path().join("src").join("app");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("app.component.ts"), "@Component({}) export class AppComponent {}").unwrap();
        let result = classify_source_file("src/app/app.component.ts", tmp.path());
        assert!(result.contains("TestBed"), "classify_source_file should detect Angular @Component: {}", result);
    }

    #[test]
    fn test_find_lcov_report_angular_nested() {
        // Angular/Karma writes to coverage/<app-name>/lcov.info
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("coverage").join("my-app");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("lcov.info"), "SF:src/app.ts\nend_of_record\n").unwrap();
        let result = find_lcov_report(tmp.path());
        assert!(result.is_some(), "should find nested Angular lcov report via glob");
        assert!(result.unwrap().ends_with("lcov.info"));
    }

    #[test]
    fn test_find_lcov_report_with_hint_explicit_path() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("my-reports").join("jacoco.xml");
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::write(&report, "<report></report>").unwrap();
        // Hint with relative path finds it
        let result = find_lcov_report_with_hint(tmp.path(), Some("my-reports/jacoco.xml"));
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("my-reports/jacoco.xml"));
    }

    #[test]
    fn test_find_lcov_report_with_hint_falls_back_to_auto() {
        let tmp = tempfile::tempdir().unwrap();
        // Hint points to nonexistent file, but standard path exists
        let cov_dir = tmp.path().join("coverage");
        fs::create_dir_all(&cov_dir).unwrap();
        fs::write(cov_dir.join("lcov.info"), "SF:test.ts\nend_of_record\n").unwrap();
        let result = find_lcov_report_with_hint(tmp.path(), Some("nope/jacoco.xml"));
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("coverage/lcov.info"));
    }

    #[test]
    fn test_find_lcov_report_with_hint_none_uses_auto() {
        let tmp = tempfile::tempdir().unwrap();
        let cov_dir = tmp.path().join("coverage");
        fs::create_dir_all(&cov_dir).unwrap();
        fs::write(cov_dir.join("lcov.info"), "SF:test.ts\nend_of_record\n").unwrap();
        let result = find_lcov_report_with_hint(tmp.path(), None);
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("coverage/lcov.info"));
    }

    #[test]
    fn test_check_local_coverage_full() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "SF:src/app/service.ts\nDA:10,5\nDA:11,3\nDA:12,1\nend_of_record\n").unwrap();
        let result = check_local_coverage(&lcov, "src/app/service.ts", 10, 12).unwrap();
        assert!(result.fully_covered);
        assert_eq!(result.covered, vec![10, 11, 12]);
        assert!(result.uncovered.is_empty());
        assert!((result.coverage_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_check_local_coverage_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "SF:src/app/service.ts\nDA:10,5\nDA:11,0\nDA:12,1\nend_of_record\n").unwrap();
        let result = check_local_coverage(&lcov, "src/app/service.ts", 10, 12).unwrap();
        assert!(!result.fully_covered);
        assert_eq!(result.covered, vec![10, 12]);
        assert_eq!(result.uncovered, vec![11]);
        assert!((result.coverage_pct - 66.66).abs() < 1.0);
    }

    #[test]
    fn test_check_local_coverage_file_not_in_report() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "SF:src/other.ts\nDA:1,5\nend_of_record\n").unwrap();
        let result = check_local_coverage(&lcov, "src/app/service.ts", 10, 12);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_local_coverage_jacoco_xml_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("jacoco.xml");
        // JaCoCo: ci>0 covered, ci=0 && mi>0 uncovered. Caller passes the
        // full src path; jacoco reports the package-qualified path.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<report name="x"><package name="com/acme/foo"><sourcefile name="Bar.java">
<line nr="10" mi="0" ci="3"/>
<line nr="11" mi="2" ci="0"/>
<line nr="12" mi="0" ci="1"/>
</sourcefile></package></report>"#;
        fs::write(&report, xml).unwrap();
        let result = check_local_coverage(
            &report,
            "src/main/java/com/acme/foo/Bar.java",
            10,
            12,
        )
        .unwrap();
        assert!(!result.fully_covered);
        assert_eq!(result.covered, vec![10, 12]);
        assert_eq!(result.uncovered, vec![11]);
    }

    #[test]
    fn test_check_local_coverage_jacoco_xml_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("jacoco.xml");
        let xml = r#"<report name="x"><package name="com/other"><sourcefile name="Baz.java">
<line nr="1" mi="0" ci="1"/>
</sourcefile></package></report>"#;
        fs::write(&report, xml).unwrap();
        let result =
            check_local_coverage(&report, "src/main/java/com/acme/Bar.java", 1, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_local_coverage_no_coverable_lines_in_range() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        // DA lines exist but outside our range
        fs::write(&lcov, "SF:src/app/service.ts\nDA:1,5\nDA:2,3\nend_of_record\n").unwrap();
        let result = check_local_coverage(&lcov, "src/app/service.ts", 10, 12).unwrap();
        assert!(!result.fully_covered);
        assert_eq!(result.coverage_pct, 0.0);
    }

    #[test]
    fn test_overall_lcov_coverage_full() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "SF:src/a.ts\nDA:1,5\nDA:2,3\nend_of_record\nSF:src/b.ts\nDA:1,1\nDA:2,1\nend_of_record\n").unwrap();
        let pct = overall_lcov_coverage(&lcov).unwrap();
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_overall_lcov_coverage_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "SF:src/a.ts\nDA:1,5\nDA:2,0\nDA:3,0\nDA:4,1\nend_of_record\n").unwrap();
        let pct = overall_lcov_coverage(&lcov).unwrap();
        assert!((pct - 50.0).abs() < 0.01); // 2 covered out of 4
    }

    #[test]
    fn test_overall_lcov_coverage_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("lcov.info");
        fs::write(&lcov, "").unwrap();
        assert!(overall_lcov_coverage(&lcov).is_none());
    }

    #[test]
    fn test_overall_lcov_coverage_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("nonexistent.info");
        assert!(overall_lcov_coverage(&lcov).is_none());
    }

    #[test]
    fn test_extract_xml_attr() {
        assert_eq!(extract_xml_attr(r#"<line nr="10" ci="5"/>"#, "ci"), Some("5".to_string()));
        assert_eq!(extract_xml_attr(r#"<line nr="10" ci="5"/>"#, "nr"), Some("10".to_string()));
        assert_eq!(extract_xml_attr(r#"<package name="com/example">"#, "name"), Some("com/example".to_string()));
        assert_eq!(extract_xml_attr(r#"<line nr="10"/>"#, "ci"), None);
    }

    #[test]
    fn test_jacoco_xml_formatted() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<report name="test">
<package name="com/example">
<sourcefile name="Foo.java">
<line nr="10" mi="3" ci="0" mb="0" cb="0"/>
<line nr="11" mi="0" ci="5" mb="0" cb="0"/>
<line nr="12" mi="2" ci="3" mb="0" cb="0"/>
</sourcefile>
<sourcefile name="Bar.java">
<line nr="5" mi="0" ci="4" mb="0" cb="0"/>
<line nr="6" mi="0" ci="2" mb="0" cb="0"/>
</sourcefile>
</package>
</report>"#;
        let results = per_file_jacoco_xml_coverage(xml);
        assert_eq!(results.len(), 2);
        // Foo.java: 3 lines, 2 covered (lines 11 and 12 have ci > 0)
        let foo = results.iter().find(|r| r.file.contains("Foo.java")).unwrap();
        assert_eq!(foo.total_lines, 3);
        assert_eq!(foo.covered_lines, 2);
        // Bar.java: 2 lines, 2 covered
        let bar = results.iter().find(|r| r.file.contains("Bar.java")).unwrap();
        assert_eq!(bar.total_lines, 2);
        assert_eq!(bar.covered_lines, 2);
    }

    #[test]
    fn test_jacoco_xml_minified() {
        // Simulate minified JaCoCo XML (all on one line)
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?><report name="test"><package name="com/example"><sourcefile name="Foo.java"><line nr="10" mi="3" ci="0" mb="0" cb="0"/><line nr="11" mi="0" ci="5" mb="0" cb="0"/><line nr="12" mi="2" ci="3" mb="0" cb="0"/></sourcefile></package></report>"#;
        let results = per_file_jacoco_xml_coverage(xml);
        assert_eq!(results.len(), 1);
        let foo = &results[0];
        assert_eq!(foo.file, "com/example/Foo.java");
        assert_eq!(foo.total_lines, 3);
        assert_eq!(foo.covered_lines, 2);
    }

    #[test]
    fn test_jacoco_xml_zero_instructions_ignored() {
        let xml = r#"<report name="test"><package name="pkg"><sourcefile name="Empty.java"><line nr="1" mi="0" ci="0" mb="0" cb="0"/></sourcefile></package></report>"#;
        let results = per_file_jacoco_xml_coverage(xml);
        // Line with mi=0 and ci=0 has no instructions — should not count
        assert!(results.is_empty());
    }

    #[test]
    fn test_cobertura_xml_formatted() {
        let xml = r#"<?xml version="1.0" ?>
<coverage version="5.5">
<packages>
<package name="src">
<classes>
<class filename="src/foo.py" line-rate="0.5">
<lines>
<line number="1" hits="1"/>
<line number="2" hits="0"/>
<line number="3" hits="1"/>
<line number="4" hits="0"/>
</lines>
</class>
</classes>
</package>
</packages>
</coverage>"#;
        let results = per_file_cobertura_xml_coverage(xml);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file, "src/foo.py");
        assert_eq!(results[0].total_lines, 4);
        assert_eq!(results[0].covered_lines, 2);
    }

    #[test]
    fn test_cobertura_xml_minified() {
        let xml = r#"<coverage><packages><package name="src"><classes><class filename="src/bar.py"><lines><line number="1" hits="3"/><line number="2" hits="0"/></lines></class></classes></package></packages></coverage>"#;
        let results = per_file_cobertura_xml_coverage(xml);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file, "src/bar.py");
        assert_eq!(results[0].total_lines, 2);
        assert_eq!(results[0].covered_lines, 1);
    }

    // -- US-064: branch coverage parsing --

    #[test]
    fn test_lcov_parses_branch_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("brda.info");
        // Line 10 has 2 branches: one taken, one not. Line 11 has 2 branches, both taken.
        let content = "\
SF:src/classify.rs
DA:10,1
DA:11,1
DA:12,0
BRDA:10,0,0,3
BRDA:10,0,1,0
BRDA:11,0,0,1
BRDA:11,0,1,2
BRF:4
BRH:3
end_of_record
";
        fs::write(&lcov, content).unwrap();
        let results = per_file_lcov_coverage(&lcov);
        assert_eq!(results.len(), 1);
        let fc = &results[0];
        assert_eq!(fc.total_branches, 4);
        assert_eq!(fc.covered_branches, 3);
        assert!((fc.branch_coverage_pct - 75.0).abs() < 0.01);
        assert_eq!(fc.uncovered_branch_lines, vec![10]);
    }

    #[test]
    fn test_lcov_no_branch_data_defaults_to_100() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("nobrda.info");
        // Classic lcov without BRDA records
        let content = "\
SF:src/foo.rs
DA:1,1
DA:2,0
end_of_record
";
        fs::write(&lcov, content).unwrap();
        let results = per_file_lcov_coverage(&lcov);
        assert_eq!(results.len(), 1);
        let fc = &results[0];
        assert_eq!(fc.total_branches, 0);
        // Without branch data we can't penalize the file — default to 100%
        assert!((fc.branch_coverage_pct - 100.0).abs() < 0.01);
        assert!(fc.uncovered_branch_lines.is_empty());
    }

    #[test]
    fn test_lcov_brda_dash_marks_uncovered() {
        let tmp = tempfile::tempdir().unwrap();
        let lcov = tmp.path().join("dash.info");
        // A "-" taken count means the branch wasn't instrumented at runtime (never reached).
        let content = "\
SF:src/foo.rs
DA:5,1
BRDA:5,0,0,1
BRDA:5,0,1,-
end_of_record
";
        fs::write(&lcov, content).unwrap();
        let results = per_file_lcov_coverage(&lcov);
        let fc = &results[0];
        assert_eq!(fc.total_branches, 2);
        assert_eq!(fc.covered_branches, 1);
        assert_eq!(fc.uncovered_branch_lines, vec![5]);
    }

    #[test]
    fn test_per_file_lcov_coverage_jacoco_xml() {
        let tmp = tempfile::tempdir().unwrap();
        let xml_path = tmp.path().join("jacoco.xml");
        let xml = r#"<report name="test"><package name="com/example"><sourcefile name="App.java"><line nr="1" mi="0" ci="5" mb="0" cb="0"/><line nr="2" mi="3" ci="0" mb="0" cb="0"/></sourcefile></package></report>"#;
        fs::write(&xml_path, xml).unwrap();
        let results = per_file_lcov_coverage(&xml_path);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].total_lines, 2);
        assert_eq!(results[0].covered_lines, 1);
    }

    #[test]
    fn test_overall_lcov_coverage_jacoco_xml() {
        let tmp = tempfile::tempdir().unwrap();
        let xml_path = tmp.path().join("jacoco.xml");
        let xml = r#"<report name="test"><package name="com/example"><sourcefile name="App.java"><line nr="1" mi="0" ci="5" mb="0" cb="0"/><line nr="2" mi="3" ci="0" mb="0" cb="0"/></sourcefile></package></report>"#;
        fs::write(&xml_path, xml).unwrap();
        let cov = overall_lcov_coverage(&xml_path).unwrap();
        assert!((cov - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_extract_error_summary_maven_error() {
        let output = "[INFO] Scanning for projects...\n\
            [INFO] ----------------------< com.example:app >-----------------------\n\
            [INFO] Building App 1.0.0\n\
            [INFO]   from pom.xml\n\
            [INFO] --------------------------------[ jar ]---------------------------------\n\
            [INFO] --- maven-compiler-plugin:3.11.0:compile (default-compile) @ app ---\n\
            [ERROR] /src/main/java/App.java:[15,10] cannot find symbol\n\
            [ERROR]   symbol:   class FooService\n\
            [ERROR]   location: class com.example.App\n\
            [INFO] BUILD FAILURE\n\
            [INFO] Total time:  2.345 s";
        let summary = extract_error_summary(output, 500);
        assert!(summary.contains("[ERROR]"), "Should contain [ERROR] lines");
        assert!(summary.contains("cannot find symbol"), "Should contain the actual error");
        assert!(summary.contains("BUILD FAILURE"), "Should contain BUILD FAILURE");
    }

    #[test]
    fn test_extract_error_summary_gradle_failure() {
        let output = "> Task :compileJava UP-TO-DATE\n\
            > Task :processResources NO-SOURCE\n\
            > Task :classes UP-TO-DATE\n\
            > Task :compileTestJava\n\
            /src/test/java/AppTest.java:10: error: cannot find symbol\n\
            > Task :compileTestJava FAILED\n\
            BUILD FAILED in 5s\n\
            3 actionable tasks: 1 executed, 2 up-to-date";
        let summary = extract_error_summary(output, 500);
        assert!(summary.contains("error: cannot find symbol"), "Should contain compile error");
        assert!(summary.contains("FAILED"), "Should contain FAILED marker");
    }

    #[test]
    fn test_extract_error_summary_no_pattern_falls_back_to_tail() {
        let mut output = String::new();
        for i in 0..100 {
            output.push_str(&format!("line {} of normal output\n", i));
        }
        let summary = extract_error_summary(&output, 200);
        assert!(summary.starts_with("..."), "Should start with ... (tail fallback)");
        assert!(summary.contains("line 99"), "Should contain last lines of output");
    }

    #[test]
    fn test_extract_error_summary_short_output_unchanged() {
        let output = "short output";
        let summary = extract_error_summary(output, 500);
        assert_eq!(summary, "short output");
    }

    #[test]
    fn test_extract_error_summary_exception_trace() {
        let output = "[INFO] Running com.example.AppTest\n\
            [INFO] Tests run: 5, Failures: 1, Errors: 0\n\
            [ERROR] testFoo(com.example.AppTest)  Time elapsed: 0.012 s  <<< FAILURE!\n\
            java.lang.NullPointerException: Cannot invoke method on null\n\
            \tat com.example.App.foo(App.java:42)\n\
            \tat com.example.AppTest.testFoo(AppTest.java:15)\n\
            [INFO] BUILD FAILURE";
        let summary = extract_error_summary(output, 800);
        assert!(summary.contains("NullPointerException"), "Should contain exception");
        assert!(summary.contains("[ERROR]"), "Should contain ERROR line");
    }
}
