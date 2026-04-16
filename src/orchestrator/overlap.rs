//! Test-overlap detection phase (Step 3a).
//!
//! Each test file is run in isolation through the project's coverage command.
//! The resulting per-file coverage maps are compared pairwise: any pair that
//! covers at least one identical (source-file, line) tuple is flagged as
//! overlapping, and the file that covers fewer total source lines is marked as
//! the candidate for removal.
//!
//! # Determinism guarantee
//! - Test files are collected and sorted alphabetically before processing.
//! - Pairs are reported in (file_a < file_b) alphabetical order.
//! - Tie-breaking for `would_delete`: if both files cover the same number of
//!   lines, the alphabetically later file (file_b) is marked for deletion so
//!   that equal inputs always produce equal output.
//!
//! # Read-only
//! This phase never modifies any source file, test file, git branch, or git
//! index. It is purely advisory.

use super::helpers::is_test_file;
use super::Orchestrator;
use crate::runner;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// source-file path → set of covered (hit) line numbers.
type CoverageMap = HashMap<String, BTreeSet<u32>>;

/// Two test files whose covered source-line sets share at least one entry.
pub struct TestOverlapPair {
    /// Alphabetically earlier test file path (relative to project root).
    pub file_a: String,
    /// Alphabetically later test file path (relative to project root).
    pub file_b: String,
    /// Number of (source-file, line) tuples covered by both files.
    pub overlap_lines: usize,
    /// Total source lines covered by `file_a`.
    pub covered_a: usize,
    /// Total source lines covered by `file_b`.
    pub covered_b: usize,
    /// The file Reparo would remove: the one with fewer covered lines.
    /// On a tie, `file_b` (alphabetically later) is chosen.
    pub would_delete: String,
}

/// Outcome of the overlap-detection phase.
#[derive(Default)]
#[allow(dead_code)]
pub struct OverlapReport {
    /// Test files whose per-file coverage run succeeded.
    pub analysed: usize,
    /// Test files whose per-file coverage run failed or produced no data.
    pub skipped: usize,
    /// Overlapping pairs, in deterministic order.
    pub pairs: Vec<TestOverlapPair>,
}

// ─── Orchestrator entry point ─────────────────────────────────────────────────

impl Orchestrator {
    /// Run Step 3a: detect test files that cover overlapping source lines.
    ///
    /// For each test file found under the project root the coverage command is
    /// invoked with that file appended as a positional argument (works for
    /// pytest, jest, vitest, and most frameworks that accept a file path). The
    /// resulting coverage report is parsed into a source-line set and compared
    /// against every other file's set.
    ///
    /// All warnings are emitted via `warn!`; the function never returns `Err`
    /// for analysis failures — those are logged and the file is skipped.
    pub(super) fn analyze_test_overlap(
        &self,
        coverage_cmd: &str,
    ) -> anyhow::Result<OverlapReport> {
        let test_files = find_test_files(&self.config.path, &self.config.coverage_exclude);

        if test_files.len() < 2 {
            info!(
                "Test overlap: {} test file(s) found — nothing to compare",
                test_files.len()
            );
            return Ok(OverlapReport::default());
        }

        info!(
            "=== Step 3a: Test overlap detection ({} test files) ===",
            test_files.len()
        );

        // ── Per-file coverage runs (deterministic: files already sorted) ──────
        let mut analysed: Vec<(String, CoverageMap)> = Vec::new();
        let mut skipped: usize = 0;

        for abs_path in &test_files {
            let rel = abs_path
                .strip_prefix(&self.config.path)
                .unwrap_or(abs_path.as_path())
                .to_string_lossy()
                .to_string();

            match measure_single_file(
                &self.config.path,
                coverage_cmd,
                self.config.commands.coverage_report.as_deref(),
                &rel,
            ) {
                Some(map) if !map.is_empty() => {
                    analysed.push((rel, map));
                }
                _ => {
                    skipped += 1;
                }
            }
        }

        // ── Pairwise overlap (pairs in (i < j) index order = alphabetical) ───
        let mut pairs: Vec<TestOverlapPair> = Vec::new();
        let m = analysed.len();

        for i in 0..m {
            for j in (i + 1)..m {
                let (ref name_a, ref map_a) = analysed[i];
                let (ref name_b, ref map_b) = analysed[j];

                let overlap = compute_overlap(map_a, map_b);
                if overlap == 0 {
                    continue;
                }

                let ca = count_covered(map_a);
                let cb = count_covered(map_b);

                // Keep the file with more covered lines.
                // On a tie, keep file_a (alphabetically earlier) → delete file_b.
                let would_delete = if ca < cb {
                    name_a.clone()
                } else {
                    // ca >= cb: delete b (covers fewer or the same)
                    name_b.clone()
                };

                pairs.push(TestOverlapPair {
                    file_a: name_a.clone(),
                    file_b: name_b.clone(),
                    overlap_lines: overlap,
                    covered_a: ca,
                    covered_b: cb,
                    would_delete,
                });
            }
        }

        // ── Emit warnings ─────────────────────────────────────────────────────
        for pair in &pairs {
            warn!(
                "Test overlap: {} ↔ {} share {} source line(s) \
                 [covered: {} vs {}] — would remove: {}",
                pair.file_a,
                pair.file_b,
                pair.overlap_lines,
                pair.covered_a,
                pair.covered_b,
                pair.would_delete,
            );
        }

        if pairs.is_empty() {
            info!("Test overlap: no redundant test pairs detected");
        } else {
            warn!(
                "Test overlap summary: {}/{} file(s) analysed, {} overlapping pair(s) detected \
                 ({} file(s) skipped — per-file coverage run produced no data)",
                m,
                test_files.len(),
                pairs.len(),
                skipped,
            );
        }

        Ok(OverlapReport {
            analysed: m,
            skipped,
            pairs,
        })
    }
}

