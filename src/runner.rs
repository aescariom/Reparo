use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::info;

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
    // Angular projects use `ng test --code-coverage` via npm
    if project_path.join("angular.json").exists() {
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

/// Run the test suite and return (success, output)
pub fn run_tests(project_path: &Path, test_command: &str, _timeout_secs: u64) -> Result<(bool, String)> {
    info!("Running tests: {}", test_command);

    let parts: Vec<&str> = test_command.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!("Empty test command");
    }

    let output = Command::new(parts[0])
        .current_dir(project_path)
        .args(&parts[1..])
        .output()
        .context(format!("Failed to execute test command: {}", test_command))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{}\n{}", stdout, stderr);

    Ok((output.status.success(), combined))
}

/// Run tests with coverage reporting
pub fn run_coverage(project_path: &Path, coverage_command: &str, _timeout_secs: u64) -> Result<(bool, String)> {
    info!("Running coverage: {}", coverage_command);

    let parts: Vec<&str> = coverage_command.split_whitespace().collect();
    if parts.is_empty() {
        anyhow::bail!("Empty coverage command");
    }

    let output = Command::new(parts[0])
        .current_dir(project_path)
        .args(&parts[1..])
        .output()
        .context(format!("Failed to execute coverage command: {}", coverage_command))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{}\n{}", stdout, stderr);

    Ok((output.status.success(), combined))
}

/// Run an arbitrary shell command in the project directory (US-014).
/// Uses `sh -c` to support pipes and redirections.
/// Returns (success, combined stdout+stderr).
pub fn run_shell_command(project_path: &Path, command: &str, label: &str) -> Result<(bool, String)> {
    info!("Running {}: {}", label, command);

    let output = Command::new("sh")
        .current_dir(project_path)
        .args(["-c", command])
        .output()
        .context(format!("Failed to execute {} command: {}", label, command))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{}\n{}", stdout, stderr);

    if output.status.success() {
        info!("{} succeeded", label);
    } else {
        tracing::warn!("{} failed (exit {})", label, output.status);
    }

    Ok((output.status.success(), combined))
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
                    // Only take first ~50 lines as example
                    let snippet: String = content.lines().take(50).collect::<Vec<_>>().join("\n");
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

/// Locate the lcov report in the project. Checks common paths.
pub fn find_lcov_report(project_path: &Path) -> Option<std::path::PathBuf> {
    let candidates = [
        "coverage/lcov.info",
        "coverage/lcov-report/lcov.info",
        "lcov.info",
        "build/reports/lcov.info",
    ];
    for candidate in &candidates {
        let path = project_path.join(candidate);
        if path.exists() {
            info!("Found lcov report: {}", path.display());
            return Some(path);
        }
    }
    None
}

/// Parse an lcov file and check coverage for a specific file and line range.
///
/// Returns `None` if the file is not found in the lcov report.
pub fn check_local_coverage(
    lcov_path: &Path,
    source_file: &str,
    start_line: u32,
    end_line: u32,
) -> Option<LocalCoverageResult> {
    let content = std::fs::read_to_string(lcov_path).ok()?;

    // Parse lcov — find the section for our source file
    let mut in_target_file = false;
    let mut line_hits: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    for line in content.lines() {
        if line.starts_with("SF:") {
            let file_path = &line[3..];
            // Match if the lcov path ends with the source file path
            in_target_file = file_path.ends_with(source_file)
                || source_file.ends_with(file_path);
        } else if line == "end_of_record" {
            if in_target_file {
                break; // We found and parsed our file
            }
            line_hits.clear();
        } else if in_target_file && line.starts_with("DA:") {
            // DA:line_number,hit_count
            let parts: Vec<&str> = line[3..].splitn(2, ',').collect();
            if parts.len() == 2 {
                if let (Ok(line_num), Ok(hits)) = (parts[0].parse::<u32>(), parts[1].parse::<u64>()) {
                    line_hits.insert(line_num, hits);
                }
            }
        }
    }

    if !in_target_file || line_hits.is_empty() {
        return None; // File not in lcov report or no DA lines
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

/// Per-file coverage info parsed from lcov.
#[derive(Debug, Clone)]
pub struct FileCoverage {
    /// Source file path as reported in the lcov SF: line.
    pub file: String,
    /// Number of coverable lines (DA entries).
    pub total_lines: u64,
    /// Number of covered lines (hit count > 0).
    pub covered_lines: u64,
    /// Coverage percentage.
    pub coverage_pct: f64,
}

/// Parse an lcov file and return per-file coverage, sorted ascending by coverage %.
///
/// Returns an empty vec if the file cannot be read.
pub fn per_file_lcov_coverage(lcov_path: &Path) -> Vec<FileCoverage> {
    let content = match std::fs::read_to_string(lcov_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut results: Vec<FileCoverage> = Vec::new();
    let mut current_file: Option<String> = None;
    let mut total: u64 = 0;
    let mut covered: u64 = 0;

    for line in content.lines() {
        if line.starts_with("SF:") {
            current_file = Some(line[3..].to_string());
            total = 0;
            covered = 0;
        } else if line.starts_with("DA:") {
            let parts: Vec<&str> = line[3..].splitn(2, ',').collect();
            if parts.len() == 2 {
                if let Ok(hits) = parts[1].parse::<u64>() {
                    total += 1;
                    if hits > 0 {
                        covered += 1;
                    }
                }
            }
        } else if line == "end_of_record" {
            if let Some(ref file) = current_file {
                let pct = if total == 0 { 0.0 } else { (covered as f64 / total as f64) * 100.0 };
                results.push(FileCoverage {
                    file: file.clone(),
                    total_lines: total,
                    covered_lines: covered,
                    coverage_pct: pct,
                });
            }
            current_file = None;
        }
    }

    // Sort ascending by coverage percentage (least covered first)
    results.sort_by(|a, b| a.coverage_pct.partial_cmp(&b.coverage_pct).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Parse an lcov file and compute the overall project-wide line coverage percentage.
///
/// Returns `None` if the lcov file cannot be read or contains no coverage data.
pub fn overall_lcov_coverage(lcov_path: &Path) -> Option<f64> {
    let content = std::fs::read_to_string(lcov_path).ok()?;

    let mut total_lines: u64 = 0;
    let mut covered_lines: u64 = 0;

    for line in content.lines() {
        if line.starts_with("DA:") {
            let parts: Vec<&str> = line[3..].splitn(2, ',').collect();
            if parts.len() == 2 {
                if let Ok(hits) = parts[1].parse::<u64>() {
                    total_lines += 1;
                    if hits > 0 {
                        covered_lines += 1;
                    }
                }
            }
        }
    }

    if total_lines == 0 {
        return None;
    }

    Some((covered_lines as f64 / total_lines as f64) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
