//! Pre-fix risk assessment for SonarQube issues.
//!
//! Before attempting an automated fix, this module assesses whether the fix is
//! safe to apply in isolation — i.e., it won't require coordinated changes in
//! other parts of the system (frontend, API consumers, mobile clients,
//! infrastructure, or shared libraries).
//!
//! Two complementary strategies:
//!
//! 1. **Static patterns** (zero latency, always active): known cross-cutting
//!    SonarQube rule IDs and message keywords (CSRF, CORS, security headers,
//!    authentication filters, etc.) are matched directly.
//!
//! 2. **AI assessment** (optional, off by default): when `ai_assessment: true`,
//!    Claude (haiku, low effort) is asked to reason about cross-system impact
//!    and return a structured `RISK_LEVEL: HIGH|MEDIUM|LOW` verdict.
//!
//! Configuration lives under `risk_assessment:` in `reparo.yaml`.

use std::path::Path;
use tracing::{info, warn};

use crate::config::RiskAssessmentConfig;
use crate::sonar::Issue;

/// Assessed risk level for a fix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "LOW"),
            RiskLevel::Medium => write!(f, "MEDIUM"),
            RiskLevel::High => write!(f, "HIGH"),
        }
    }
}

/// Outcome of the risk assessment for a single issue.
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub reason: String,
    pub suggested_action: String,
}

// ---------------------------------------------------------------------------
// Static patterns — cross-cutting rule IDs known to require coordinated changes
// ---------------------------------------------------------------------------

/// SonarQube rule ID suffixes that are known to have cross-cutting impact.
/// Enabling or fixing these typically requires changes outside the single file
/// being touched (frontend, API consumers, infrastructure, etc.).
const HIGH_RISK_RULE_SUFFIXES: &[&str] = &[
    "S4502",  // CSRF: Disabling CSRF protections is security-sensitive
    "S5122",  // CORS: Allowing all origins is security-sensitive
    "S2092",  // Cookie without Secure flag — may affect session handling system-wide
    "S3330",  // Cookie without HttpOnly — security header affecting cross-layer trust
    "S4787",  // Encrypting data with RSA without OAEP padding
    "S4790",  // MD5/SHA-1 weak hashing — stored-data format may be shared
    "S2755",  // XML external entities — affects data parsing contracts
    "S6287",  // Spring Security: permitAll() — affects auth contracts
    "S4834",  // Controlling permissions: may affect authorization model
    "S5247",  // Unsafe deserialization — may affect serialization contracts
    "S2384",  // Exposing mutable internal collections via public API
    "S3457",  // Format string: affects API output consumed by callers
];

/// Issue tags that signal cross-cutting or security-hotspot status.
const HIGH_RISK_TAGS: &[&str] = &["csrf", "cors", "security-hotspot"];