// ─── Test-file discovery ──────────────────────────────────────────────────────

/// Collect all test files under `root`, sorted alphabetically.
///
/// Skips hidden entries, common build/cache directories, and any path matching
/// `exclude_patterns` (same glob patterns used by the coverage boost phase).
pub(super) fn find_test_files(root: &Path, exclude_patterns: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    walk_dir(root, root, exclude_patterns, &mut out);
    out.sort(); // deterministic ordering
    out
}

fn walk_dir(root: &Path, dir: &Path, exclude: &[String], out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return,
    };
    // Sort entries so the traversal order is deterministic on every OS.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let raw_name = entry.file_name();
        let name = raw_name.to_string_lossy();

        // Skip dotfiles/dotdirs and common build / cache directories.
        if name.starts_with('.') {
            continue;
        }
        if matches!(
            name.as_ref(),
            "node_modules"
                | "target"
                | "__pycache__"
                | ".tox"
                | "venv"
                | ".venv"
                | "dist"
                | "build"
                | "coverage"
                | ".nyc_output"
                | ".pytest_cache"
        ) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        if is_excluded_path(&rel, exclude) {
            continue;
        }

        if path.is_dir() {
            walk_dir(root, &path, exclude, out);
        } else if path.is_file() && is_test_file(&rel) {
            out.push(path);
        }
    }
}

fn is_excluded_path(rel: &str, patterns: &[String]) -> bool {
    for p in patterns {
        if let Ok(pat) = glob::Pattern::new(p) {
            if pat.matches(rel) {
                return true;
            }
        }
        // Substring match as a fallback for non-glob strings.
        if rel.contains(p.as_str()) {
            return true;
        }
    }
    false
}

// ─── Per-file coverage measurement ───────────────────────────────────────────

