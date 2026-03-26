//! Pact/contract testing support.
//!
//! Provides API-file detection, contract verification, and contract test
//! generation orchestration. Sits between the coverage check and the fix step.

use anyhow::Result;
use std::path::Path;
use tracing::{info, warn};

/// Result of checking whether a file involves API interactions.
#[derive(Debug, PartialEq)]
pub enum ApiCheckResult {
    /// The file matches API patterns — pact steps should run.
    IsApiFile,
    /// The file is not an API file — skip pact steps.
    NotApiFile,
}

/// Result of contract verification.
#[derive(Debug)]
pub enum PactVerifyResult {
    /// All contracts pass.
    Passed,
    /// Contract verification failed with output.
    Failed { output: String },
    /// No contracts found for this provider/consumer.
    NoContracts,
    /// Verification could not be run (missing command, etc.)
    Unavailable { reason: String },
}

/// Result of contract test generation.
#[derive(Debug)]
pub enum PactTestGenResult {
    /// Contract tests generated and pass.
    Success { test_files: Vec<String> },
    /// Contract tests generated but verification fails.
    TestsFailed { output: String },
    /// Claude failed to generate contract tests.
    GenerationFailed { error: String },
}

/// Check if a file path matches any of the configured API patterns.
///
/// If no patterns are configured, returns `IsApiFile` — the user opted in
/// globally so all files are candidates.
pub fn check_api_file(file_path: &str, api_patterns: &[String]) -> ApiCheckResult {
    if api_patterns.is_empty() {
        return ApiCheckResult::IsApiFile;
    }
    for pattern in api_patterns {
        if let Ok(glob) = glob::Pattern::new(pattern) {
            if glob.matches(file_path) {
                return ApiCheckResult::IsApiFile;
            }
        }
    }
    ApiCheckResult::NotApiFile
}

/// Verify existing pact contracts using the configured verify_command.
///
/// Sets `PACT_DIR` environment variable if an external pact directory is configured,
/// allowing the verify command to locate contracts outside the project root.
pub fn verify_contracts(
    project_path: &Path,
    verify_command: &str,
    pact_dir: Option<&str>,
) -> Result<PactVerifyResult> {
    info!("Running pact verification: {}", verify_command);

    let mut cmd = std::process::Command::new("sh");
    cmd.current_dir(project_path)
        .args(["-c", verify_command]);

    // Set PACT_DIR env var for external pact directories, creating if needed
    if let Some(dir) = pact_dir {
        let resolved = if Path::new(dir).is_absolute() {
            std::path::PathBuf::from(dir)
        } else {
            project_path.join(dir)
        };
        if !resolved.exists() {
            std::fs::create_dir_all(&resolved)?;
            info!("Created pact directory: {}", resolved.display());
        }
        cmd.env("PACT_DIR", &resolved);
        info!("PACT_DIR set to {}", resolved.display());
    }

    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{}\n{}", stdout, stderr);

    if output.status.success() {
        // Check for "no pacts found" indicators
        let lower = combined.to_lowercase();
        if lower.contains("no pacts found") || lower.contains("no contracts") || lower.contains("0 interactions") {
            return Ok(PactVerifyResult::NoContracts);
        }
        Ok(PactVerifyResult::Passed)
    } else {
        // Check if it's a "not found" vs actual failure
        let lower = combined.to_lowercase();
        if lower.contains("no pacts found") || lower.contains("no contracts") {
            return Ok(PactVerifyResult::NoContracts);
        }
        if lower.contains("command not found") || lower.contains("not recognized") {
            return Ok(PactVerifyResult::Unavailable {
                reason: format!("Verify command not found: {}", verify_command),
            });
        }
        Ok(PactVerifyResult::Failed { output: combined })
    }
}

