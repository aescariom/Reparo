//! Compliance module for Reparo.
//!
//! Implements US-069 (IEC 62304 risk classification), US-073 (requirements YAML),
//! US-070 (MC/DC coverage for Class C), and US-071 (compliance report).
//!
//! # Activation guards:
//! - `compliance_enabled` (--compliance): enables requirements, traceability matrix, compliance report
//! - `health_mode` (--health-mode): additionally enables risk class A/B/C, MC/DC, medical sections

pub mod mcdc;
pub mod report;

use anyhow::{bail, Result};
use tracing::warn;

// ─── Risk classification (US-069) ─────────────────────────────────────────────

/// IEC 62304 software safety class.
/// Ordered from least to most restrictive: A < B < C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum RiskClass {
    /// Class A: no harm possible (logging, UI, reports)
    #[default]
    A,
    /// Class B: non-serious injury possible (business logic, APIs)
    B,
    /// Class C: death or serious injury possible (dosing, life-support, safety control)
    C,
}

impl RiskClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskClass::A => "A",
            RiskClass::B => "B",
            RiskClass::C => "C",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_uppercase().as_str() {
            "A" => Some(RiskClass::A),
            "B" => Some(RiskClass::B),
            "C" => Some(RiskClass::C),
            _ => None,
        }
    }

    /// Return the IEC 62304 description for this class.
    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            RiskClass::A => "No injury possible",
            RiskClass::B => "Non-serious injury possible",
            RiskClass::C => "Death or serious injury possible (safety-critical)",
        }
    }
}

impl std::fmt::Display for RiskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Class {}", self.as_str())
    }
}

/// Per-class testing policy. Fields are optional; missing fields fall back
/// to global config values.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct RiskPolicy {
    pub min_file_coverage: Option<f64>,
    pub min_branch_coverage: Option<f64>,
    pub coverage_rounds: Option<u32>,
    pub require_negative_tests: bool,
    pub require_boundary_tests: bool,
    pub require_mcdc: bool,
}

/// Resolved risk class rule (one entry from `compliance.risk_classes`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RiskClassRule {
    pub class: RiskClass,
    pub description: Option<String>,
    /// Glob patterns for files that belong to this class.
    pub patterns: Vec<String>,
    pub policy: RiskPolicy,
}

/// A requirement declared in `compliance.requirements`.
#[derive(Debug, Clone)]
pub struct Requirement {
    pub id: String,
    pub description: String,
    /// IEC 62304 risk class for this requirement (optional, health-mode only).
    pub risk_class: RiskClass,
    pub source: Option<String>,
    pub risk_control: Option<String>,
    /// Glob patterns for source files this requirement applies to.
    pub files: Vec<String>,
    pub acceptance_criteria: Option<String>,
    /// If "manual", no automatic test is expected.
    pub verification: Option<String>,
    #[allow(dead_code)]
    pub verified_by: Option<String>,
}

impl Requirement {
    /// Returns true when the requirement does not expect automatic test coverage
    /// (i.e. `verification: manual`).
    pub fn is_manual(&self) -> bool {
        self.verification.as_deref().map(|v| v.trim().eq_ignore_ascii_case("manual")).unwrap_or(false)
    }
}

/// Resolved compliance configuration for runtime use.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ComplianceConfig {
    pub enabled: bool,
    /// Ordered list of risk class rules (most restrictive first at resolution time).
    pub risk_classes: Vec<RiskClassRule>,
    /// Default class when no pattern matches.
    pub default_risk_class: RiskClass,
    /// Requirements from the YAML `compliance.requirements` section.
    pub requirements: Vec<Requirement>,
    /// Standards targeted (for display in the compliance report).
    pub standards: Vec<String>,
    /// When true, a FAIL verdict aborts the process with non-zero exit.
    pub fail_on_violation: bool,
    /// Traceability matrix output directory (relative to project path).
    pub traceability_dir: Option<String>,
    /// Include risk class column (health-mode only).
    pub include_risk_class_column: bool,
}