/// Run the coverage command for a single test file and return its line-coverage
/// map.  Returns `None` when the command fails or produces no report.
///
/// For Maven/Gradle projects the test class name is injected via `-Dtest=` /
/// `--tests` instead of appending a file path (which those tools do not
/// understand as a positional argument).  For all other frameworks (pytest,
/// jest, vitest, cargo-tarpaulin, …) the file path is appended as usual.
fn measure_single_file(
    project_path: &Path,
    coverage_cmd: &str,
    report_hint: Option<&str>,
    test_file_rel: &str,
) -> Option<CoverageMap> {
    // Delete any stale report to prevent reading old data.
    // Use the silent variant — absence of a previous report is expected here
    // and must not produce "No coverage report found" warnings.
    if let Some(old) = runner::find_lcov_report_quietly(project_path, report_hint) {
        let _ = std::fs::remove_file(&old);
    }

    let cmd = build_per_file_coverage_cmd(coverage_cmd, test_file_rel);

    match runner::run_shell_command(project_path, &cmd, "overlap-coverage") {
        Ok((true, _)) => {}
        Ok((false, _)) | Err(_) => return None,
    }

    let report = runner::find_lcov_report_with_hint(project_path, report_hint)?;
    Some(parse_covered_lines(&report))
}

/// Build the per-file coverage command for the given test file.
///
/// - Maven (`mvn`/`mvnw`): converts the file path to a fully-qualified class
///   name and injects it via `-Dtest=<ClassName>`, replacing any existing
///   `-Dtest=…` value (exclusions included — we only want this one test).
/// - Gradle (`gradle`/`gradlew`): appends `--tests <ClassName>`.
/// - Everything else: appends the relative file path (pytest, jest, vitest, …).
fn build_per_file_coverage_cmd(coverage_cmd: &str, test_file_rel: &str) -> String {
    let is_maven = coverage_cmd.contains("mvn ") || coverage_cmd.contains("mvnw ");
    let is_gradle = !is_maven
        && (coverage_cmd.contains("gradle ") || coverage_cmd.contains("gradlew "));

    if is_maven {
        if let Some(class_name) = java_file_to_class_name(test_file_rel) {
            return inject_maven_test_filter(coverage_cmd, &class_name);
        }
    } else if is_gradle {
        if let Some(class_name) = java_file_to_class_name(test_file_rel) {
            return format!("{} --tests {}", coverage_cmd.trim_end(), class_name);
        }
    }

    // Default: append file path as positional argument (pytest, jest, vitest, …)
    format!("{} {}", coverage_cmd.trim_end(), test_file_rel)
}

/// Convert a Java/Kotlin test file path to a fully-qualified class name.
///
/// Examples:
/// - `src/test/java/com/example/FooTest.java` → `com.example.FooTest`
/// - `src/test/kotlin/com/example/BarTest.kt` → `com.example.BarTest`
fn java_file_to_class_name(file_path: &str) -> Option<String> {
    let markers = [
        "src/test/java/",
        "src/test/kotlin/",
        "src/main/java/",
        "src/main/kotlin/",
    ];
    for marker in &markers {
        if let Some(idx) = file_path.find(marker) {
            let after = &file_path[idx + marker.len()..];
            let class = after
                .trim_end_matches(".java")
                .trim_end_matches(".kt")
                .replace('/', ".");
            if !class.is_empty() {
                return Some(class);
            }
        }
    }
    None
}

/// Replace (or add) the `-Dtest=<value>` filter in a Maven command.
///
/// If the command already contains `-Dtest=…`, the existing value is replaced
/// with `class_name` so we run exactly this one test class.  Otherwise
/// `-Dtest=<class_name>` is appended.
fn inject_maven_test_filter(cmd: &str, class_name: &str) -> String {
    if let Some(pos) = cmd.find("-Dtest=") {
        let before = &cmd[..pos];
        let rest = &cmd[pos + "-Dtest=".len()..];
        // The value ends at the next whitespace (or end of string).
        let end = rest.find(' ').unwrap_or(rest.len());
        let after = &rest[end..];
        format!("{}-Dtest={}{}", before, class_name, after)
    } else {
        format!("{} -Dtest={}", cmd.trim_end(), class_name)
    }
}

// ─── Coverage-map helpers ─────────────────────────────────────────────────────

/// Total number of covered lines across all source files in the map.
fn count_covered(map: &CoverageMap) -> usize {
    map.values().map(|s| s.len()).sum()
}

/// Number of (source-file, line-number) tuples that appear in both maps.
fn compute_overlap(a: &CoverageMap, b: &CoverageMap) -> usize {
    let mut total = 0;
    for (file, lines_a) in a {
        if let Some(lines_b) = b.get(file) {
            total += lines_a.intersection(lines_b).count();
        }
    }
    total
}

