//! Checkstyle XML output parser.
//!
//! Handles output from `checkstyle -f xml` and the Maven Checkstyle plugin
//! (`mvn checkstyle:checkstyle` writes `target/checkstyle-result.xml`).
//!
//! The format is stable and line-oriented enough that a regex parser is more
//! reliable than pulling in a full XML crate — checkstyle emits one
//! `<error …/>` element per line, with well-known attribute names.

use anyhow::Result;
use regex::Regex;

use super::LintFinding;

/// Parse checkstyle XML output into findings.
///
/// Accepts the raw `<checkstyle>` document. The Maven plugin also prints a
/// plain-text summary to stdout when `mvn checkstyle:checkstyle` runs, and
/// writes the XML to `target/checkstyle-result.xml` — callers should feed
/// the XML contents here, not the stdout summary.
pub fn parse(output: &str) -> Result<Vec<LintFinding>> {
    // `<file name="..."> ... </file>` — one file block per source file.
    // We scan with a simple state machine: track the current <file name="">
    // and collect every <error …/> inside it.
    let file_open = Regex::new(r#"<file\s+name="([^"]+)""#)?;
    let file_close = Regex::new(r#"</file>"#)?;
    // `source` is the checkstyle check class (fully qualified). We rename it
    // into a short rule key (the last dotted segment, dropping `Check`).
    let error_line = Regex::new(
        r#"<error\s+([^/>]*?)/?>"#,
    )?;
    let attr = Regex::new(r#"(\w+)="([^"]*)""#)?;

    let mut findings = Vec::new();
    let mut current_file: Option<String> = None;

    for line in output.lines() {
        if let Some(cap) = file_open.captures(line) {
            current_file = Some(cap[1].to_string());
            continue;
        }
        if file_close.is_match(line) {
            current_file = None;
            continue;
        }
        let Some(file) = current_file.as_deref() else {
            continue;
        };
        let Some(err_cap) = error_line.captures(line) else {
            continue;
        };
        let attrs_blob = err_cap.get(1).map(|m| m.as_str()).unwrap_or("");

        let mut line_no: u32 = 0;
        let mut severity = String::from("warning");
        let mut message = String::new();
        let mut source = String::new();

        for a in attr.captures_iter(attrs_blob) {
            let key = &a[1];
            let val = &a[2];
            match key {
                "line" => line_no = val.parse().unwrap_or(0),
                "severity" => severity = val.to_string(),
                "message" => message = unescape_xml(val),
                "source" => source = val.to_string(),
                _ => {}
            }
        }

        if line_no == 0 {
            continue;
        }
        let rule = short_rule_name(&source);
        findings.push(LintFinding {
            file: file.to_string(),
            start_line: line_no,
            end_line: line_no,
            rule,
            severity,
            message,
        });
    }
    Ok(findings)
}

/// `com.puppycrawl.tools.checkstyle.checks.coding.UnusedLocalVariableCheck`
///   → `UnusedLocalVariable`. Keeps the rule key short and human-readable
/// so the synthetic `lint:checkstyle:<rule>` issue keys stay grep-friendly.
fn short_rule_name(source: &str) -> String {
    let last = source.rsplit('.').next().unwrap_or(source);
    last.strip_suffix("Check").unwrap_or(last).to_string()
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_checkstyle_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<checkstyle version="10.12.5">
<file name="src/main/java/Foo.java">
<error line="12" column="5" severity="warning" message="Unused import" source="com.puppycrawl.tools.checkstyle.checks.imports.UnusedImportsCheck"/>
<error line="30" severity="error" message="Line too long" source="com.puppycrawl.tools.checkstyle.checks.sizes.LineLengthCheck"/>
</file>
<file name="src/main/java/Bar.java">
<error line="8" severity="info" message="Magic number 42" source="com.puppycrawl.tools.checkstyle.checks.coding.MagicNumberCheck"/>
</file>
</checkstyle>"#;
        let findings = parse(xml).unwrap();
        assert_eq!(findings.len(), 3);

        assert_eq!(findings[0].file, "src/main/java/Foo.java");
        assert_eq!(findings[0].start_line, 12);
        assert_eq!(findings[0].severity, "warning");
        assert_eq!(findings[0].rule, "UnusedImports");
        assert_eq!(findings[0].message, "Unused import");

        assert_eq!(findings[1].start_line, 30);
        assert_eq!(findings[1].severity, "error");
        assert_eq!(findings[1].rule, "LineLength");

        assert_eq!(findings[2].file, "src/main/java/Bar.java");
        assert_eq!(findings[2].rule, "MagicNumber");
        assert_eq!(findings[2].severity, "info");
    }

    #[test]
    fn unescapes_xml_entities_in_message() {
        let xml = r#"<checkstyle>
<file name="X.java">
<error line="1" severity="warning" message="Use &quot;foo&quot; &amp; &lt;bar&gt;" source="com.puppycrawl.tools.checkstyle.checks.Check"/>
</file>
</checkstyle>"#;
        let findings = parse(xml).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, r#"Use "foo" & <bar>"#);
    }

    #[test]
    fn empty_input_yields_empty() {
        let findings = parse("").unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn ignores_errors_without_a_file_wrapper() {
        // Malformed — no <file name="…"> — should be skipped gracefully.
        let xml = r#"<checkstyle>
<error line="1" severity="warning" message="x" source="c.p.t.checkstyle.checks.Check"/>
</checkstyle>"#;
        let findings = parse(xml).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn skips_errors_without_line_number() {
        let xml = r#"<checkstyle>
<file name="X.java">
<error severity="warning" message="global" source="c.p.t.checkstyle.checks.Check"/>
<error line="5" severity="warning" message="ok" source="c.p.t.checkstyle.checks.Check"/>
</file>
</checkstyle>"#;
        let findings = parse(xml).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].start_line, 5);
    }
}
