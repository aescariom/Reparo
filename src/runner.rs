use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

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

/// Run the test suite and return (success, output)
pub fn run_tests(project_path: &Path, test_command: &str, _timeout_secs: u64) -> Result<(bool, String)> {
    info!("Running tests: {}", test_command);

    if test_command.is_empty() {
        anyhow::bail!("Empty test command");
    }

    let output = Command::new("sh")
        .current_dir(project_path)
        .args(["-c", test_command])
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

    if coverage_command.is_empty() {
        anyhow::bail!("Empty coverage command");
    }

    let output = Command::new("sh")
        .current_dir(project_path)
        .args(["-c", coverage_command])
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

    // Try package.json for JS/TS projects
    if let Ok(content) = std::fs::read_to_string(project_path.join("package.json")) {
        let content_lower = content.to_lowercase();
        if content_lower.contains("jest") {
            deps.push("Jest");
        }
        if content_lower.contains("mocha") {
            deps.push("Mocha");
        }
        if content_lower.contains("vitest") {
            deps.push("Vitest");
        }
        if content_lower.contains("sinon") {
            deps.push("Sinon");
        }
        if content_lower.contains("chai") {
            deps.push("Chai");
        }
        return deps.join(", ");
    }

    String::new()
}

/// Classify a source file by reading its content to determine the type of test needed (US-040).
///
/// For Java/Kotlin files, detects Spring annotations to recommend the appropriate test approach.
/// Returns empty string for non-Java files or when no classification is possible.
pub fn classify_source_file(file_path: &str, project_path: &Path) -> String {
    let full_path = project_path.join(file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    // Only classify Java/Kotlin files
    if !file_path.ends_with(".java") && !file_path.ends_with(".kt") {
        return String::new();
    }

    // Detect Spring annotations
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

    // Check for DTO/POJO/Enum patterns (no Spring annotations, simple class)
    let is_enum = content.contains("public enum ") || content.contains("enum class ");
    let is_record = content.contains("public record ");
    let is_interface = content.contains("public interface ");
    let has_only_fields = !content.contains("@Autowired")
        && !content.contains("@Inject")
        && !has_rest_controller
        && !has_service
        && !has_repository;

    if is_enum {
        return "This is an enum. Generate simple unit tests with JUnit 5 only — test values, valueOf, any custom methods. Do NOT use Spring or Mockito.".to_string();
    }
    if is_record {
        return "This is a record/DTO. Generate simple unit tests with JUnit 5 only — test constructor, accessors, equals/hashCode. Do NOT use Spring or Mockito.".to_string();
    }
    if is_interface {
        return String::new(); // Interfaces usually don't need direct tests
    }
    if has_only_fields {
        return "This is a POJO/DTO class. Generate simple unit tests with JUnit 5 only — test constructors, getters, setters. Do NOT use @SpringBootTest.".to_string();
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
/// If `coverage_report_hint` is set, it is tried first (absolute or relative to project).
pub fn find_lcov_report_with_hint(project_path: &Path, coverage_report_hint: Option<&str>) -> Option<std::path::PathBuf> {
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
        warn!(
            "commands.coverage_report '{}' not found (resolved: {})",
            hint,
            abs.display()
        );
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
    warn!(
        "No coverage report found. Searched paths: {}",
        candidates.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
    );
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

/// Per-file coverage info parsed from coverage reports (lcov, JaCoCo XML, Cobertura XML).
#[derive(Debug, Clone)]
pub struct FileCoverage {
    /// Source file path as reported in the coverage report.
    pub file: String,
    /// Number of coverable lines (DA entries).
    pub total_lines: u64,
    /// Number of covered lines (hit count > 0).
    pub covered_lines: u64,
    /// Coverage percentage.
    pub coverage_pct: f64,
    /// Line numbers that are coverable but not yet hit (hit count == 0).
    pub uncovered_lines: Vec<u32>,
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

    for line in content.lines() {
        if line.starts_with("SF:") {
            current_file = Some(line[3..].to_string());
            total = 0;
            covered = 0;
            uncovered_lines = Vec::new();
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
        } else if line == "end_of_record" {
            if let Some(ref file) = current_file {
                let pct = if total == 0 { 0.0 } else { (covered as f64 / total as f64) * 100.0 };
                results.push(FileCoverage {
                    file: file.clone(),
                    total_lines: total,
                    covered_lines: covered,
                    coverage_pct: pct,
                    uncovered_lines: uncovered_lines.clone(),
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
                    });
                }
            }
            current_file = None;
        }
    }

    results.sort_by(|a, b| a.coverage_pct.partial_cmp(&b.coverage_pct).unwrap_or(std::cmp::Ordering::Equal));
    results
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
    fn test_classify_source_file_non_java() {
        let tmp = tempfile::tempdir().unwrap();
        let py_path = tmp.path().join("calc.py");
        fs::write(&py_path, "def add(a, b): return a + b\n").unwrap();
        let result = classify_source_file(py_path.to_str().unwrap(), tmp.path());
        // Non-Java files return empty string
        assert!(result.is_empty(), "expected empty for non-Java file, got: {result}");
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