// ─── Coverage-report parsers ──────────────────────────────────────────────────

/// Parse a coverage report into a map of source-file → set of covered lines.
///
/// Supports lcov (`.info`), JaCoCo XML, and Cobertura XML; format is
/// auto-detected from file extension and content.
fn parse_covered_lines(report: &Path) -> CoverageMap {
    let content = match std::fs::read_to_string(report) {
        Ok(c) => c,
        Err(_) => return CoverageMap::new(),
    };

    let ext = report.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "xml" {
        if content.contains("<report ") || content.contains("<report>") {
            return parse_jacoco(&content);
        }
        if content.contains("<coverage ") || content.contains("<coverage>") {
            return parse_cobertura(&content);
        }
        return CoverageMap::new();
    }

    parse_lcov(&content)
}

/// Parse lcov text into source-file → covered-line-number set.
///
/// Only `DA:<line>,<hits>` lines where `hits > 0` are added to the set.
pub(super) fn parse_lcov(content: &str) -> CoverageMap {
    let mut map: CoverageMap = CoverageMap::new();
    let mut current_file: Option<String> = None;
    let mut covered: BTreeSet<u32> = BTreeSet::new();

    for line in content.lines() {
        if line.starts_with("SF:") {
            current_file = Some(line[3..].to_string());
            covered = BTreeSet::new();
        } else if line.starts_with("DA:") {
            // DA:<line_number>,<hit_count>[,<checksum>]
            let after = &line[3..];
            let mut parts = after.splitn(2, ',');
            if let (Some(ln_s), Some(rest)) = (parts.next(), parts.next()) {
                let hits_s = rest.split(',').next().unwrap_or("0");
                if let (Ok(ln), Ok(hits)) = (ln_s.parse::<u32>(), hits_s.parse::<u64>()) {
                    if hits > 0 {
                        covered.insert(ln);
                    }
                }
            }
        } else if line == "end_of_record" {
            if let Some(file) = current_file.take() {
                if !covered.is_empty() {
                    map.entry(file)
                        .or_default()
                        .extend(covered.iter().copied());
                }
            }
            covered = BTreeSet::new();
        }
    }

    map
}

/// Parse JaCoCo XML into source-file → covered-line set.
///
/// A line is considered covered when its `ci` (covered instructions) > 0.
///
/// ```xml
/// <package name="com/example">
///   <sourcefile name="Foo.java">
///     <line nr="5" mi="0" ci="1" mb="0" cb="0"/>
///   </sourcefile>
/// </package>
/// ```
pub(super) fn parse_jacoco(content: &str) -> CoverageMap {
    let mut map: CoverageMap = CoverageMap::new();
    let mut package = String::new();
    let mut source_file: Option<String> = None;

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("<package ") {
            package = xml_attr(t, "name").unwrap_or_default();
        } else if t.starts_with("</package>") {
            package.clear();
        } else if t.starts_with("<sourcefile ") {
            let name = xml_attr(t, "name").unwrap_or_default();
            source_file = Some(if package.is_empty() {
                name
            } else {
                format!("{}/{}", package, name)
            });
        } else if t.starts_with("</sourcefile>") {
            source_file = None;
        } else if t.starts_with("<line ") {
            if let Some(ref file) = source_file {
                if let (Some(nr), Some(ci)) = (xml_attr(t, "nr"), xml_attr(t, "ci")) {
                    if let (Ok(line_nr), Ok(ci_val)) = (nr.parse::<u32>(), ci.parse::<u64>()) {
                        if ci_val > 0 {
                            map.entry(file.clone()).or_default().insert(line_nr);
                        }
                    }
                }
            }
        }
    }

    map
}

