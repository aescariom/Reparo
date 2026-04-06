//! Token usage tracking across all AI engine invocations.
//!
//! Provides a shared collector that records input/output tokens per (step, engine, model)
//! so the orchestrator can print a summary table at the end of a run.
//!
//! Each engine exposes usage differently:
//! - **Claude** (`--output-format json`): usage is a structured field in the result JSON.
//! - **Gemini**: best-effort parsing from stdout (unstable format).
//! - **Aider**: best-effort regex on stdout (e.g., "Tokens: 1.2k sent, 340 received").
//!
//! When parsing fails, the step is still recorded with zero tokens so the user sees
//! the call happened even if counts are unknown.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

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

/// A single recorded invocation.
#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub step: String,
    pub engine: EngineKind,
    pub model: String,
    pub usage: TokenUsage,
    /// True when parsing failed and counts are unknown (recorded as 0).
    pub unknown: bool,
}

/// Shared, thread-safe accumulator of usage entries.
#[derive(Debug, Default)]
pub struct UsageTracker {
    entries: Mutex<Vec<UsageEntry>>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, entry: UsageEntry) {
        if let Ok(mut v) = self.entries.lock() {
            v.push(entry);
        }
    }

    pub fn snapshot(&self) -> Vec<UsageEntry> {
        self.entries.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Merge all entries from another tracker into this one.
    /// Used to collect usage from parallel workers.
    pub fn merge_from(&self, other: &UsageTracker) {
        let entries = other.snapshot();
        if let Ok(mut v) = self.entries.lock() {
            v.extend(entries);
        }
    }
}

// --- Claude JSON output parsing ---

#[derive(Debug, Deserialize)]
struct ClaudeResultJson {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsageJson>,
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

/// Parse the JSON emitted by `claude --output-format json`.
///
/// Returns `(result_text, usage)` when successful. `result_text` is the final
/// assistant message — what previous callers received as raw stdout under
/// `--output-format text`. Returns `None` when stdout is not JSON or doesn't
/// match the expected schema (in which case callers should fall back to raw stdout).
pub fn parse_claude_json(stdout: &str) -> Option<(String, TokenUsage)> {
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: ClaudeResultJson = serde_json::from_str(trimmed).ok()?;
    let result = parsed.result.unwrap_or_default();
    let usage = parsed.usage.map(|u| TokenUsage {
        input: u.input_tokens,
        output: u.output_tokens,
        cache_read: u.cache_read_input_tokens,
        cache_creation: u.cache_creation_input_tokens,
    }).unwrap_or_default();
    Some((result, usage))
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

/// Render a markdown table of usage grouped by (step, model).
///
/// Each row is one (step, model) pair with aggregated tokens. A final TOTAL row
/// sums across all entries.
pub fn render_usage_table(entries: &[UsageEntry]) -> String {
    if entries.is_empty() {
        return "_No AI calls recorded._\n".to_string();
    }

    // Aggregate by (step, model) — BTreeMap for stable ordering.
    let mut agg: BTreeMap<(String, String, EngineKind), (TokenUsage, u32, u32)> = BTreeMap::new();
    for e in entries {
        let key = (e.step.clone(), e.model.clone(), e.engine.clone());
        let slot = agg.entry(key).or_insert((TokenUsage::default(), 0, 0));
        slot.0 += e.usage;
        slot.1 += 1;
        if e.unknown {
            slot.2 += 1;
        }
    }

    let mut out = String::new();
    out.push_str("| Step | Engine | Model | Calls | Input | Cache read | Output | Unknown |\n");
    out.push_str("|------|--------|-------|------:|------:|-----------:|-------:|--------:|\n");

    let mut total = TokenUsage::default();
    let mut total_calls: u32 = 0;
    let mut total_unknown: u32 = 0;

    for ((step, model, engine), (usage, calls, unknown)) in &agg {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            step,
            engine,
            model,
            calls,
            fmt_num(usage.input),
            fmt_num(usage.cache_read),
            fmt_num(usage.output),
            unknown,
        ));
        total += *usage;
        total_calls += calls;
        total_unknown += unknown;
    }

    out.push_str(&format!(
        "| **TOTAL** | — | — | **{}** | **{}** | **{}** | **{}** | **{}** |\n",
        total_calls,
        fmt_num(total.input),
        fmt_num(total.cache_read),
        fmt_num(total.output),
        total_unknown,
    ));

    out
}

fn fmt_num(n: u64) -> String {
    // Thousands separator for readability.
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_json_extracts_result_and_usage() {
        let stdout = r#"{"type":"result","subtype":"success","result":"All fixed.","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":10}}"#;
        let (text, usage) = parse_claude_json(stdout).unwrap();
        assert_eq!(text, "All fixed.");
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_read, 200);
        assert_eq!(usage.cache_creation, 10);
    }

    #[test]
    fn parse_claude_json_returns_none_for_non_json() {
        assert!(parse_claude_json("hello world").is_none());
        assert!(parse_claude_json("").is_none());
    }

    #[test]
    fn parse_claude_json_missing_usage_defaults_to_zero() {
        let stdout = r#"{"result":"done"}"#;
        let (text, usage) = parse_claude_json(stdout).unwrap();
        assert_eq!(text, "done");
        assert_eq!(usage, TokenUsage::default());
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
    fn tracker_records_and_snapshots() {
        let t = UsageTracker::new();
        t.record(UsageEntry {
            step: "fix".into(),
            engine: EngineKind::Claude,
            model: "sonnet".into(),
            usage: TokenUsage { input: 10, output: 20, cache_read: 0, cache_creation: 0 },
            unknown: false,
        });
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].usage.input, 10);
    }

    #[test]
    fn render_table_aggregates_by_step_model() {
        let entries = vec![
            UsageEntry {
                step: "fix".into(),
                engine: EngineKind::Claude,
                model: "sonnet".into(),
                usage: TokenUsage { input: 100, output: 50, cache_read: 0, cache_creation: 0 },
                unknown: false,
            },
            UsageEntry {
                step: "fix".into(),
                engine: EngineKind::Claude,
                model: "sonnet".into(),
                usage: TokenUsage { input: 200, output: 80, cache_read: 0, cache_creation: 0 },
                unknown: false,
            },
            UsageEntry {
                step: "coverage".into(),
                engine: EngineKind::Claude,
                model: "haiku".into(),
                usage: TokenUsage { input: 30, output: 10, cache_read: 0, cache_creation: 0 },
                unknown: false,
            },
        ];
        let table = render_usage_table(&entries);
        assert!(table.contains("fix"));
        assert!(table.contains("sonnet"));
        assert!(table.contains("haiku"));
        assert!(table.contains("300")); // aggregated input for fix/sonnet
        assert!(table.contains("TOTAL"));
    }

    #[test]
    fn fmt_num_adds_thousands_separator() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1000), "1,000");
        assert_eq!(fmt_num(1234567), "1,234,567");
    }
}