/// Detect the pact framework from project dependency files.
///
/// Returns a hint string for Claude prompt generation (e.g., "pact-js", "pact-jvm").
pub fn detect_pact_framework(project_path: &Path) -> String {
    // Check package.json for JS/TS pact
    let pkg_json = project_path.join("package.json");
    if pkg_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            if content.contains("@pact-foundation/pact") {
                return "pact-js (@pact-foundation/pact)".to_string();
            }
        }
    }

    // Check pom.xml for JVM pact
    let pom = project_path.join("pom.xml");
    if pom.exists() {
        if let Ok(content) = std::fs::read_to_string(&pom) {
            if content.contains("au.com.dius.pact") || content.contains("au.com.dius:pact") {
                return "pact-jvm (Maven)".to_string();
            }
        }
    }

    // Check build.gradle for JVM pact
    for gradle_file in &["build.gradle", "build.gradle.kts"] {
        let gradle = project_path.join(gradle_file);
        if gradle.exists() {
            if let Ok(content) = std::fs::read_to_string(&gradle) {
                if content.contains("au.com.dius.pact") {
                    return "pact-jvm (Gradle)".to_string();
                }
            }
        }
    }

    // Check Cargo.toml for Rust pact
    let cargo = project_path.join("Cargo.toml");
    if cargo.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo) {
            if content.contains("pact_consumer") || content.contains("pact_verifier") {
                return "pact-rust".to_string();
            }
        }
    }

    // Check Python dependencies
    for dep_file in &["requirements.txt", "pyproject.toml", "setup.py", "Pipfile"] {
        let path = project_path.join(dep_file);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if content.contains("pact-python") || content.contains("pact_python") {
                    return "pact-python".to_string();
                }
            }
        }
    }

    // Check go.mod for Go pact
    let gomod = project_path.join("go.mod");
    if gomod.exists() {
        if let Ok(content) = std::fs::read_to_string(&gomod) {
            if content.contains("pact-go") || content.contains("pact-foundation/pact-go") {
                return "pact-go".to_string();
            }
        }
    }

    warn!("Could not detect pact framework — Claude will infer from project context");
    "unknown".to_string()
}

/// Find existing pact/contract test files to use as examples for Claude.
///
/// Returns the content of the first 2 found files (up to ~50 lines each).
pub fn find_contract_test_examples(project_path: &Path) -> Vec<String> {
    let patterns = [
        "**/*.pact.spec.ts",
        "**/*.pact.spec.js",
        "**/*.pact.test.ts",
        "**/*.pact.test.js",
        "**/pact/**/*.test.*",
        "**/pact/**/*.spec.*",
        "**/contract/**/*Test.*",
        "**/*Contract*Test.java",
        "**/*Pact*Test.java",
        "**/test_*pact*.py",
        "**/*_pact_test.go",
        "**/*_pact_test.rs",
    ];

    let mut examples = Vec::new();
    for pattern in &patterns {
        let full = format!("{}/{}", project_path.display(), pattern);
        if let Ok(paths) = glob::glob(&full) {
            for entry in paths.flatten() {
                if examples.len() >= 2 {
                    return examples;
                }
                if let Ok(content) = std::fs::read_to_string(&entry) {
                    let lines: Vec<&str> = content.lines().take(50).collect();
                    let truncated = lines.join("\n");
                    examples.push(format!(
                        "// File: {}\n{}",
                        entry.display(),
                        truncated
                    ));
                }
            }
        }
    }
    examples
}

/// Find existing pact JSON contract files in the pact directory.
///
/// Returns content summaries of found `.json` pact files.
pub fn find_existing_pact_files(project_path: &Path, pact_dir: Option<&str>) -> Vec<String> {
    let search_dir = match pact_dir {
        Some(dir) => {
            let p = Path::new(dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                project_path.join(dir)
            }
        }
        None => {
            // Check common default locations
            let candidates = ["pacts", "pact", "contracts", "contract"];
            let mut found = None;
            for c in &candidates {
                let d = project_path.join(c);
                if d.is_dir() {
                    found = Some(d);
                    break;
                }
            }
            match found {
                Some(d) => d,
                None => return Vec::new(),
            }
        }
    };

    if !search_dir.is_dir() {
        // Auto-create when explicitly configured via pact_dir
        if pact_dir.is_some() {
            if let Err(e) = std::fs::create_dir_all(&search_dir) {
                warn!("Could not create pact directory {}: {}", search_dir.display(), e);
            } else {
                info!("Created pact directory: {}", search_dir.display());
            }
        }
        return Vec::new();
    }

    let pattern = format!("{}/**/*.json", search_dir.display());
    let mut files = Vec::new();
    if let Ok(paths) = glob::glob(&pattern) {
        for entry in paths.flatten() {
            if files.len() >= 3 {
                break;
            }
            if let Ok(content) = std::fs::read_to_string(&entry) {
                // Only include if it looks like a pact file
                if content.contains("\"interactions\"") || content.contains("\"provider\"") || content.contains("\"consumer\"") {
                    let lines: Vec<&str> = content.lines().take(30).collect();
                    files.push(format!("// Pact: {}\n{}", entry.display(), lines.join("\n")));
                }
            }
        }
    }
    files
}