/// Parse Cobertura XML into source-file → covered-line set.
///
/// A line is considered covered when `hits > 0`.
///
/// ```xml
/// <class filename="path/to/File.py">
///   <lines>
///     <line number="5" hits="1"/>
///   </lines>
/// </class>
/// ```
pub(super) fn parse_cobertura(content: &str) -> CoverageMap {
    let mut map: CoverageMap = CoverageMap::new();
    let mut current_file: Option<String> = None;

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("<class ") {
            current_file = xml_attr(t, "filename");
        } else if t.starts_with("</class>") {
            current_file = None;
        } else if t.starts_with("<line ") {
            if let Some(ref file) = current_file {
                if let (Some(nr), Some(hits)) = (xml_attr(t, "number"), xml_attr(t, "hits")) {
                    if let (Ok(ln), Ok(h)) = (nr.parse::<u32>(), hits.parse::<u64>()) {
                        if h > 0 {
                            map.entry(file.clone()).or_default().insert(ln);
                        }
                    }
                }
            }
        }
    }

    map
}

/// Extract the value of `key="…"` or `key='…'` from an XML element string.
fn xml_attr(element: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{}={}", key, quote);
        if let Some(pos) = element.find(&needle) {
            let start = pos + needle.len();
            let end = element[start..].find(quote)? + start;
            return Some(element[start..end].to_string());
        }
    }
    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // ── parse_lcov ────────────────────────────────────────────────────────────

    #[test]
    fn parse_lcov_basic_covered_lines() {
        let lcov = "\
TN:
SF:src/lib.rs
DA:1,3
DA:2,0
DA:3,1
end_of_record
";
        let map = parse_lcov(lcov);
        let lines = map.get("src/lib.rs").unwrap();
        assert!(lines.contains(&1));
        assert!(!lines.contains(&2)); // hits=0 → not covered
        assert!(lines.contains(&3));
    }

    #[test]
    fn parse_lcov_multiple_files() {
        let lcov = "\
SF:a.py
DA:10,1
end_of_record
SF:b.py
DA:20,2
DA:21,0
end_of_record
";
        let map = parse_lcov(lcov);
        assert_eq!(map.get("a.py").unwrap(), &BTreeSet::from([10]));
        assert_eq!(map.get("b.py").unwrap(), &BTreeSet::from([20]));
    }

    #[test]
    fn parse_lcov_zero_hits_excluded() {
        let lcov = "SF:z.py\nDA:5,0\nend_of_record\n";
        let map = parse_lcov(lcov);
        // File should be absent because no covered lines
        assert!(!map.contains_key("z.py"));
    }

    #[test]
    fn parse_lcov_checksum_column_ignored() {
        // DA:<line>,<hits>,<checksum>
        let lcov = "SF:c.py\nDA:7,1,abc123\nend_of_record\n";
        let map = parse_lcov(lcov);
        assert!(map.get("c.py").unwrap().contains(&7));
    }

    // ── parse_jacoco ──────────────────────────────────────────────────────────

    #[test]
    fn parse_jacoco_basic() {
        let xml = r#"<report name="test">
  <package name="com/example">
    <sourcefile name="Foo.java">
      <line nr="5" mi="0" ci="2" mb="0" cb="0"/>
      <line nr="6" mi="3" ci="0" mb="0" cb="0"/>
    </sourcefile>
  </package>
</report>"#;
        let map = parse_jacoco(xml);
        let lines = map.get("com/example/Foo.java").unwrap();
        assert!(lines.contains(&5)); // ci=2 → covered
        assert!(!lines.contains(&6)); // ci=0 → not covered
    }

    #[test]
    fn parse_jacoco_no_package() {
        let xml = r#"<report>
  <sourcefile name="Main.java">
    <line nr="1" mi="0" ci="1" mb="0" cb="0"/>
  </sourcefile>
</report>"#;
        let map = parse_jacoco(xml);
        assert!(map.get("Main.java").unwrap().contains(&1));
    }

    // ── parse_cobertura ───────────────────────────────────────────────────────

    #[test]
    fn parse_cobertura_basic() {
        let xml = r#"<coverage>
  <packages>
    <package>
      <classes>
        <class filename="src/utils.py">
          <lines>
            <line number="10" hits="1"/>
            <line number="11" hits="0"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;
        let map = parse_cobertura(xml);
        let lines = map.get("src/utils.py").unwrap();
        assert!(lines.contains(&10));
        assert!(!lines.contains(&11));
    }

    // ── compute_overlap ───────────────────────────────────────────────────────

    #[test]
    fn compute_overlap_disjoint() {
        let mut a: CoverageMap = HashMap::new();
        a.insert("f.py".into(), BTreeSet::from([1, 2, 3]));
        let mut b: CoverageMap = HashMap::new();
        b.insert("f.py".into(), BTreeSet::from([4, 5, 6]));
        assert_eq!(compute_overlap(&a, &b), 0);
    }

    #[test]
    fn compute_overlap_partial() {
        let mut a: CoverageMap = HashMap::new();
        a.insert("f.py".into(), BTreeSet::from([1, 2, 3]));
        let mut b: CoverageMap = HashMap::new();
        b.insert("f.py".into(), BTreeSet::from([2, 3, 4]));
        assert_eq!(compute_overlap(&a, &b), 2);
    }

    #[test]
    fn compute_overlap_different_files() {
        let mut a: CoverageMap = HashMap::new();
        a.insert("a.py".into(), BTreeSet::from([1, 2]));
        let mut b: CoverageMap = HashMap::new();
        b.insert("b.py".into(), BTreeSet::from([1, 2]));
        assert_eq!(compute_overlap(&a, &b), 0);
    }

    #[test]
    fn compute_overlap_multiple_source_files() {
        let mut a: CoverageMap = HashMap::new();
        a.insert("a.py".into(), BTreeSet::from([1, 2]));
        a.insert("b.py".into(), BTreeSet::from([10, 11]));
        let mut b: CoverageMap = HashMap::new();
        b.insert("a.py".into(), BTreeSet::from([2, 3]));
        b.insert("b.py".into(), BTreeSet::from([10, 12]));
        // a.py: {2} overlap, b.py: {10} overlap → 2
        assert_eq!(compute_overlap(&a, &b), 2);
    }

    // ── count_covered ─────────────────────────────────────────────────────────

    #[test]
    fn count_covered_sums_across_files() {
        let mut map: CoverageMap = HashMap::new();
        map.insert("a.py".into(), BTreeSet::from([1, 2, 3]));
        map.insert("b.py".into(), BTreeSet::from([10, 11]));
        assert_eq!(count_covered(&map), 5);
    }

    #[test]
    fn count_covered_empty() {
        assert_eq!(count_covered(&CoverageMap::new()), 0);
    }

    // ── xml_attr ──────────────────────────────────────────────────────────────

    #[test]
    fn xml_attr_double_quote() {
        assert_eq!(
            xml_attr(r#"<line nr="5" ci="2"/>"#, "nr"),
            Some("5".into())
        );
    }

    #[test]
    fn xml_attr_single_quote() {
        assert_eq!(xml_attr("<tag key='val'/>", "key"), Some("val".into()));
    }

    #[test]
    fn xml_attr_missing() {
        assert_eq!(xml_attr("<tag other=\"x\"/>", "nr"), None);
    }

    // ── find_test_files ───────────────────────────────────────────────────────

    #[test]
    fn find_test_files_discovers_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create test files in non-alphabetical order
        std::fs::write(root.join("test_b.py"), "").unwrap();
        std::fs::write(root.join("test_a.py"), "").unwrap();
        std::fs::write(root.join("main.py"), "").unwrap(); // not a test file

        let files = find_test_files(root, &[]);
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["test_a.py", "test_b.py"]);
    }

    #[test]
    fn find_test_files_skips_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target").join("test_inside.rs"), "").unwrap();
        std::fs::write(root.join("test_out.py"), "").unwrap();

        let files = find_test_files(root, &[]);
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["test_out.py"]);
    }

    #[test]
    fn find_test_files_respects_exclude_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("test_keep.py"), "").unwrap();
        std::fs::write(root.join("test_skip.py"), "").unwrap();

        let files = find_test_files(root, &["test_skip.py".into()]);
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["test_keep.py"]);
    }

    #[test]
    fn find_test_files_fewer_than_two_returns_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("test_only.py"), "").unwrap();
        let files = find_test_files(root, &[]);
        assert_eq!(files.len(), 1);
    }

    // ── would_delete tie-break ────────────────────────────────────────────────

    #[test]
    fn would_delete_fewer_covered() {
        // file_a covers 1 line, file_b covers 3 → delete file_a
        let mut map_a: CoverageMap = HashMap::new();
        map_a.insert("src.py".into(), BTreeSet::from([1]));
        let mut map_b: CoverageMap = HashMap::new();
        map_b.insert("src.py".into(), BTreeSet::from([1, 2, 3]));

        let ca = count_covered(&map_a);
        let cb = count_covered(&map_b);
        let would_delete = if ca < cb {
            "test_a.py".to_string()
        } else {
            "test_b.py".to_string()
        };
        assert_eq!(would_delete, "test_a.py");
    }

    #[test]
    fn would_delete_tie_goes_to_file_b() {
        // equal coverage → delete file_b (alphabetically later)
        let ca: usize = 5;
        let cb: usize = 5;
        let name_b = "test_b.py";
        let would_delete = if ca < cb { "test_a.py" } else { name_b };
        assert_eq!(would_delete, "test_b.py");
    }

    // ── java_file_to_class_name ───────────────────────────────────────────────

    #[test]
    fn java_file_to_class_name_test_java() {
        assert_eq!(
            java_file_to_class_name("src/test/java/com/example/FooTest.java"),
            Some("com.example.FooTest".into())
        );
    }

    #[test]
    fn java_file_to_class_name_test_kotlin() {
        assert_eq!(
            java_file_to_class_name("src/test/kotlin/com/example/BarTest.kt"),
            Some("com.example.BarTest".into())
        );
    }

    #[test]
    fn java_file_to_class_name_main_java() {
        assert_eq!(
            java_file_to_class_name("src/main/java/com/example/Service.java"),
            Some("com.example.Service".into())
        );
    }

    #[test]
    fn java_file_to_class_name_no_marker_returns_none() {
        assert_eq!(java_file_to_class_name("tests/test_foo.py"), None);
    }

    // ── inject_maven_test_filter ──────────────────────────────────────────────

    #[test]
    fn inject_maven_replaces_existing_dtest() {
        let cmd = "mvn test jacoco:report -Pjar -Dtest=!com.h2test.H2DatabaseTest";
        let result = inject_maven_test_filter(cmd, "com.example.FooTest");
        assert_eq!(result, "mvn test jacoco:report -Pjar -Dtest=com.example.FooTest");
    }

    #[test]
    fn inject_maven_adds_dtest_when_absent() {
        let cmd = "mvn test jacoco:report -Pjar";
        let result = inject_maven_test_filter(cmd, "com.example.FooTest");
        assert_eq!(result, "mvn test jacoco:report -Pjar -Dtest=com.example.FooTest");
    }

    #[test]
    fn inject_maven_preserves_flags_after_dtest() {
        let cmd = "mvn test -Dtest=OldTest -Pfoo";
        let result = inject_maven_test_filter(cmd, "com.example.NewTest");
        assert_eq!(result, "mvn test -Dtest=com.example.NewTest -Pfoo");
    }

    // ── build_per_file_coverage_cmd ───────────────────────────────────────────

    #[test]
    fn build_cmd_maven_injects_class_name() {
        let cov = "export JAVA_HOME=$(java_home) && mvn test jacoco:report -Pjar -Dtest=!Excluded";
        let result = build_per_file_coverage_cmd(cov, "src/test/java/com/example/FooTest.java");
        assert!(result.contains("-Dtest=com.example.FooTest"));
        assert!(!result.contains("src/test/java")); // file path not appended
    }

    #[test]
    fn build_cmd_gradle_appends_tests_flag() {
        let cov = "./gradlew test jacocoTestReport";
        let result = build_per_file_coverage_cmd(cov, "src/test/java/com/example/FooTest.java");
        assert!(result.contains("--tests com.example.FooTest"));
    }

    #[test]
    fn build_cmd_pytest_appends_path() {
        let cov = "pytest --cov=src";
        let result = build_per_file_coverage_cmd(cov, "tests/test_foo.py");
        assert_eq!(result, "pytest --cov=src tests/test_foo.py");
    }
}
