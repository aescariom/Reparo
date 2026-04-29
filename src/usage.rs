//! Token usage data types and per-engine parsers.
//!
//! This module provides:
//! - `TokenUsage` / `UsageEntry`: data structures describing one AI invocation's cost
//! - Parsers that extract usage from each engine's stdout (Claude JSON, Aider, Gemini)
//!
//! Historical note: an in-memory `UsageTracker` used to accumulate entries for a
//! run-end summary table. That responsibility has moved to `execution_log`
//! (SQLite-backed) — the summary is now computed directly from the database,
//! so this module keeps only the parsing primitives.

use serde::Deserialize;

use crate::engine::EngineKind;

/// Token counts for a single AI invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl TokenUsage {
    /// Total input tokens including cache reads + creation.
    #[allow(dead_code)]
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation
    }
}

impl std::ops::Add for TokenUsage {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            input: self.input + rhs.input,
            output: self.output + rhs.output,
            cache_read: self.cache_read + rhs.cache_read,
            cache_creation: self.cache_creation + rhs.cache_creation,
        }
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

/// A single recorded invocation. Produced by the engine layer and forwarded
/// directly to the execution log (SQLite).
#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub step: String,
    pub engine: EngineKind,
    pub model: String,
    pub usage: TokenUsage,
    /// True when parsing failed and counts are unknown (recorded as 0).
    pub unknown: bool,
}

// --- Claude JSON output parsing ---

