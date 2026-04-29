//! Issue pre-processing: overlap dedup + same-file/same-rule grouping.
//!
//! Both phases exist to cut redundant AI work before the fix loop starts.
//!
//! - `dedup_overlapping` (C2): within the same `(component, rule)`, drop issues
//!   whose `text_range` is fully contained in another's. Sonar sometimes reports
//!   nested findings (e.g. cognitive complexity) that collapse to the same fix.
//!
//! - `group_issues` (A3): bucket surviving issues by `(component, rule)` so the
//!   orchestrator can dispatch one AI call per bucket instead of one per issue.
//!   A bucket of 1 degrades to the legacy single-issue path cleanly.

use crate::sonar::{Issue, TextRange};

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields are read via Debug + future batched-prompt work.
pub struct IssueGroup {
    /// Sonar component (or linter-synthesized component) — the file identity.
    pub component: String,
    /// Rule key, e.g. `java:S1481` or `lint:clippy:unused_imports`.
    pub rule: String,
    /// Highest severity found in the group (by SonarQube ordering).
    pub severity: String,
    /// Member issues. Always non-empty; `len() == 1` for ungrouped issues.
    pub issues: Vec<Issue>,
}

impl IssueGroup {
    #[cfg(test)]
    pub fn primary(&self) -> &Issue {
        // Safe: groups are never empty.
        &self.issues[0]
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.issues.len()
    }

    pub fn is_batched(&self) -> bool {
        self.issues.len() > 1
    }

    /// Collapse a multi-issue group into a single representative `Issue` whose
    /// `message` enumerates every member's line range. This lets the existing
    /// fix loop process the group in a single AI call without any new plumbing:
    /// the AI sees one file, one rule, and a bulleted list of "fix here, here,
    /// and here".
    ///
    /// For single-issue groups, returns the member unchanged.
    ///
    /// The synthesized key uses a `batch:` prefix + the primary key so logs,
    /// commit messages, and the result table clearly show a batched fix.
    pub fn into_representative(self) -> Issue {
        if self.issues.len() == 1 {
            return self.issues.into_iter().next().unwrap();
        }
        let primary = self.issues[0].clone();
        let n = self.issues.len();
        let lines: String = self
            .issues
            .iter()
            .filter_map(|i| i.text_range.as_ref().map(|r| (r.start_line, r.end_line)))
            .map(|(s, e)| if s == e { format!("line {}", s) } else { format!("lines {}-{}", s, e) })
            .collect::<Vec<_>>()
            .join(", ");
        let augmented = format!(
            "{} (batched: {} occurrences of {} in this file — {})",
            primary.message, n, primary.rule, lines
        );
        let batched_key = format!("batch:{}:{}", n, primary.key);
        // Compute the widest enclosing range so downstream coverage checks /
        // prompt builders see the whole span.
        let min_start = self
            .issues
            .iter()
            .filter_map(|i| i.text_range.as_ref().map(|r| r.start_line))
            .min()
            .unwrap_or(0);
        let max_end = self
            .issues
            .iter()
            .filter_map(|i| i.text_range.as_ref().map(|r| r.end_line))
            .max()
            .unwrap_or(0);
        let merged_range = primary.text_range.as_ref().map(|r| TextRange {
            start_line: min_start,
            end_line: max_end,
            start_offset: r.start_offset,
            end_offset: r.end_offset,
        });
        Issue {
            key: batched_key,
            message: augmented,
            severity: self.severity,
            text_range: merged_range,
            ..primary
        }
    }
}

/// Drop issues whose text range is fully contained in another issue of the
/// same `(component, rule)`. Preserves the outer (larger-range) issue, which
/// is generally what Sonar considers the "root" finding.
pub fn dedup_overlapping(issues: Vec<Issue>) -> Vec<Issue> {
    // Bucket first so containment checks stay O(bucket²) instead of O(N²).
    use std::collections::HashMap;
    let mut buckets: HashMap<(String, String), Vec<Issue>> = HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    for issue in issues {
        let key = (issue.component.clone(), issue.rule.clone());
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(issue);
    }

    let mut out: Vec<Issue> = Vec::new();
    for key in order {
        let bucket = buckets.remove(&key).unwrap_or_default();
        // Sort by range size (largest first) so the first-seen "outer" range
        // wins the containment check.
        let mut sorted: Vec<Issue> = bucket;
        sorted.sort_by_key(|i| {
            i.text_range
                .as_ref()
                .map(|r| std::cmp::Reverse(r.end_line.saturating_sub(r.start_line)))
                .unwrap_or(std::cmp::Reverse(0))
        });
        let mut kept: Vec<Issue> = Vec::with_capacity(sorted.len());
        for issue in sorted {
            let contained = kept.iter().any(|k| range_contains(&k.text_range, &issue.text_range));
            if !contained {
                kept.push(issue);
            }
        }
        out.extend(kept);
    }
    out
}

