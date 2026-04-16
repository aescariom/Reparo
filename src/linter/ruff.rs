//! Parser for `ruff check --output-format json` output.
//!
//! Ruff emits a flat JSON array of diagnostics. Each diagnostic carries
//! `filename`, `code` (rule name), `message`, and a `location`/`end_location`
//! with `row` fields.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::LintFinding;

#[derive(Deserialize)]
struct Diagnostic {
    filename: String,
    code: Option<String>,
    message: String,
    location: Location,
    end_location: Option<Location>,
}

#[derive(Deserialize)]
struct Location {
    row: u32,
}

pub fn parse(output: &str) -> Result<Vec<LintFinding>> {
    let start = output.find('[').ok_or_else(|| anyhow!("no JSON array in ruff output"))?;
    let end = output.rfind(']').ok_or_else(|| anyhow!("truncated ruff JSON"))?;
    let json = &output[start..=end];

    let diagnostics: Vec<Diagnostic> = serde_json::from_str(json)
        .map_err(|e| anyhow!("invalid ruff JSON: {}", e))?;

    let mut out = Vec::new();
    for d in diagnostics {
        let start_line = d.location.row;
        let end_line = d.end_location.map(|l| l.row).unwrap_or(start_line);
        out.push(LintFinding {
            file: d.filename,
            start_line,
            end_line,
            rule: d.code.unwrap_or_else(|| "unknown".to_string()),
            // Ruff findings don't carry a severity; tagging as "warning" puts
            // them into the MAJOR bucket, matching how ruff treats all
            // non-autofixable diagnostics as lint violations.
            severity: "warning".to_string(),
            message: d.message,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ruff_json() {
        let sample = r#"[{"code":"F401","message":"`os` imported but unused","fix":null,"location":{"row":3,"column":8},"end_location":{"row":3,"column":10},"filename":"src/app.py","noqa_row":3}]"#;
        let findings = parse(sample).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.file, "src/app.py");
        assert_eq!(f.start_line, 3);
        assert_eq!(f.end_line, 3);
        assert_eq!(f.rule, "F401");
    }

    #[test]
    fn spans_multiple_lines() {
        let sample = r#"[{"code":"E501","message":"line too long","location":{"row":10,"column":1},"end_location":{"row":12,"column":1},"filename":"a.py"}]"#;
        let f = &parse(sample).unwrap()[0];
        assert_eq!(f.start_line, 10);
        assert_eq!(f.end_line, 12);
    }

    #[test]
    fn missing_code_falls_back() {
        let sample = r#"[{"message":"m","location":{"row":1,"column":1},"end_location":{"row":1,"column":1},"filename":"a.py"}]"#;
        assert_eq!(parse(sample).unwrap()[0].rule, "unknown");
    }

    #[test]
    fn empty_array_is_ok() {
        assert!(parse("[]").unwrap().is_empty());
    }

    #[test]
    fn handles_preamble() {
        let sample = "Ruff 0.1.0\n[{\"code\":\"F401\",\"message\":\"m\",\"location\":{\"row\":1,\"column\":1},\"end_location\":{\"row\":1,\"column\":1},\"filename\":\"a.py\"}]\n";
        assert_eq!(parse(sample).unwrap().len(), 1);
    }
}