/// Case-insensitive substrings in the issue message that indicate cross-cutting risk.
const HIGH_RISK_MESSAGE_PATTERNS: &[&str] = &[
    "csrf",
    "cors",
    "cross-site request forgery",
    "cross-origin",
    "security header",
    "content-security-policy",
    "strict-transport-security",
    "x-frame-options",
    "authentication filter",
    "authorization filter",
    "permitall",
    "authentication entrypoint",
    "security configuration",
    "disable csrf",
    "disable cors",
    "enable csrf",
    "enable cors",
    "http security",
    "websecurityconfigurer",
    "securityfilterchain",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Assess whether a fix for `issue` is safe to apply in isolation.
///
/// Returns `None` when the risk is below the configured skip threshold
/// (i.e., the fix should proceed normally). Returns `Some(RiskAssessment)`
/// when the fix should be skipped, with the reason and suggested action.
pub fn assess_fix_risk(
    issue: &Issue,
    config: &RiskAssessmentConfig,
    project_path: &Path,
    file_content: &str,
    claude_timeout: u64,
    skip_permissions: bool,
    show_prompts: bool,
) -> Option<RiskAssessment> {
    if !config.enabled {
        return None;
    }

    // --- Step 1: Static pattern matching ---
    if let Some(assessment) = check_static_patterns(issue) {
        if assessment.level >= config.skip_threshold_level() {
            info!(
                "Risk assessment: {} skipped (static match, level={}): {}",
                issue.key, assessment.level, assessment.reason
            );
            return Some(assessment);
        }
    }

    // --- Step 2: AI assessment (optional) ---
    if config.ai_assessment {
        match run_ai_assessment(issue, project_path, file_content, claude_timeout, skip_permissions, show_prompts) {
            Ok(Some(assessment)) => {
                if assessment.level >= config.skip_threshold_level() {
                    info!(
                        "Risk assessment: {} skipped (AI, level={}): {}",
                        issue.key, assessment.level, assessment.reason
                    );
                    return Some(assessment);
                }
                info!(
                    "Risk assessment: {} proceed (AI level={})",
                    issue.key, assessment.level
                );
            }
            Ok(None) => {
                info!("Risk assessment: {} — AI returned no verdict, proceeding", issue.key);
            }
            Err(e) => {
                warn!("Risk assessment AI call failed for {}: {} — proceeding with fix", issue.key, e);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Static pattern checks
// ---------------------------------------------------------------------------

fn check_static_patterns(issue: &Issue) -> Option<RiskAssessment> {
    let rule_suffix = issue.rule.split(':').last().unwrap_or(&issue.rule);

    // Match by rule ID
    if HIGH_RISK_RULE_SUFFIXES.iter().any(|r| *r == rule_suffix) {
        return Some(RiskAssessment {
            level: RiskLevel::High,
            reason: format!(
                "Rule `{}` typically requires coordinated changes across multiple system layers \
                 (e.g., frontend, API consumers, security configuration). \
                 Applying this fix in isolation may break other components.",
                issue.rule
            ),
            suggested_action: format!(
                "Review the full impact of enabling `{}` before applying. \
                 Coordinate with frontend, API consumers, and infrastructure teams. \
                 Consider a separate, tracked change with integration testing.",
                issue.rule
            ),
        });
    }

    // Match by tags
    let msg_lower = issue.message.to_lowercase();
    if issue.tags.iter().any(|t| HIGH_RISK_TAGS.contains(&t.as_str())) {
        let matched_tag = issue.tags.iter()
            .find(|t| HIGH_RISK_TAGS.contains(&t.as_str()))
            .map(|s| s.as_str())
            .unwrap_or("security-sensitive");
        return Some(RiskAssessment {
            level: RiskLevel::High,
            reason: format!(
                "Issue is tagged `{}`, indicating it has cross-cutting security implications \
                 that extend beyond the single file being changed.",
                matched_tag
            ),
            suggested_action: "Manually assess the full blast radius before applying. \
                Coordinate with all affected system components."
                .to_string(),
        });
    }

    // Match by message content
    if let Some(pattern) = HIGH_RISK_MESSAGE_PATTERNS.iter().find(|p| msg_lower.contains(*p)) {
        return Some(RiskAssessment {
            level: RiskLevel::High,
            reason: format!(
                "Issue message contains '{}', which typically signals a cross-cutting \
                 security or API contract change that requires coordination across \
                 multiple system layers.",
                pattern
            ),
            suggested_action: "Manually assess the full blast radius before applying. \
                Check if frontend, API consumers, or infrastructure config also need updating."
                .to_string(),
        });
    }

    None
}

// ---------------------------------------------------------------------------
// AI assessment
// ---------------------------------------------------------------------------

fn build_risk_assessment_prompt(issue: &Issue, file_content: &str) -> String {
    // Include only the first 60 lines of the file to give context without
    // overwhelming the model.
    let preview: String = file_content
        .lines()
        .take(60)
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"You are a software architect assessing whether a SonarQube issue can be fixed safely in isolation — without requiring coordinated changes to other parts of the system (frontend, API consumers, mobile clients, infrastructure, or shared libraries).

Issue details:
  Rule:     {rule}
  Message:  {message}
  File:     {file}
  Severity: {severity}

File preview (first 60 lines):
```
{preview}
```

Respond ONLY in this exact format — no other text, no markdown, no explanation outside these lines:
RISK_LEVEL: HIGH|MEDIUM|LOW
CROSS_CUTTING: YES|NO
REASON: <one sentence>
SUGGESTED_ACTION: <one sentence>

Definitions:
  HIGH   — fix requires coordinated changes outside this file (e.g., enabling CSRF forces frontend to send tokens; changing auth headers breaks API consumers; modifying serialization format breaks all clients).
  MEDIUM — fix might affect other components but can likely be done carefully with integration testing (e.g., changing an error message format visible to callers, tightening a public API null-check).
  LOW    — fix is fully local to this file (e.g., removing unused imports, simplifying internal logic, fixing code style, renaming private methods)."#,
        rule = issue.rule,
        message = issue.message,
        file = issue.component,
        severity = issue.severity,
        preview = preview,
    )
}

fn parse_ai_response(output: &str) -> Option<RiskAssessment> {
    let mut level: Option<RiskLevel> = None;
    let mut reason = String::new();
    let mut suggested_action = String::new();

    for line in output.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("RISK_LEVEL:") {
            let val = val.trim().to_uppercase();
            level = match val.as_str() {
                "HIGH" => Some(RiskLevel::High),
                "MEDIUM" => Some(RiskLevel::Medium),
                "LOW" => Some(RiskLevel::Low),
                _ => None,
            };
        } else if let Some(val) = line.strip_prefix("REASON:") {
            reason = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("SUGGESTED_ACTION:") {
            suggested_action = val.trim().to_string();
        }
    }

    level.map(|l| RiskAssessment {
        level: l,
        reason: if reason.is_empty() {
            "AI assessment indicated elevated risk".to_string()
        } else {
            reason
        },
        suggested_action: if suggested_action.is_empty() {
            "Manually review before applying this fix.".to_string()
        } else {
            suggested_action
        },
    })
}

fn run_ai_assessment(
    issue: &Issue,
    project_path: &Path,
    file_content: &str,
    timeout_secs: u64,
    skip_permissions: bool,
    show_prompts: bool,
) -> anyhow::Result<Option<RiskAssessment>> {
    let prompt = build_risk_assessment_prompt(issue, file_content);

    // Use a lightweight text-output invocation (no -d file-edit mode).
    // Haiku + low effort is intentionally cheap — this runs once per issue.
    let invocation = crate::engine::EngineInvocation {
        engine_kind: crate::engine::EngineKind::Claude,
        command: "claude".to_string(),
        base_args: vec!["--output-format".to_string(), "text".to_string()],
        model: Some("haiku".to_string()),
        effort: Some("low".to_string()),
        prompt_flag: "-p".to_string(),
        prompt_via_stdin: false,
        extra_args: Vec::new(),
    };

    // Use a short timeout (30% of base) — assessment should be fast.
    let effective_timeout = ((timeout_secs as f64) * 0.3) as u64;
    let effective_timeout = effective_timeout.max(30).min(120);

    let output = crate::engine::run_engine(
        project_path,
        &prompt,
        effective_timeout,
        skip_permissions,
        show_prompts,
        &invocation,
    )?;

    Ok(parse_ai_response(&output))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonar::Issue;

    fn make_issue(rule: &str, message: &str, tags: Vec<String>) -> Issue {
        Issue {
            key: "TEST-001".to_string(),
            rule: rule.to_string(),
            severity: "MAJOR".to_string(),
            component: "src/SecurityConfig.java".to_string(),
            issue_type: "VULNERABILITY".to_string(),
            message: message.to_string(),
            text_range: None,
            status: "OPEN".to_string(),
            tags,
        }
    }

    #[test]
    fn test_csrf_rule_is_high_risk() {
        let issue = make_issue("java:S4502", "Make sure disabling CSRF protection is safe here.", vec![]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_some());
        assert_eq!(assessment.unwrap().level, RiskLevel::High);
    }

    #[test]
    fn test_cors_rule_is_high_risk() {
        let issue = make_issue("java:S5122", "Make sure allowing requests from any origin is safe.", vec![]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_some());
        assert_eq!(assessment.unwrap().level, RiskLevel::High);
    }

    #[test]
    fn test_csrf_tag_is_high_risk() {
        let issue = make_issue("java:S9999", "Some message.", vec!["csrf".to_string()]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_some());
        assert_eq!(assessment.unwrap().level, RiskLevel::High);
    }

    #[test]
    fn test_csrf_message_pattern_is_high_risk() {
        let issue = make_issue("java:S9999", "CSRF protection is disabled in this configuration.", vec![]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_some());
        assert_eq!(assessment.unwrap().level, RiskLevel::High);
    }

    #[test]
    fn test_cors_message_pattern_is_high_risk() {
        let issue = make_issue("java:S9999", "CORS policy allows all cross-origin requests.", vec![]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_some());
        assert_eq!(assessment.unwrap().level, RiskLevel::High);
    }

    #[test]
    fn test_unused_import_is_not_risk() {
        let issue = make_issue("java:S1128", "Remove this unnecessary import of 'java.util.List'.", vec![]);
        let assessment = check_static_patterns(&issue);
        assert!(assessment.is_none());
    }

    #[test]
    fn test_parse_ai_response_high() {
        let output = "RISK_LEVEL: HIGH\nCROSS_CUTTING: YES\nREASON: Enabling CSRF requires frontend to send tokens.\nSUGGESTED_ACTION: Coordinate with the frontend team before applying.";
        let result = parse_ai_response(output);
        assert!(result.is_some());
        let a = result.unwrap();
        assert_eq!(a.level, RiskLevel::High);
        assert!(a.reason.contains("frontend"));
    }

    #[test]
    fn test_parse_ai_response_low() {
        let output = "RISK_LEVEL: LOW\nCROSS_CUTTING: NO\nREASON: Only removes an unused import.\nSUGGESTED_ACTION: Apply automatically.";
        let result = parse_ai_response(output);
        assert!(result.is_some());
        assert_eq!(result.unwrap().level, RiskLevel::Low);
    }

    #[test]
    fn test_parse_ai_response_malformed_returns_none() {
        let output = "This is not a structured response.";
        let result = parse_ai_response(output);
        assert!(result.is_none());
    }
}