fn range_contains(outer: &Option<TextRange>, inner: &Option<TextRange>) -> bool {
    let (Some(o), Some(i)) = (outer.as_ref(), inner.as_ref()) else {
        return false;
    };
    o.start_line <= i.start_line && o.end_line >= i.end_line
}

/// Bucket issues by `(component, rule)` so callers can fix N findings in a
/// single AI call. Order of groups preserves the input order (first-seen key
/// wins), so severity-sorted inputs produce severity-sorted groups.
///
/// Groups larger than `max_group_size` are split into chunks of that size to
/// keep the batched prompt manageable.
pub fn group_issues(issues: Vec<Issue>, max_group_size: usize) -> Vec<IssueGroup> {
    use std::collections::HashMap;
    let cap = if max_group_size == 0 { usize::MAX } else { max_group_size };

    let mut buckets: HashMap<(String, String), Vec<Issue>> = HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    for issue in issues {
        let key = (issue.component.clone(), issue.rule.clone());
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(issue);
    }

    let mut out: Vec<IssueGroup> = Vec::new();
    for key in order {
        let bucket = buckets.remove(&key).unwrap_or_default();
        if bucket.is_empty() {
            continue;
        }
        for chunk in bucket.chunks(cap) {
            let severity = max_severity(chunk);
            let component = chunk[0].component.clone();
            let rule = chunk[0].rule.clone();
            out.push(IssueGroup {
                component,
                rule,
                severity,
                issues: chunk.to_vec(),
            });
        }
    }
    out
}

fn severity_rank(sev: &str) -> u8 {
    match sev.to_uppercase().as_str() {
        "BLOCKER" => 0,
        "CRITICAL" => 1,
        "MAJOR" => 2,
        "MINOR" => 3,
        "INFO" => 4,
        _ => 5,
    }
}

fn max_severity(issues: &[Issue]) -> String {
    issues
        .iter()
        .min_by_key(|i| severity_rank(&i.severity))
        .map(|i| i.severity.clone())
        .unwrap_or_else(|| "MAJOR".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(key: &str, component: &str, rule: &str, severity: &str, start: u32, end: u32) -> Issue {
        Issue {
            key: key.to_string(),
            rule: rule.to_string(),
            severity: severity.to_string(),
            component: component.to_string(),
            issue_type: "CODE_SMELL".to_string(),
            message: String::new(),
            text_range: Some(TextRange {
                start_line: start,
                end_line: end,
                start_offset: None,
                end_offset: None,
            }),
            status: "OPEN".to_string(),
            tags: vec![],
            effort: None,
        }
    }

    #[test]
    fn dedup_drops_contained_ranges() {
        let issues = vec![
            mk("A", "proj:F.java", "java:S3776", "MAJOR", 10, 40),
            mk("B", "proj:F.java", "java:S3776", "MAJOR", 15, 25), // contained in A
            mk("C", "proj:F.java", "java:S3776", "MAJOR", 50, 60), // separate
        ];
        let out = dedup_overlapping(issues);
        let keys: Vec<_> = out.iter().map(|i| i.key.as_str()).collect();
        assert!(keys.contains(&"A"));
        assert!(!keys.contains(&"B"));
        assert!(keys.contains(&"C"));
    }

    #[test]
    fn dedup_preserves_different_rules() {
        let issues = vec![
            mk("A", "proj:F.java", "java:S1481", "MINOR", 10, 10),
            mk("B", "proj:F.java", "java:S3776", "MAJOR", 10, 10),
        ];
        let out = dedup_overlapping(issues);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn group_buckets_by_file_and_rule() {
        let issues = vec![
            mk("A", "proj:F.java", "java:S1481", "MINOR", 5, 5),
            mk("B", "proj:F.java", "java:S1481", "MINOR", 10, 10),
            mk("C", "proj:F.java", "java:S1481", "MINOR", 15, 15),
            mk("D", "proj:G.java", "java:S1481", "MINOR", 5, 5),
        ];
        let groups = group_issues(issues, 20);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 3);
        assert_eq!(groups[1].len(), 1);
    }

    #[test]
    fn group_splits_oversized_buckets() {
        let issues: Vec<Issue> = (0..25)
            .map(|n| mk(&format!("K{}", n), "proj:F.java", "java:S1481", "MINOR", n, n))
            .collect();
        let groups = group_issues(issues, 10);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].len(), 10);
        assert_eq!(groups[1].len(), 10);
        assert_eq!(groups[2].len(), 5);
    }

    #[test]
    fn group_promotes_max_severity() {
        let issues = vec![
            mk("A", "proj:F.java", "java:S1481", "MINOR", 5, 5),
            mk("B", "proj:F.java", "java:S1481", "BLOCKER", 10, 10),
        ];
        let groups = group_issues(issues, 20);
        assert_eq!(groups[0].severity, "BLOCKER");
    }

    #[test]
    fn group_singletons_degrade_cleanly() {
        let issues = vec![mk("A", "proj:F.java", "java:S1481", "MINOR", 5, 5)];
        let groups = group_issues(issues, 20);
        assert_eq!(groups.len(), 1);
        assert!(!groups[0].is_batched());
    }
}