/// Resolve the IEC 62304 risk class for a given file path.
///
/// Rules are evaluated from most restrictive (C) to least (A).
/// The first matching pattern wins (most restrictive class wins when
/// a file matches patterns in multiple classes, because we sort C→B→A).
///
/// When health_mode is false, always returns the default class.
pub fn resolve_risk_class(file: &str, config: &ComplianceConfig, health_mode: bool) -> RiskClass {
    if !health_mode {
        return config.default_risk_class;
    }
    // Sort rules most restrictive first (C > B > A)
    let mut rules = config.risk_classes.iter().collect::<Vec<_>>();
    rules.sort_by(|a, b| b.class.cmp(&a.class));

    for rule in rules {
        for pattern in &rule.patterns {
            if let Ok(p) = glob::Pattern::new(pattern) {
                if p.matches(file) {
                    return rule.class;
                }
            }
        }
    }
    config.default_risk_class
}

/// Find the requirements applicable to a given file by glob matching.
pub fn requirements_for_file<'a>(file: &str, config: &'a ComplianceConfig) -> Vec<&'a Requirement> {
    config.requirements.iter()
        .filter(|r| {
            r.files.iter().any(|pat| {
                glob::Pattern::new(pat)
                    .map(|p| p.matches(file))
                    .unwrap_or(false)
            })
        })
        .collect()
}

/// Resolve the effective risk class for a file, taking into account both the
/// pattern-based risk_class rules (US-069) and any applicable requirements
/// (US-073). The most restrictive class wins.
///
/// Only active with health_mode = true.
pub fn resolve_effective_risk_class(file: &str, config: &ComplianceConfig, health_mode: bool) -> RiskClass {
    let from_rules = resolve_risk_class(file, config, health_mode);
    if !health_mode {
        return from_rules;
    }
    // Check if any requirement for this file has a stricter class
    let reqs = requirements_for_file(file, config);
    reqs.iter()
        .map(|r| r.risk_class)
        .fold(from_rules, |acc, rc| acc.max(rc))
}

/// Find the policy for a given risk class. Falls back to a default policy.
#[allow(dead_code)]
pub fn policy_for_class(class: RiskClass, config: &ComplianceConfig) -> RiskPolicy {
    // Collect all matching rules for the class and merge
    config.risk_classes.iter()
        .find(|r| r.class == class)
        .map(|r| r.policy.clone())
        .unwrap_or_else(|| default_policy_for_class(class))
}

/// Built-in default policy for each IEC 62304 class.
#[allow(dead_code)]
pub fn default_policy_for_class(class: RiskClass) -> RiskPolicy {
    match class {
        RiskClass::A => RiskPolicy {
            min_file_coverage: Some(60.0),
            min_branch_coverage: None,
            coverage_rounds: Some(2),
            require_negative_tests: false,
            require_boundary_tests: false,
            require_mcdc: false,
        },
        RiskClass::B => RiskPolicy {
            min_file_coverage: Some(80.0),
            min_branch_coverage: Some(80.0),
            coverage_rounds: Some(3),
            require_negative_tests: true,
            require_boundary_tests: true,
            require_mcdc: false,
        },
        RiskClass::C => RiskPolicy {
            min_file_coverage: Some(95.0),
            min_branch_coverage: Some(100.0),
            coverage_rounds: Some(5),
            require_negative_tests: true,
            require_boundary_tests: true,
            require_mcdc: true,
        },
    }
}

/// Build the safety classification section to inject into prompts (US-069).
///
/// Returns an empty string when health_mode is false.
pub fn build_safety_classification_section(class: RiskClass, health_mode: bool) -> String {
    if !health_mode {
        return String::new();
    }
    match class {
        RiskClass::C => r#"
## Safety classification: Class C (IEC 62304 — safety-critical)
This file's code directly affects patient safety. Tests MUST cover:
- Every decision outcome independently (MC/DC)
- All boundary values including MIN/MAX/±1
- All error paths and exception types
- Null/empty/zero inputs with defensive behavior verification
- Concurrent-access scenarios if thread safety is claimed
"#.to_string(),
        RiskClass::B => r#"
## Safety classification: Class B (IEC 62304 — non-serious injury possible)
Tests MUST cover:
- Error handling paths and boundary conditions
- Integration with downstream services (mock external dependencies)
- Invalid inputs and defensive behavior
"#.to_string(),
        RiskClass::A => r#"
## Safety classification: Class A (IEC 62304 — no harm possible)
Standard unit testing: cover happy-path and primary error cases.
"#.to_string(),
    }
}