/// Detected project role in a consumer-driven contract testing context.
#[derive(Debug, PartialEq)]
pub enum ProjectRole {
    /// Backend/API server — provides endpoints.
    Provider,
    /// Frontend/API client — consumes endpoints.
    Consumer,
    /// Full-stack project with both provider and consumer signals.
    Both,
    /// Could not determine the role.
    Unknown,
}

/// Detect whether a project is a provider (backend), consumer (frontend), or both.
///
/// Inspects dependency files and directory structures for framework signals.
/// Used to improve Claude prompt quality when provider_name/consumer_name are not configured.
pub fn detect_project_role(project_path: &Path) -> ProjectRole {
    let mut provider_signals = 0;
    let mut consumer_signals = 0;

    // Check package.json for JS/TS frameworks
    let pkg_json = project_path.join("package.json");
    if pkg_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            // Provider signals: Express, Fastify, NestJS, Koa, Hapi
            if content.contains("\"express\"")
                || content.contains("\"fastify\"")
                || content.contains("\"@nestjs/")
                || content.contains("\"koa\"")
                || content.contains("\"@hapi/")
            {
                provider_signals += 1;
            }
            // Consumer signals: React, Angular, Vue, Next.js, axios, fetch wrappers
            if content.contains("\"react\"")
                || content.contains("\"@angular/")
                || content.contains("\"vue\"")
                || content.contains("\"next\"")
                || content.contains("\"axios\"")
                || content.contains("\"@tanstack/react-query\"")
            {
                consumer_signals += 1;
            }
        }
    }

    // Check for Python backend frameworks
    for dep_file in &["requirements.txt", "pyproject.toml", "setup.py", "Pipfile"] {
        let path = project_path.join(dep_file);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if content.contains("django")
                    || content.contains("flask")
                    || content.contains("fastapi")
                    || content.contains("starlette")
                {
                    provider_signals += 1;
                }
                if content.contains("requests")
                    || content.contains("httpx")
                    || content.contains("aiohttp")
                {
                    consumer_signals += 1;
                }
            }
        }
    }

    // Check pom.xml / build.gradle for JVM frameworks
    let pom = project_path.join("pom.xml");
    if pom.exists() {
        if let Ok(content) = std::fs::read_to_string(&pom) {
            if content.contains("spring-boot")
                || content.contains("javax.ws.rs")
                || content.contains("jakarta.ws.rs")
            {
                provider_signals += 1;
            }
            if content.contains("spring-cloud-openfeign")
                || content.contains("retrofit")
                || content.contains("okhttp")
            {
                consumer_signals += 1;
            }
        }
    }

    for gradle_file in &["build.gradle", "build.gradle.kts"] {
        let gradle = project_path.join(gradle_file);
        if gradle.exists() {
            if let Ok(content) = std::fs::read_to_string(&gradle) {
                if content.contains("spring-boot") || content.contains("ktor-server") {
                    provider_signals += 1;
                }
                if content.contains("retrofit") || content.contains("ktor-client") {
                    consumer_signals += 1;
                }
            }
        }
    }

    // Check Go modules
    let gomod = project_path.join("go.mod");
    if gomod.exists() {
        if let Ok(content) = std::fs::read_to_string(&gomod) {
            if content.contains("gin-gonic") || content.contains("gorilla/mux") || content.contains("go-chi") {
                provider_signals += 1;
            }
        }
    }

    // Check Cargo.toml for Rust frameworks
    let cargo = project_path.join("Cargo.toml");
    if cargo.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo) {
            if content.contains("actix-web") || content.contains("axum") || content.contains("rocket") {
                provider_signals += 1;
            }
            if content.contains("reqwest") {
                consumer_signals += 1;
            }
        }
    }

    match (provider_signals > 0, consumer_signals > 0) {
        (true, true) => ProjectRole::Both,
        (true, false) => ProjectRole::Provider,
        (false, true) => ProjectRole::Consumer,
        (false, false) => ProjectRole::Unknown,
    }
}

