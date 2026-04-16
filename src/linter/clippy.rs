//! Parser for `cargo clippy --message-format=json` output.
//!
//! Clippy emits one JSON object per line. Messages we care about have
//! `reason == "compiler-message"` and `message.code.code` starting with
//! `clippy::`. Each message carries a `spans` array — we take the primary
//! span for the location.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::LintFinding;

#[derive(Deserialize)]
struct CargoLine {
    reason: Option<String>,
    message: Option<ClippyMessage>,
}

#[derive(Deserialize)]
struct ClippyMessage {
    message: String,
    level: String,
    code: Option<ClippyCode>,
    spans: Vec<ClippySpan>,
}

#[derive(Deserialize)]
struct ClippyCode {
    code: String,
}

#[derive(Deserialize)]
struct ClippySpan {
    file_name: String,
    is_primary: bool,
    line_start: u32,
    line_end: u32,
}

pub fn parse(output: &str) -> Result<Vec<LintFinding>> {
    let mut out = Vec::new();
    let mut saw_any_json = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        saw_any_json = true;
        let Ok(entry) = serde_json::from_str::<CargoLine>(trimmed) else {
            continue;
        };
        if entry.reason.as_deref() != Some("compiler-message") {
            continue;
        }
        let Some(msg) = entry.message else { continue };
        let Some(code) = msg.code else { continue };
        if !code.code.starts_with("clippy::") {
            continue;
        }
        let primary = msg
            .spans
            .iter()
            .find(|s| s.is_primary)
            .or_else(|| msg.spans.first());
        let Some(span) = primary else { continue };

        out.push(LintFinding {
            file: span.file_name.clone(),
            start_line: span.line_start,
            end_line: span.line_end,
            rule: code.code.trim_start_matches("clippy::").to_string(),
            severity: msg.level.clone(),
            message: msg.message.clone(),
        });
    }

    if !saw_any_json && !output.trim().is_empty() {
        return Err(anyhow!(
            "no JSON objects found in clippy output — did you pass --message-format=json?"
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clippy_warning() {
        let sample = r#"{"reason":"compiler-message","package_id":"x","message":{"message":"unused import: `Bar`","level":"warning","spans":[{"file_name":"src/main.rs","byte_start":1,"byte_end":2,"line_start":10,"line_end":10,"column_start":1,"column_end":5,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"code":{"code":"clippy::unused_imports","explanation":null},"rendered":"warning: unused import"}}"#;
        let findings = parse(sample).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, "src/main.rs");
        assert_eq!(f.start_line, 10);
        assert_eq!(f.rule, "unused_imports");
        assert_eq!(f.severity, "warning");
        assert!(f.message.contains("unused import"));
    }

    #[test]
    fn skips_non_clippy_codes() {
        // rustc warning without clippy:: prefix — should NOT appear in results.
        let sample = r#"{"reason":"compiler-message","message":{"message":"x","level":"warning","spans":[{"file_name":"a.rs","byte_start":0,"byte_end":0,"line_start":1,"line_end":1,"column_start":1,"column_end":1,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"code":{"code":"unused_variables","explanation":null},"rendered":""}}"#;
        assert_eq!(parse(sample).unwrap().len(), 0);
    }

    #[test]
    fn skips_non_compiler_message_reasons() {
        let sample = r#"{"reason":"build-script-executed","package_id":"x"}"#;
        assert_eq!(parse(sample).unwrap().len(), 0);
    }

    #[test]
    fn empty_output_is_ok() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn non_json_output_is_error() {
        let sample = "warning: unused import\n  --> src/main.rs:10:1\n";
        assert!(parse(sample).is_err());
    }

    #[test]
    fn multiple_lines_with_gaps_ok() {
        let sample = "garbage line\n{\"reason\":\"compiler-artifact\",\"package_id\":\"x\"}\n{\"reason\":\"compiler-message\",\"message\":{\"message\":\"m\",\"level\":\"error\",\"spans\":[{\"file_name\":\"f.rs\",\"byte_start\":0,\"byte_end\":0,\"line_start\":3,\"line_end\":3,\"column_start\":1,\"column_end\":1,\"is_primary\":true,\"text\":[],\"label\":null,\"suggested_replacement\":null,\"suggestion_applicability\":null,\"expansion\":null}],\"children\":[],\"code\":{\"code\":\"clippy::needless_return\",\"explanation\":null},\"rendered\":\"\"}}\n";
        let findings = parse(sample).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "needless_return");
    }
}