/// Build the requirements section to inject into prompts (US-073).
///
/// Returns an empty string when there are no applicable requirements.
pub fn build_requirements_section(reqs: &[&Requirement], health_mode: bool) -> String {
    if reqs.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "\n## Requirements applicable to this file:\n\n\
         The following requirements from the Software Requirements Specification\n\
         apply to this file. Every test you generate MUST:\n\
         1. Reference the requirement ID it verifies in its `@Reparo.requirement` trace block.\n\
         2. Cover the acceptance criteria below where applicable.\n\n",
    );

    for req in reqs {
        let class_info = if health_mode {
            format!(" ({} — {}", req.risk_class, req.source.as_deref().unwrap_or("no source"))
        } else {
            req.source.as_ref()
                .map(|s| format!(" ({})", s))
                .unwrap_or_default()
        };
        out.push_str(&format!("### {}{}\n", req.id, class_info));
        out.push_str(&format!("**Description**: {}\n", req.description));
        if let Some(ac) = &req.acceptance_criteria {
            out.push_str(&format!("**Acceptance criteria**:\n{}\n", ac));
        }
        if let Some(rc) = &req.risk_control {
            out.push_str(&format!("**Risk control**: {}\n", rc));
        }
        out.push('\n');
    }

    out
}

/// Validate loaded requirements (called at config load time).
pub fn validate_requirements(requirements: &[Requirement]) -> Result<()> {
    use std::collections::HashSet;
    let mut seen_ids = HashSet::new();

    for req in requirements {
        if req.id.is_empty() {
            bail!("compliance.requirements: requirement with empty ID");
        }
        if !seen_ids.insert(req.id.clone()) {
            bail!(
                "compliance.requirements: duplicate requirement ID '{}'",
                req.id
            );
        }
        if req.files.is_empty() && !req.is_manual() {
            bail!(
                "compliance.requirements: requirement '{}' has no files — \
                 add at least one glob pattern or set verification: manual",
                req.id
            );
        }
        // Validate glob patterns
        for pat in &req.files {
            if glob::Pattern::new(pat).is_err() {
                bail!(
                    "compliance.requirements: requirement '{}' has invalid glob pattern '{}'",
                    req.id, pat
                );
            }
        }
    }
    Ok(())
}

/// Bump a tier to account for the risk class (US-069).
///
/// Class B: elevate 1 level. Class C: elevate 2 levels (minimum sonnet-high).
/// Returns the new (model, effort) strings.
#[allow(dead_code)]
pub fn bump_tier_for_risk_class(
    model: &str,
    effort: &str,
    class: RiskClass,
) -> (String, String) {
    let bumps = match class {
        RiskClass::A => 0,
        RiskClass::B => 1,
        RiskClass::C => 2,
    };
    if bumps == 0 {
        return (model.to_string(), effort.to_string());
    }

    // Tier ladder from lowest to highest
    let ladder = [
        ("haiku", "low"),
        ("haiku", "medium"),
        ("sonnet", "low"),
        ("sonnet", "medium"),
        ("sonnet", "high"),
        ("opus", "high"),
        ("opus", "max"),
    ];

    let current_idx = ladder.iter().position(|(m, e)| *m == model && *e == effort)
        .unwrap_or(3); // default to sonnet:medium
    let new_idx = (current_idx + bumps).min(ladder.len() - 1);
    (ladder[new_idx].0.to_string(), ladder[new_idx].1.to_string())
}

/// Validate risk class patterns (called at YAML load time).
pub fn validate_risk_class_patterns(rules: &[RiskClassRule]) -> Result<()> {
    for rule in rules {
        for pat in &rule.patterns {
            if glob::Pattern::new(pat).is_err() {
                bail!(
                    "compliance.risk_classes: Class {} has invalid glob pattern '{}'",
                    rule.class.as_str(), pat
                );
            }
        }
    }
    Ok(())
}

