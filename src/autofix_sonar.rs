//! Deterministic (no-AI) autofix for SonarQube rules via OpenRewrite.
//!
//! SonarQube queues are dominated by a small set of mechanical rules
//! (unused imports, missing @Override, modifier order, etc.) that map 1:1
//! to existing OpenRewrite recipes. One `mvn rewrite:run` invocation
//! resolves hundreds of findings in ~60s — orders of magnitude cheaper
//! than one Claude call per finding.
//!
//! Flow:
//! 1. Scan the queued issues for rules in `RULE_TO_RECIPE`.
//! 2. Collect the union of recipes + their dependency artifacts.
//! 3. Run the rewrite plugin with full GAV (no project-side pom edits).
//! 4. Touched files become evidence that the corresponding-rule issues
//!    in those files were mechanically fixed. They skip the AI loop.
//!
//! Non-matching rules flow through the existing AI path unchanged.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::runner;
use crate::sonar::{self, Issue};

/// Version pin for the Maven rewrite plugin. Kept current but conservative;
/// bump in tandem with tested recipe versions below.
const REWRITE_PLUGIN_GAV: &str = "org.openrewrite.maven:rewrite-maven-plugin:5.46.0";

/// OpenRewrite recipe descriptor. `artifact` is the GAV coordinate that ships
/// the recipe; we collect the union for the plugin's `recipeArtifactCoordinates`.
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    pub name: &'static str,
    pub artifact: &'static str,
}