#[derive(Debug, Deserialize)]
struct ClaudeResultJson {
    // `result` can be a plain string OR an array of content blocks depending on
    // the Claude CLI version — use Value so a type mismatch never silently drops the line.
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<ClaudeUsageJson>,
    /// US-081: session id present on `type: "result"` lines emitted by `claude
    /// --output-format json`. Captured so the orchestrator can pass it back
    /// via `--resume <id>` on the next invocation for the same file.
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeUsageJson {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// Parse the JSON emitted by `claude --output-format json`. Returns
/// `(result_text, usage, session_id)` (US-081 — the orchestrator chains
/// successive invocations on the same file via `--resume <session_id>`).
/// `result_text` is the final assistant message — what previous callers
/// received as raw stdout under `--output-format text`. The session id is
/// itself optional — older Claude CLI versions may not emit it. Returns
/// `None` when stdout is not JSON or doesn't match the expected schema
/// (callers fall back to raw stdout).
pub fn parse_claude_json_full(stdout: &str) -> Option<(String, TokenUsage, Option<String>)> {
    let result_line = stdout
        .lines()
        .filter(|l| l.trim_start().starts_with('{'))
        .filter_map(|l| serde_json::from_str::<ClaudeResultJson>(l).ok())
        .filter(|p| p.usage.is_some())
        .last()?;

    let result = match result_line.result {
        Some(serde_json::Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let usage = result_line.usage.map(|u| TokenUsage {
        input: u.input_tokens,
        output: u.output_tokens,
        cache_read: u.cache_read_input_tokens,
        cache_creation: u.cache_creation_input_tokens,
    }).unwrap_or_default();
    Some((result, usage, result_line.session_id))
}

/// Best-effort parser for Aider's end-of-run summary.
///
/// Aider prints lines like "Tokens: 1.2k sent, 340 received." — this looks for
/// the pattern and returns the usage if found.
pub fn parse_aider_usage(stdout: &str) -> Option<TokenUsage> {
    let re = regex::Regex::new(r"(?i)tokens:\s*([\d.]+)\s*([km]?)\s*sent[,\s]+([\d.]+)\s*([km]?)\s*received").ok()?;
    let caps = re.captures(stdout)?;
    let input = parse_k_number(&caps[1], &caps[2]);
    let output = parse_k_number(&caps[3], &caps[4]);
    Some(TokenUsage { input, output, cache_read: 0, cache_creation: 0 })
}

fn parse_k_number(num: &str, suffix: &str) -> u64 {
    let val: f64 = num.parse().unwrap_or(0.0);
    let mult = match suffix.to_lowercase().as_str() {
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => 1.0,
    };
    (val * mult) as u64
}

/// Best-effort parser for Gemini CLI usage output.
/// Format is not stable — returns None if not detected.
pub fn parse_gemini_usage(stdout: &str) -> Option<TokenUsage> {
    // Gemini CLI may print lines like "Prompt tokens: 1234" / "Response tokens: 567"
    let re_in = regex::Regex::new(r"(?i)(?:prompt|input)\s+tokens[:\s]+(\d+)").ok()?;
    let re_out = regex::Regex::new(r"(?i)(?:response|output|completion)\s+tokens[:\s]+(\d+)").ok()?;
    let input = re_in.captures(stdout).and_then(|c| c[1].parse().ok()).unwrap_or(0);
    let output = re_out.captures(stdout).and_then(|c| c[1].parse().ok()).unwrap_or(0);
    if input == 0 && output == 0 {
        return None;
    }
    Some(TokenUsage { input, output, cache_read: 0, cache_creation: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_json_extracts_result_and_usage() {
        let stdout = r#"{"type":"result","subtype":"success","result":"All fixed.","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":10}}"#;
        let (text, usage) = parse_claude_json_full(stdout).map(|(t, u, _)| (t, u)).unwrap();
        assert_eq!(text, "All fixed.");
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_read, 200);
        assert_eq!(usage.cache_creation, 10);
    }

    #[test]
    fn parse_claude_json_returns_none_for_non_json() {
        assert!(parse_claude_json_full("hello world").is_none());
        assert!(parse_claude_json_full("").is_none());
    }

    #[test]
    fn parse_claude_json_missing_usage_returns_none() {
        // No `usage` field → filtered out; function returns None (caller falls back to raw stdout).
        let stdout = r#"{"result":"done"}"#;
        assert!(parse_claude_json_full(stdout).is_none());
    }

    #[test]
    fn parse_claude_json_full_extracts_session_id() {
        let stdout = r#"{"type":"result","subtype":"success","result":"ok","session_id":"abc-123","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        let (_text, _usage, sid) = parse_claude_json_full(stdout).unwrap();
        assert_eq!(sid.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_claude_json_full_session_id_absent_yields_none() {
        // Older Claude CLI versions don't emit session_id; the parser must
        // still succeed and return None for that field.
        let stdout = r#"{"type":"result","subtype":"success","result":"ok","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        let (_text, _usage, sid) = parse_claude_json_full(stdout).unwrap();
        assert!(sid.is_none());
    }

    #[test]
    fn parse_claude_json_result_as_array_content_blocks() {
        // Newer Claude CLI versions may emit result as an array of content blocks.
        // The function must still return usage even when result is not a plain string.
        let stdout = r#"{"type":"result","subtype":"success","result":[{"type":"text","text":"Fixed."}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#;
        let (_text, usage) = parse_claude_json_full(stdout).map(|(t, u, _)| (t, u)).unwrap();
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 5);
    }

    #[test]
    fn parse_claude_json_multiline_ndjson() {
        // claude --output-format json emits one JSON object per line (NDJSON).
        // The result+usage live on the last line with "type":"result".
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"abc\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"fixing...\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Fixed.\",",
            "\"usage\":{\"input_tokens\":500,\"output_tokens\":120,",
            "\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":0}}\n",
        );
        let (text, usage) = parse_claude_json_full(stdout).map(|(t, u, _)| (t, u)).unwrap();
        assert_eq!(text, "Fixed.");
        assert_eq!(usage.input, 500);
        assert_eq!(usage.output, 120);
        assert_eq!(usage.cache_read, 300);
    }

    #[test]
    fn parse_aider_usage_basic() {
        let out = "Some output\nTokens: 1.2k sent, 340 received.\n";
        let u = parse_aider_usage(out).unwrap();
        assert_eq!(u.input, 1200);
        assert_eq!(u.output, 340);
    }

    #[test]
    fn parse_aider_usage_no_match() {
        assert!(parse_aider_usage("nothing here").is_none());
    }

    #[test]
    fn parse_gemini_usage_basic() {
        let out = "Prompt tokens: 500\nResponse tokens: 120\n";
        let u = parse_gemini_usage(out).unwrap();
        assert_eq!(u.input, 500);
        assert_eq!(u.output, 120);
    }

    #[test]
    fn parse_claude_json_actual_cli_output() {
        // Real output from `claude 2.1.x --output-format json`.
        // The `usage` object has nested sub-objects (server_tool_use, cache_creation,
        // iterations) that are NOT in our struct — they must be silently ignored.
        let stdout = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1224,"num_turns":1,"result":"ok","#,
            r#""stop_reason":"end_turn","session_id":"abc","total_cost_usd":0.036,"#,
            r#""usage":{"input_tokens":2,"cache_creation_input_tokens":8743,"cache_read_input_tokens":12030,"output_tokens":4,"#,
            r#""server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","#,
            r#""cache_creation":{"ephemeral_1h_input_tokens":8743,"ephemeral_5m_input_tokens":0},"inference_geo":"","#,
            r#""iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":12030,"cache_creation_input_tokens":8743}],"speed":"standard"}}"#,
        );
        let (text, usage) = parse_claude_json_full(stdout)
            .map(|(t, u, _)| (t, u))
            .expect("must parse real CLI output");
        assert_eq!(text, "ok");
        assert_eq!(usage.input, 2);
        assert_eq!(usage.output, 4);
        assert_eq!(usage.cache_read, 12030);
        assert_eq!(usage.cache_creation, 8743);
    }

}
