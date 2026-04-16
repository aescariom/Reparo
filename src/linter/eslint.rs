//! Parser for `eslint -f json` output.
//!
//! ESLint JSON is a flat array of file entries; each entry has a `messages`
//! array with `ruleId`, `severity` (1=warn, 2=error), `line`, `endLine`,
//! and `message`.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::LintFinding;

#[derive(Deserialize)]
struct FileEntry {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(default)]
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    severity: u8,
    message: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(rename = "endLine", default)]
    end_line: Option<u32>,
}

pub fn parse(output: &str) -> Result<Vec<LintFinding>> {
    // ESLint wraps its JSON array in the stdout stream; strip anything before
    // the first `[`.
    let start = output.find('[').ok_or_else(|| anyhow!("no JSON array in eslint output"))?;
    let end = output.rfind(']').ok_or_else(|| anyhow!("truncated eslint JSON"))?;
    let json = &output[start..=end];

    let files: Vec<FileEntry> = serde_json::from_str(json)
        .map_err(|e| anyhow!("invalid eslint JSON: {}", e))?;

    let mut out = Vec::new();
    for f in files {
        for m in f.messages {
            let Some(line) = m.line else { continue };
            // Parse-errors appear with ruleId=null; skip — Reparo can't fix
            // a file that ESLint itself can't parse.
            let Some(rule_id) = m.rule_id else { continue };
            let severity = match m.severity {
                2 => "error",
                1 => "warning",
                _ => "info",
            };
            out.push(LintFinding {
                file: f.file_path.clone(),
                start_line: line,
                end_line: m.end_line.unwrap_or(line),
                rule: rule_id,
                severity: severity.to_string(),
                message: m.message,
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eslint_json() {
        let sample = r#"[{"filePath":"/proj/src/Foo.ts","messages":[{"ruleId":"no-unused-vars","severity":2,"message":"'x' is assigned a value but never used.","line":42,"endLine":42,"column":1}],"errorCount":1,"warningCount":0}]"#;
        let findings = parse(sample).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, "/proj/src/Foo.ts");
        assert_eq!(f.start_line, 42);
        assert_eq!(f.end_line, 42);
        assert_eq!(f.rule, "no-unused-vars");
        assert_eq!(f.severity, "error");
    }

    #[test]
    fn warning_severity_maps_correctly() {
        let sample = r#"[{"filePath":"a.js","messages":[{"ruleId":"semi","severity":1,"message":"m","line":1,"column":1}]}]"#;
        let findings = parse(sample).unwrap();
        assert_eq!(findings[0].severity, "warning");
    }

    #[test]
    fn skips_parse_error_null_rule() {
        let sample = r#"[{"filePath":"a.js","messages":[{"ruleId":null,"severity":2,"message":"parse error","line":1,"column":1}]}]"#;
        assert!(parse(sample).unwrap().is_empty());
    }

    #[test]
    fn handles_preamble_before_json() {
        // ESLint sometimes prints warnings on stderr that get interleaved.
        let sample = "deprecation: blah\n[{\"filePath\":\"a.js\",\"messages\":[{\"ruleId\":\"r\",\"severity\":2,\"message\":\"m\",\"line\":1,\"column\":1}]}]\n";
        let findings = parse(sample).unwrap();
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn empty_array_is_ok() {
        assert!(parse("[]").unwrap().is_empty());
    }

    #[test]
    fn endline_defaults_to_line() {
        let sample = r#"[{"filePath":"a.js","messages":[{"ruleId":"r","severity":2,"message":"m","line":7,"column":1}]}]"#;
        let f = &parse(sample).unwrap()[0];
        assert_eq!(f.start_line, 7);
        assert_eq!(f.end_line, 7);
    }
}