/// Log a warning when requirement files don't exist on disk.
pub fn warn_orphan_requirement_files(requirements: &[Requirement], project_path: &std::path::Path) {
    for req in requirements {
        let any_exists = req.files.iter().any(|pat| {
            // For glob patterns, check if any file matches
            glob::glob(&project_path.join(pat).display().to_string())
                .ok()
                .and_then(|mut iter| iter.next())
                .is_some()
        });
        if !any_exists && !req.files.is_empty() {
            warn!(
                "compliance.requirements: requirement '{}' — no files matched any pattern on disk: {:?}",
                req.id, req.files
            );
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(rules: Vec<RiskClassRule>) -> ComplianceConfig {
        ComplianceConfig {
            enabled: true,
            risk_classes: rules,
            default_risk_class: RiskClass::A,
            requirements: vec![],
            standards: vec![],
            fail_on_violation: false,
            traceability_dir: None,
            include_risk_class_column: false,
        }
    }

    #[test]
    fn test_risk_class_ordering() {
        assert!(RiskClass::A < RiskClass::B);
        assert!(RiskClass::B < RiskClass::C);
        assert!(RiskClass::C > RiskClass::A);
    }

    #[test]
    fn test_resolve_risk_class_no_match_returns_default() {
        let config = make_config(vec![
            RiskClassRule {
                class: RiskClass::C,
                description: None,
                patterns: vec!["src/safety/**".to_string()],
                policy: RiskPolicy::default(),
            },
        ]);
        let class = resolve_risk_class("src/ui/Button.java", &config, true);
        assert_eq!(class, RiskClass::A);
    }

    #[test]
    fn test_resolve_risk_class_matches_pattern() {
        let config = make_config(vec![
            RiskClassRule {
                class: RiskClass::C,
                description: None,
                patterns: vec!["src/safety/**".to_string()],
                policy: RiskPolicy::default(),
            },
            RiskClassRule {
                class: RiskClass::B,
                description: None,
                patterns: vec!["src/service/**".to_string()],
                policy: RiskPolicy::default(),
            },
        ]);
        assert_eq!(resolve_risk_class("src/safety/DoseCalculator.java", &config, true), RiskClass::C);
        assert_eq!(resolve_risk_class("src/service/OrderService.java", &config, true), RiskClass::B);
        assert_eq!(resolve_risk_class("src/ui/Button.java", &config, true), RiskClass::A);
    }

    #[test]
    fn test_resolve_risk_class_most_restrictive_wins() {
        // File matches both B and C patterns — C should win
        let config = make_config(vec![
            RiskClassRule {
                class: RiskClass::C,
                description: None,
                patterns: vec!["src/**".to_string()],
                policy: RiskPolicy::default(),
            },
            RiskClassRule {
                class: RiskClass::B,
                description: None,
                patterns: vec!["src/**".to_string()],
                policy: RiskPolicy::default(),
            },
        ]);
        assert_eq!(resolve_risk_class("src/service/OrderService.java", &config, true), RiskClass::C);
    }

    #[test]
    fn test_resolve_risk_class_health_mode_false_returns_default() {
        let config = make_config(vec![
            RiskClassRule {
                class: RiskClass::C,
                description: None,
                patterns: vec!["src/**".to_string()],
                policy: RiskPolicy::default(),
            },
        ]);
        // Even though pattern matches, health_mode=false → always return default (A)
        assert_eq!(resolve_risk_class("src/safety/DoseCalc.java", &config, false), RiskClass::A);
    }

    #[test]
    fn test_requirements_for_file_glob_match() {
        let config = ComplianceConfig {
            enabled: true,
            requirements: vec![
                Requirement {
                    id: "REQ-001".to_string(),
                    description: "Test requirement".to_string(),
                    risk_class: RiskClass::C,
                    source: None,
                    risk_control: None,
                    files: vec!["src/safety/**".to_string()],
                    acceptance_criteria: None,
                    verification: None,
                    verified_by: None,
                },
                Requirement {
                    id: "REQ-002".to_string(),
                    description: "Auth requirement".to_string(),
                    risk_class: RiskClass::B,
                    source: None,
                    risk_control: None,
                    files: vec!["src/auth/**".to_string()],
                    acceptance_criteria: None,
                    verification: None,
                    verified_by: None,
                },
            ],
            ..Default::default()
        };
        let reqs = requirements_for_file("src/safety/DoseCalc.java", &config);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].id, "REQ-001");

        let reqs2 = requirements_for_file("src/ui/Button.java", &config);
        assert!(reqs2.is_empty());
    }

    #[test]
    fn test_resolve_effective_risk_class_req_override() {
        // File is Class A by pattern, but REQ-001 is Class C → should be C
        let config = ComplianceConfig {
            enabled: true,
            risk_classes: vec![],
            default_risk_class: RiskClass::A,
            requirements: vec![
                Requirement {
                    id: "REQ-001".to_string(),
                    description: "Safety req".to_string(),
                    risk_class: RiskClass::C,
                    source: None,
                    risk_control: None,
                    files: vec!["src/service/**".to_string()],
                    acceptance_criteria: None,
                    verification: None,
                    verified_by: None,
                },
            ],
            standards: vec![],
            fail_on_violation: false,
            traceability_dir: None,
            include_risk_class_column: true,
        };
        let class = resolve_effective_risk_class("src/service/OrderService.java", &config, true);
        assert_eq!(class, RiskClass::C);
    }

    #[test]
    fn test_validate_requirements_duplicate_ids() {
        let reqs = vec![
            Requirement {
                id: "REQ-001".to_string(),
                description: "First".to_string(),
                risk_class: RiskClass::A,
                source: None,
                risk_control: None,
                files: vec!["src/**".to_string()],
                acceptance_criteria: None,
                verification: None,
                verified_by: None,
            },
            Requirement {
                id: "REQ-001".to_string(),
                description: "Duplicate".to_string(),
                risk_class: RiskClass::A,
                source: None,
                risk_control: None,
                files: vec!["src/**".to_string()],
                acceptance_criteria: None,
                verification: None,
                verified_by: None,
            },
        ];
        assert!(validate_requirements(&reqs).is_err());
    }

    #[test]
    fn test_validate_requirements_empty_files() {
        let reqs = vec![
            Requirement {
                id: "REQ-001".to_string(),
                description: "No files".to_string(),
                risk_class: RiskClass::A,
                source: None,
                risk_control: None,
                files: vec![],
                acceptance_criteria: None,
                verification: None,
                verified_by: None,
            },
        ];
        assert!(validate_requirements(&reqs).is_err());
    }

    #[test]
    fn test_validate_requirements_manual_no_files_ok() {
        // Manual verification requirements don't need files
        let reqs = vec![
            Requirement {
                id: "REQ-PROC-001".to_string(),
                description: "Code review".to_string(),
                risk_class: RiskClass::A,
                source: None,
                risk_control: None,
                files: vec![],
                acceptance_criteria: None,
                verification: Some("manual".to_string()),
                verified_by: Some("PR template".to_string()),
            },
        ];
        assert!(validate_requirements(&reqs).is_ok());
    }

    #[test]
    fn test_bump_tier_class_a_no_change() {
        let (m, e) = bump_tier_for_risk_class("sonnet", "medium", RiskClass::A);
        assert_eq!(m, "sonnet");
        assert_eq!(e, "medium");
    }

    #[test]
    fn test_bump_tier_class_b_elevates_one() {
        let (m, e) = bump_tier_for_risk_class("sonnet", "medium", RiskClass::B);
        assert_eq!(m, "sonnet");
        assert_eq!(e, "high");
    }

    #[test]
    fn test_bump_tier_class_c_elevates_two() {
        let (m, e) = bump_tier_for_risk_class("sonnet", "medium", RiskClass::C);
        assert_eq!(m, "opus");
        assert_eq!(e, "high");
    }

    #[test]
    fn test_bump_tier_class_c_minimum_sonnet_high() {
        // haiku:low is at ladder index 0. Class C bumps by 2: index 0+2 = index 2 = sonnet:low
        // ladder: [(haiku,low),(haiku,medium),(sonnet,low),(sonnet,medium),(sonnet,high),(opus,high),(opus,max)]
        let (m, e) = bump_tier_for_risk_class("haiku", "low", RiskClass::C);
        assert_eq!(m, "sonnet");
        assert_eq!(e, "low");
    }

    #[test]
    fn test_safety_classification_section_class_c() {
        let section = build_safety_classification_section(RiskClass::C, true);
        assert!(section.contains("Class C"));
        assert!(section.contains("IEC 62304"));
        assert!(section.contains("MC/DC"));
    }

    #[test]
    fn test_safety_classification_section_health_mode_off() {
        let section = build_safety_classification_section(RiskClass::C, false);
        assert!(section.is_empty());
    }

    #[test]
    fn test_requirements_section_with_reqs() {
        let req = Requirement {
            id: "REQ-DOSE-001".to_string(),
            description: "Insulin bolus cap".to_string(),
            risk_class: RiskClass::C,
            source: Some("SRS v1.3".to_string()),
            risk_control: None,
            files: vec!["src/safety/**".to_string()],
            acceptance_criteria: Some("- bolus > 20 → ERR".to_string()),
            verification: None,
            verified_by: None,
        };
        let section = build_requirements_section(&[&req], true);
        assert!(section.contains("REQ-DOSE-001"));
        assert!(section.contains("Insulin bolus cap"));
        assert!(section.contains("bolus > 20"));
    }
}