/// Extended framework detection result with installation status.
#[derive(Debug)]
pub struct PactFrameworkInfo {
    /// Framework name (e.g., "pact-js (@pact-foundation/pact)")
    pub name: String,
    /// Whether the framework appears to be installed (not just declared)
    pub installed: bool,
    /// Hint command to install the framework if not installed
    pub install_hint: String,
}

/// Detect pact framework and check if it's actually installed.
///
/// Goes beyond `detect_pact_framework()` by verifying that dependencies
/// are resolved (e.g., node_modules exists for JS projects).
pub fn detect_pact_framework_info(project_path: &Path) -> PactFrameworkInfo {
    let name = detect_pact_framework(project_path);

    let (installed, install_hint) = match name.as_str() {
        n if n.contains("pact-js") => {
            let installed = project_path
                .join("node_modules/@pact-foundation/pact")
                .exists();
            (
                installed,
                "npm install @pact-foundation/pact --save-dev".to_string(),
            )
        }
        n if n.contains("pact-python") => {
            // Check common venv/site-packages locations
            let venv_check = project_path.join(".venv").exists()
                || project_path.join("venv").exists();
            // If there's a virtualenv, trust that pip install happened
            (venv_check, "pip install pact-python".to_string())
        }
        n if n.contains("pact-jvm") => {
            // Maven/Gradle download deps at build time — trust declaration
            (true, "Dependencies are managed by Maven/Gradle".to_string())
        }
        "pact-rust" => {
            // Cargo fetches at build time — trust declaration
            (true, "Dependencies are managed by Cargo".to_string())
        }
        "pact-go" => {
            // Check if pact-go CLI tool is available
            let installed = which::which("pact-go").is_ok();
            (installed, "go install github.com/pact-foundation/pact-go/v2@latest".to_string())
        }
        _ => (false, "Install your language's pact library".to_string()),
    };

    PactFrameworkInfo {
        name,
        installed,
        install_hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_api_file_no_patterns_returns_api_file() {
        let result = check_api_file("src/services/user.ts", &[]);
        assert_eq!(result, ApiCheckResult::IsApiFile);
    }

    #[test]
    fn test_check_api_file_matches_pattern() {
        let patterns = vec!["**/api/**".to_string(), "**/services/**".to_string()];
        assert_eq!(
            check_api_file("src/api/user.ts", &patterns),
            ApiCheckResult::IsApiFile
        );
        assert_eq!(
            check_api_file("src/services/auth.ts", &patterns),
            ApiCheckResult::IsApiFile
        );
    }

    #[test]
    fn test_check_api_file_no_match() {
        let patterns = vec!["**/api/**".to_string()];
        assert_eq!(
            check_api_file("src/utils/helpers.ts", &patterns),
            ApiCheckResult::NotApiFile
        );
    }

    #[test]
    fn test_check_api_file_glob_pattern() {
        let patterns = vec!["**/clients/**".to_string()];
        assert_eq!(
            check_api_file("src/clients/http-client.ts", &patterns),
            ApiCheckResult::IsApiFile
        );
    }

    #[test]
    fn test_detect_pact_framework_js() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"@pact-foundation/pact": "^12.0.0"}}"#,
        )
        .unwrap();
        let result = detect_pact_framework(tmp.path());
        assert!(result.contains("pact-js"));
    }

    #[test]
    fn test_detect_pact_framework_java_maven() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pom.xml"),
            r#"<dependency><groupId>au.com.dius.pact.provider</groupId></dependency>"#,
        )
        .unwrap();
        let result = detect_pact_framework(tmp.path());
        assert!(result.contains("pact-jvm"));
        assert!(result.contains("Maven"));
    }

    #[test]
    fn test_detect_pact_framework_python() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("requirements.txt"),
            "pact-python==2.0.0\nrequests==2.28.0\n",
        )
        .unwrap();
        let result = detect_pact_framework(tmp.path());
        assert_eq!(result, "pact-python");
    }

    #[test]
    fn test_detect_pact_framework_go() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("go.mod"),
            "module example.com/myapp\nrequire github.com/pact-foundation/pact-go v2.0.0\n",
        )
        .unwrap();
        let result = detect_pact_framework(tmp.path());
        assert_eq!(result, "pact-go");
    }

    #[test]
    fn test_detect_pact_framework_rust() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[dependencies]\npact_consumer = \"0.10\"\n",
        )
        .unwrap();
        let result = detect_pact_framework(tmp.path());
        assert_eq!(result, "pact-rust");
    }

    #[test]
    fn test_detect_pact_framework_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name": "test"}"#).unwrap();
        let result = detect_pact_framework(tmp.path());
        assert_eq!(result, "unknown");
    }

    #[test]
    fn test_find_contract_test_examples() {
        let tmp = tempfile::tempdir().unwrap();
        let test_dir = tmp.path().join("src").join("__tests__");
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(
            test_dir.join("user.pact.spec.ts"),
            "describe('User API pact', () => {\n  it('gets user', () => {});\n});\n",
        )
        .unwrap();
        let examples = find_contract_test_examples(tmp.path());
        assert!(!examples.is_empty());
        assert!(examples[0].contains("User API pact"));
    }

    #[test]
    fn test_find_contract_test_examples_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let examples = find_contract_test_examples(tmp.path());
        assert!(examples.is_empty());
    }

    #[test]
    fn test_find_existing_pact_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pacts_dir = tmp.path().join("pacts");
        std::fs::create_dir_all(&pacts_dir).unwrap();
        std::fs::write(
            pacts_dir.join("webapp-userservice.json"),
            r#"{"consumer": {"name": "WebApp"}, "provider": {"name": "UserService"}, "interactions": []}"#,
        )
        .unwrap();
        let files = find_existing_pact_files(tmp.path(), None);
        assert_eq!(files.len(), 1);
        assert!(files[0].contains("consumer"));
    }

    #[test]
    fn test_find_existing_pact_files_external_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ext_dir = tmp.path().join("external-pacts");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("contract.json"),
            r#"{"provider": {"name": "SVC"}, "interactions": [{"description": "get user"}]}"#,
        )
        .unwrap();
        let files = find_existing_pact_files(tmp.path(), Some(ext_dir.to_str().unwrap()));
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_find_existing_pact_files_no_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let files = find_existing_pact_files(tmp.path(), None);
        assert!(files.is_empty());
    }

    #[test]
    fn test_verify_contracts_command_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let result = verify_contracts(tmp.path(), "echo 'all pacts verified'", None).unwrap();
        assert!(matches!(result, PactVerifyResult::Passed));
    }

    #[test]
    fn test_verify_contracts_command_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let result = verify_contracts(tmp.path(), "echo 'verification failed' && exit 1", None).unwrap();
        assert!(matches!(result, PactVerifyResult::Failed { .. }));
    }

    #[test]
    fn test_verify_contracts_no_pacts() {
        let tmp = tempfile::tempdir().unwrap();
        let result = verify_contracts(tmp.path(), "echo 'no pacts found'", None).unwrap();
        assert!(matches!(result, PactVerifyResult::NoContracts));
    }

    #[test]
    fn test_verify_contracts_with_pact_dir_env() {
        let tmp = tempfile::tempdir().unwrap();
        let pact_dir = tmp.path().join("shared-pacts");
        std::fs::create_dir_all(&pact_dir).unwrap();
        // Verify that the PACT_DIR env var is set by checking it in the command
        let result = verify_contracts(
            tmp.path(),
            "test -n \"$PACT_DIR\"",
            Some(pact_dir.to_str().unwrap()),
        )
        .unwrap();
        assert!(matches!(result, PactVerifyResult::Passed));
    }

    // --- Tests for PactConfig::validate ---

    #[test]
    fn test_pact_config_validate_disabled_returns_empty() {
        let config = crate::config::PactConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(config.validate().is_empty());
    }

    #[test]
    fn test_pact_config_validate_verify_steps_without_command() {
        let config = crate::config::PactConfig {
            enabled: true,
            check_contracts: true,
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("verify_command")));
    }

    #[test]
    fn test_pact_config_validate_generate_tests_without_test_command() {
        let config = crate::config::PactConfig {
            enabled: true,
            generate_tests: true,
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("test_command")));
    }

    #[test]
    fn test_pact_config_validate_missing_names() {
        let config = crate::config::PactConfig {
            enabled: true,
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("provider_name")));
    }

    #[test]
    fn test_pact_config_validate_broker_warning() {
        let config = crate::config::PactConfig {
            enabled: true,
            broker_url: Some("https://broker.example.com".into()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("broker")));
    }

    #[test]
    fn test_pact_config_validate_fully_configured() {
        let config = crate::config::PactConfig {
            enabled: true,
            verify_command: Some("npm run pact:verify".into()),
            test_command: Some("npm run pact:test".into()),
            provider_name: Some("MyAPI".into()),
            consumer_name: Some("MyFrontend".into()),
            check_contracts: true,
            generate_tests: true,
            verify_before_fix: true,
            verify_after_fix: true,
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.is_empty());
    }

    // --- Tests for detect_project_role ---

    #[test]
    fn test_detect_project_role_provider_express() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"express": "^4.18.0"}}"#,
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Provider);
    }

    #[test]
    fn test_detect_project_role_consumer_react() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"react": "^18.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Consumer);
    }

    #[test]
    fn test_detect_project_role_both() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"express": "^4.18.0", "react": "^18.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Both);
    }

    #[test]
    fn test_detect_project_role_python_provider() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("requirements.txt"),
            "fastapi==0.100.0\nuvicorn==0.23.0\n",
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Provider);
    }

    #[test]
    fn test_detect_project_role_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Unknown);
    }

    #[test]
    fn test_detect_project_role_rust_provider() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[dependencies]\nactix-web = \"4\"\n",
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Provider);
    }

    #[test]
    fn test_detect_project_role_rust_consumer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[dependencies]\nreqwest = \"0.11\"\n",
        )
        .unwrap();
        assert_eq!(detect_project_role(tmp.path()), ProjectRole::Consumer);
    }

    // --- Tests for detect_pact_framework_info ---

    #[test]
    fn test_detect_pact_framework_info_js_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"@pact-foundation/pact": "^12.0.0"}}"#,
        )
        .unwrap();
        let info = detect_pact_framework_info(tmp.path());
        assert!(info.name.contains("pact-js"));
        assert!(!info.installed);
        assert!(info.install_hint.contains("npm install"));
    }

    #[test]
    fn test_detect_pact_framework_info_js_installed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"dependencies": {"@pact-foundation/pact": "^12.0.0"}}"#,
        )
        .unwrap();
        let pact_dir = tmp.path().join("node_modules/@pact-foundation/pact");
        std::fs::create_dir_all(&pact_dir).unwrap();
        let info = detect_pact_framework_info(tmp.path());
        assert!(info.name.contains("pact-js"));
        assert!(info.installed);
    }

    #[test]
    fn test_detect_pact_framework_info_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let info = detect_pact_framework_info(tmp.path());
        assert_eq!(info.name, "unknown");
        assert!(!info.installed);
    }

    // --- Tests for pact_dir auto-creation ---

    #[test]
    fn test_verify_contracts_creates_pact_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pact_dir = tmp.path().join("new-pacts");
        assert!(!pact_dir.exists());
        let _ = verify_contracts(
            tmp.path(),
            "echo 'no pacts found'",
            Some("new-pacts"),
        );
        assert!(pact_dir.exists());
    }

    #[test]
    fn test_find_existing_pact_files_creates_configured_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pact_dir = tmp.path().join("my-pacts");
        assert!(!pact_dir.exists());
        let files = find_existing_pact_files(tmp.path(), Some("my-pacts"));
        assert!(files.is_empty());
        assert!(pact_dir.exists());
    }

    #[test]
    fn test_find_existing_pact_files_does_not_create_default_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = find_existing_pact_files(tmp.path(), None);
        // Default candidates should NOT be auto-created
        assert!(!tmp.path().join("pacts").exists());
        assert!(!tmp.path().join("contracts").exists());
    }
}