/// Sonar rule key → one-or-more OpenRewrite recipes that implement the fix.
///
/// Entries chosen conservatively: only recipes whose semantics match the
/// Sonar rule cleanly. When in doubt, the rule is NOT listed and the AI
/// path handles it.
///
/// Extend freely as you validate more recipes against real Sonar output.
pub const RULE_TO_RECIPES: &[(&str, &[Recipe])] = &[
    // java:S1128 — Remove unused imports.
    (
        "java:S1128",
        &[Recipe {
            name: "org.openrewrite.java.RemoveUnusedImports",
            artifact: "org.openrewrite.recipe:rewrite-java-dependencies:RELEASE",
        }],
    ),
    // java:S1481 — Remove unused local variables.
    (
        "java:S1481",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.RemoveUnusedLocalVariables",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1068 — Remove unused private fields.
    (
        "java:S1068",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.RemoveUnusedPrivateFields",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1161 — Missing @Override.
    (
        "java:S1161",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.MissingOverrideAnnotation",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1118 — Add private constructor to hide implicit public one (utility class).
    (
        "java:S1118",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.HideUtilityClassConstructor",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1124 — Reorder modifiers (e.g. `static final` ordering).
    (
        "java:S1124",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.ModifierOrder",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S2293 — Replace type spec with diamond operator.
    (
        "java:S2293",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.UseDiamondOperator",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1155 — Use isEmpty() instead of size()==0.
    (
        "java:S1155",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.UseCollectionIsEmpty",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1488 — Return the expression directly instead of assigning to a local var.
    (
        "java:S1488",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.InlineVariable",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1066 — Merge nested `if` with its parent.
    (
        "java:S1066",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.CombineSemanticallyEqualCatchBlocks",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1125 — Remove unnecessary boolean literal from conditions.
    (
        "java:S1125",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.SimplifyBooleanExpression",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1126 — Replace `if/else` that returns a boolean with a single return.
    (
        "java:S1126",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.SimplifyBooleanReturn",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1905 — Remove unnecessary cast.
    (
        "java:S1905",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.UnnecessaryExplicitTypeArguments",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S6201 — Pattern matching for instanceof (Java 16+).
    (
        "java:S6201",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.InstanceOfPatternMatch",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S1165 — Make exception fields final.
    (
        "java:S1165",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.FinalizePrivateFields",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
    // java:S2864 — Iterate over entrySet instead of keySet+get.
    (
        "java:S2864",
        &[Recipe {
            name: "org.openrewrite.staticanalysis.UseMapEntrySetIterator",
            artifact: "org.openrewrite.recipe:rewrite-static-analysis:RELEASE",
        }],
    ),
];

/// Returns Some(recipes) if this rule is autofix-eligible, else None.
fn recipes_for(rule: &str) -> Option<&'static [Recipe]> {
    RULE_TO_RECIPES.iter().find(|(k, _)| *k == rule).map(|(_, v)| *v)
}

/// Result of one autofix pass.
#[allow(dead_code)] // `changed_files` currently consumed only for logs.
pub struct AutofixOutcome {
    /// Issue keys deemed resolved by the run (their rule was activated AND their file changed).
    pub resolved_keys: Vec<String>,
    /// Relative paths modified by OpenRewrite.
    pub changed_files: Vec<String>,
    /// Ordered list of recipes that were activated (for logging / commit msg).
    pub activated_recipes: Vec<String>,
}

/// Run OpenRewrite against the project, activating the subset of recipes
/// that match rules present in `issues`. Returns the outcome or an error
/// if the plugin invocation itself failed catastrophically.
///
/// A "no matching recipes" situation returns `Ok` with all-empty fields —
/// the orchestrator treats that as a no-op.
pub fn run(project_path: &Path, issues: &[Issue]) -> Result<AutofixOutcome> {
    let mut recipe_names: Vec<&str> = Vec::new();
    let mut artifacts: HashSet<&str> = HashSet::new();
    let mut eligible_rules: HashSet<&str> = HashSet::new();
    let mut seen_recipe: HashSet<&str> = HashSet::new();

    for issue in issues {
        if let Some(recs) = recipes_for(&issue.rule) {
            eligible_rules.insert(issue.rule.as_str());
            for r in recs {
                if seen_recipe.insert(r.name) {
                    recipe_names.push(r.name);
                }
                artifacts.insert(r.artifact);
            }
        }
    }

    if recipe_names.is_empty() {
        info!("autofix-sonar: no queued rules match the OpenRewrite recipe map — skipping");
        return Ok(AutofixOutcome {
            resolved_keys: vec![],
            changed_files: vec![],
            activated_recipes: vec![],
        });
    }

    info!(
        "autofix-sonar: {} eligible rules → activating {} OpenRewrite recipe(s)",
        eligible_rules.len(),
        recipe_names.len()
    );

    let mvn = runner::mvn_binary();
    // Full GAV form: plugin is resolved from Maven Central without touching pom.xml.
    // `recipeArtifactCoordinates` puts the recipe JARs on the plugin classpath
    // at runtime, keeping this a zero-project-config feature.
    let cmd = format!(
        "{} {}:run -Drewrite.activeRecipes={} -Drewrite.recipeArtifactCoordinates={} -Drewrite.failOnInvalidActiveRecipes=false",
        mvn,
        REWRITE_PLUGIN_GAV,
        recipe_names.join(","),
        artifacts.iter().copied().collect::<Vec<_>>().join(","),
    );

    info!("autofix-sonar: running {}", truncate(&cmd, 300));
    let (ok, output) = runner::run_shell_command(project_path, &cmd, "autofix-sonar")
        .context("autofix-sonar: failed to spawn mvn rewrite:run")?;

    if !ok {
        warn!(
            "autofix-sonar: mvn rewrite:run returned non-zero — continuing with AI path. Output tail:\n{}",
            truncate_tail(&output, 600)
        );
        return Ok(AutofixOutcome {
            resolved_keys: vec![],
            changed_files: vec![],
            activated_recipes: recipe_names.iter().map(|s| s.to_string()).collect(),
        });
    }

    // Which files did OpenRewrite actually touch? This is the ground truth
    // — if it didn't modify a file we thought was eligible, the finding
    // wasn't really reachable by the recipe and should flow to AI.
    let changed_files = crate::git::changed_files(project_path).unwrap_or_default();
    let changed_set: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();

    // Mark an issue resolved iff:
    //   - its rule is in the eligible set, AND
    //   - its component maps to a file OpenRewrite touched.
    let mut resolved_keys: Vec<String> = Vec::new();
    let mut per_rule_counts: HashMap<&str, usize> = HashMap::new();
    for issue in issues {
        if !eligible_rules.contains(issue.rule.as_str()) {
            continue;
        }
        let file = sonar::component_to_path(&issue.component);
        if changed_set.contains(file.as_str()) {
            resolved_keys.push(issue.key.clone());
            *per_rule_counts.entry(issue.rule.as_str()).or_insert(0) += 1;
        }
    }

    if resolved_keys.is_empty() {
        info!(
            "autofix-sonar: plugin run succeeded but produced no file changes — all findings continue to the AI path"
        );
    } else {
        info!(
            "autofix-sonar: {} issue(s) resolved without AI across {} file(s)",
            resolved_keys.len(),
            changed_files.len()
        );
        for (rule, count) in per_rule_counts.iter() {
            info!("  {} — {} fix(es)", rule, count);
        }
    }

    Ok(AutofixOutcome {
        resolved_keys,
        changed_files,
        activated_recipes: recipe_names.iter().map(|s| s.to_string()).collect(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{}…", t)
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let skip = count - max;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_lookup_hits_known_rule() {
        let r = recipes_for("java:S1128").expect("S1128 should be mapped");
        assert!(r.iter().any(|x| x.name.contains("RemoveUnusedImports")));
    }

    #[test]
    fn recipe_lookup_misses_unknown_rule() {
        assert!(recipes_for("java:SXXXX").is_none());
    }

    #[test]
    fn recipe_map_entries_are_well_formed() {
        for (rule, recipes) in RULE_TO_RECIPES {
            assert!(rule.starts_with("java:S"), "rule key must be java:S*: {}", rule);
            assert!(!recipes.is_empty(), "rule {} has empty recipe list", rule);
            for r in *recipes {
                assert!(r.name.starts_with("org.openrewrite."), "recipe name looks wrong: {}", r.name);
                assert!(r.artifact.contains(':'), "artifact GAV looks wrong: {}", r.artifact);
            }
        }
    }
}
