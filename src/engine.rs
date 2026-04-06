//! AI engine abstraction for multi-engine support.
//!
//! Provides a unified interface to invoke different AI CLI tools (Claude, Gemini, Aider)
//! based on tier routing configuration. The tier system classifies tasks by complexity
//! and routes them to the appropriate engine/model combination.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::claude::ClaudeTier;
use crate::usage::{self, TokenUsage};

/// Which AI engine to use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Claude,
    Gemini,
    Aider,
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineKind::Claude => write!(f, "claude"),
            EngineKind::Gemini => write!(f, "gemini"),
            EngineKind::Aider => write!(f, "aider"),
        }
    }
}

/// Configuration for a single AI engine.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct EngineConfig {
    /// CLI command to invoke (e.g., "claude", "gemini", "aider")
    pub command: String,
    /// Base arguments always passed to the command
    pub args: Vec<String>,
    /// Whether this engine is available for routing
    pub enabled: bool,
    /// CLI flag used to pass the prompt (e.g., "-p", "--message")
    pub prompt_flag: String,
    /// If true, prompt is passed via stdin instead of a flag
    pub prompt_via_stdin: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            enabled: false,
            prompt_flag: "-p".to_string(),
            prompt_via_stdin: false,
        }
    }
}

/// Routing entry: maps a tier to an engine + model + effort.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TierRouting {
    /// Engine name (must match a key in the engines map)
    pub engine: String,
    /// Model to use (engine-specific, e.g., "sonnet", "qwen-coder-30b")
    pub model: Option<String>,
    /// Effort level (only applies to Claude: "low", "medium", "high", "max")
    pub effort: Option<String>,
}

/// Routing configuration mapping tier levels to engines.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
#[serde(default)]
pub struct RoutingConfig {
    /// Simple tasks (haiku-equivalent): unused imports, trivial fixes
    pub tier1: Option<TierRouting>,
    /// Medium tasks (sonnet-equivalent): moderate refactoring
    pub tier2: Option<TierRouting>,
    /// Complex tasks (opus high): significant logic changes
    pub tier3: Option<TierRouting>,
    /// Very complex tasks (opus max): deep refactoring, high cognitive complexity
    pub tier4: Option<TierRouting>,
}

/// Bundled engine + routing configuration for runtime use.
#[derive(Debug, Clone)]
pub struct EngineRoutingConfig {
    pub engines: HashMap<String, EngineConfig>,
    pub routing: RoutingConfig,
}

impl Default for EngineRoutingConfig {
    fn default() -> Self {
        Self {
            engines: default_engines(),
            routing: default_routing(),
        }
    }
}

/// Resolved engine invocation — everything needed to spawn the process.
#[derive(Debug, Clone)]
pub struct EngineInvocation {
    pub engine_kind: EngineKind,
    pub command: String,
    pub base_args: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt_flag: String,
    pub prompt_via_stdin: bool,
}

/// Default engine configurations.
pub fn default_engines() -> HashMap<String, EngineConfig> {
    let mut engines = HashMap::new();
    engines.insert(
        "claude".to_string(),
        EngineConfig {
            command: "claude".to_string(),
            // JSON output lets us extract token usage alongside the final text.
            // The `result` field of the JSON contains what `--output-format text` used to return.
            args: vec!["-d".to_string(), "--output-format".to_string(), "json".to_string()],
            enabled: true,
            prompt_flag: "-p".to_string(),
            prompt_via_stdin: false,
        },
    );
    engines.insert(
        "gemini".to_string(),
        EngineConfig {
            command: "gemini".to_string(),
            args: Vec::new(),
            enabled: false,
            prompt_flag: "-p".to_string(),
            prompt_via_stdin: false,
        },
    );
    engines.insert(
        "aider".to_string(),
        EngineConfig {
            command: "aider".to_string(),
            args: vec!["--yes-always".to_string(), "--no-git".to_string()],
            enabled: false,
            prompt_flag: "--message".to_string(),
            prompt_via_stdin: false,
        },
    );
    engines
}

/// Default routing: all tiers go to Claude (preserves current behavior).
pub fn default_routing() -> RoutingConfig {
    RoutingConfig {
        tier1: Some(TierRouting {
            engine: "claude".to_string(),
            model: Some("haiku".to_string()),
            effort: Some("low".to_string()),
        }),
        tier2: Some(TierRouting {
            engine: "claude".to_string(),
            model: Some("sonnet".to_string()),
            effort: Some("medium".to_string()),
        }),
        tier3: Some(TierRouting {
            engine: "claude".to_string(),
            model: Some("opus".to_string()),
            effort: Some("high".to_string()),
        }),
        tier4: Some(TierRouting {
            engine: "claude".to_string(),
            model: Some("opus".to_string()),
            effort: Some("max".to_string()),
        }),
    }
}

/// Map a ClaudeTier to a tier level string (tier1..tier4).
fn tier_level(tier: &ClaudeTier) -> &'static str {
    match (tier.model, tier.effort) {
        ("haiku", _) => "tier1",
        ("sonnet", "low") | ("sonnet", "medium") => "tier2",
        ("sonnet", "high") => "tier3",
        ("opus", "high") => "tier3",
        ("opus", "max") => "tier4",
        _ => "tier2", // safe default
    }
}

/// Parse an engine name string into an EngineKind.
fn parse_engine_kind(name: &str) -> Result<EngineKind> {
    match name.to_lowercase().as_str() {
        "claude" => Ok(EngineKind::Claude),
        "gemini" => Ok(EngineKind::Gemini),
        "aider" => Ok(EngineKind::Aider),
        _ => bail!("Unknown engine: '{}'. Supported: claude, gemini, aider", name),
    }
}

/// Resolve which engine to use for a given tier.
///
/// Looks up the tier level in routing, finds the engine config, and builds
/// an EngineInvocation ready to spawn.
pub fn resolve_engine_for_tier(
    tier: &ClaudeTier,
    config: &EngineRoutingConfig,
) -> Result<EngineInvocation> {
    let level = tier_level(tier);

    // Get the routing entry for this tier level
    let routing = match level {
        "tier1" => config.routing.tier1.as_ref(),
        "tier2" => config.routing.tier2.as_ref(),
        "tier3" => config.routing.tier3.as_ref(),
        "tier4" => config.routing.tier4.as_ref(),
        _ => None,
    };

    // Fall back to Claude with the tier's own model/effort if no routing
    let (engine_name, model, effort) = match routing {
        Some(r) => (
            r.engine.as_str(),
            r.model.clone(),
            r.effort.clone(),
        ),
        None => (
            "claude",
            Some(tier.model.to_string()),
            Some(tier.effort.to_string()),
        ),
    };

    let engine_kind = parse_engine_kind(engine_name)?;

    let engine_config = config
        .engines
        .get(engine_name)
        .with_context(|| format!("Engine '{}' referenced in routing but not defined in engines", engine_name))?;

    Ok(EngineInvocation {
        engine_kind,
        command: engine_config.command.clone(),
        base_args: engine_config.args.clone(),
        model,
        effort,
        prompt_flag: engine_config.prompt_flag.clone(),
        prompt_via_stdin: engine_config.prompt_via_stdin,
    })
}

/// Validate that all engines used in routing are enabled and available in PATH.
///
/// Returns an error if any referenced engine is not available — the user decided
/// that missing engines should be fatal, not a warning.
pub fn validate_engines(config: &EngineRoutingConfig) -> Result<()> {
    // Collect all engine names actually referenced in routing
    let mut referenced = std::collections::HashSet::new();
    if let Some(ref r) = config.routing.tier1 { referenced.insert(r.engine.as_str()); }
    if let Some(ref r) = config.routing.tier2 { referenced.insert(r.engine.as_str()); }
    if let Some(ref r) = config.routing.tier3 { referenced.insert(r.engine.as_str()); }
    if let Some(ref r) = config.routing.tier4 { referenced.insert(r.engine.as_str()); }

    for engine_name in referenced {
        let engine_config = config
            .engines
            .get(engine_name)
            .with_context(|| format!(
                "Engine '{}' referenced in routing but not defined in engines config",
                engine_name
            ))?;

        if !engine_config.enabled {
            bail!(
                "Engine '{}' is referenced in tier routing but is disabled. \
                 Enable it in your config or change the routing.",
                engine_name
            );
        }

        if which::which(&engine_config.command).is_err() {
            bail!(
                "Engine '{}' command '{}' not found in PATH. \
                 Install it or change the routing to use an available engine.",
                engine_name,
                engine_config.command
            );
        }
    }

    Ok(())
}

/// Result of an engine invocation: the assistant's final text output plus
/// (when available) token usage reported by the engine.
#[derive(Debug, Clone, Default)]
pub struct EngineOutput {
    /// Final text output from the assistant. For Claude JSON mode this is the
    /// `result` field; for other engines it's the raw stdout.
    pub stdout: String,
    /// Parsed token usage. `None` when the engine didn't report usage or
    /// parsing failed — callers should record the step as "unknown" in that case.
    pub usage: Option<TokenUsage>,
}

/// Backward-compat wrapper: run an engine and return only the text output.
///
/// Existing call sites that don't care about token usage keep using this.
/// New code that needs usage should call `run_engine_full`.
pub fn run_engine(
    project_path: &Path,
    prompt: &str,
    timeout_secs: u64,
    skip_permissions: bool,
    show_prompt: bool,
    invocation: &EngineInvocation,
) -> Result<String> {
    run_engine_full(project_path, prompt, timeout_secs, skip_permissions, show_prompt, invocation)
        .map(|o| o.stdout)
}

/// Execute an AI engine with the given prompt.
///
/// Handles the differences between CLI tools:
/// - Claude: `-d --output-format json --model X --effort Y -p <prompt>` — usage parsed from JSON
/// - Gemini: `--model X -p <prompt>` — usage parsed best-effort from stdout
/// - Aider: `--yes-always --no-git --model X --message <prompt>` — usage parsed best-effort from stdout
pub fn run_engine_full(
    project_path: &Path,
    prompt: &str,
    timeout_secs: u64,
    skip_permissions: bool,
    show_prompt: bool,
    invocation: &EngineInvocation,
) -> Result<EngineOutput> {
    info!(
        "Running {} (prompt: {} chars, timeout: {}s, model: {})",
        invocation.engine_kind,
        prompt.len(),
        timeout_secs,
        invocation.model.as_deref().unwrap_or("default"),
    );
    if show_prompt {
        info!("AI prompt:\n{}", prompt);
    }

    let start = Instant::now();
    let mut args: Vec<String> = invocation.base_args.clone();

    // Engine-specific argument building
    match invocation.engine_kind {
        EngineKind::Claude => {
            if skip_permissions {
                args.push("--dangerously-skip-permissions".to_string());
            }
            // Add model and effort
            if let Some(ref model) = invocation.model {
                args.extend(["--model".to_string(), model.clone()]);
            }
            if let Some(ref effort) = invocation.effort {
                args.extend(["--effort".to_string(), effort.clone()]);
            }
        }
        EngineKind::Gemini => {
            if let Some(ref model) = invocation.model {
                args.extend(["--model".to_string(), model.clone()]);
            }
        }
        EngineKind::Aider => {
            if let Some(ref model) = invocation.model {
                args.extend(["--model".to_string(), model.clone()]);
            }
        }
    }

    // Add prompt via flag or prepare for stdin
    if !invocation.prompt_via_stdin {
        args.extend([invocation.prompt_flag.clone(), prompt.to_string()]);
    }

    let mut cmd = Command::new(&invocation.command);
    cmd.current_dir(project_path)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if invocation.prompt_via_stdin {
        cmd.stdin(std::process::Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!(
            "Failed to spawn '{}' CLI. Is it installed and in PATH?",
            invocation.command
        ))?;

    // If prompt goes via stdin, write it
    if invocation.prompt_via_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(prompt.as_bytes());
            // Drop stdin to close it and signal EOF
        }
    }

    // Wait with timeout
    let timeout = Duration::from_secs(timeout_secs);
    let result = wait_with_timeout(&mut child, timeout);

    match result {
        WaitResult::Completed(output) => {
            let elapsed = start.elapsed().as_secs();
            let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                warn!("{} exited with status: {} ({}s)", invocation.engine_kind, output.status, elapsed);
                if !stderr.is_empty() {
                    warn!("{} stderr: {}", invocation.engine_kind, stderr);
                }
                // Quick failure (< 10s) = CLI error
                if elapsed < 10 {
                    let error_detail = if !stderr.is_empty() {
                        stderr.clone()
                    } else if !raw_stdout.is_empty() {
                        truncate_str(&raw_stdout, 500)
                    } else {
                        format!("exit status: {} (no output)", output.status)
                    };
                    bail!(
                        "{} CLI failed immediately ({}s): {}",
                        invocation.engine_kind, elapsed, error_detail
                    );
                }
                // Longer runs that exit non-zero may have done partial work
                warn!(
                    "{} exited non-zero after {}s — checking for changes anyway",
                    invocation.engine_kind, elapsed
                );
            } else {
                info!("{} completed in {}s", invocation.engine_kind, elapsed);
            }

            // Engine-specific usage extraction. For Claude JSON mode, also replace
            // the returned stdout with the `result` field so downstream callers
            // see the same text they used to see under `--output-format text`.
            let (final_stdout, usage) = match invocation.engine_kind {
                EngineKind::Claude => {
                    if let Some((result_text, u)) = usage::parse_claude_json(&raw_stdout) {
                        (result_text, Some(u))
                    } else {
                        // Non-JSON output (older CLI, `--output-format text`, or test stubs).
                        (raw_stdout, None)
                    }
                }
                EngineKind::Gemini => {
                    let u = usage::parse_gemini_usage(&raw_stdout);
                    (raw_stdout, u)
                }
                EngineKind::Aider => {
                    let u = usage::parse_aider_usage(&raw_stdout);
                    (raw_stdout, u)
                }
            };

            Ok(EngineOutput { stdout: final_stdout, usage })
        }
        WaitResult::TimedOut => {
            let _ = child.kill();
            let _ = child.wait(); // reap zombie
            let elapsed = start.elapsed().as_secs();
            bail!(
                "{} timed out after {}s (limit: {}s). The process was killed.",
                invocation.engine_kind, elapsed, timeout_secs
            );
        }
    }
}

/// UTF-8-safe string truncation.
fn truncate_str(s: &str, max: usize) -> String {
    let truncated: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{}...", truncated)
    } else {
        truncated
    }
}

enum WaitResult {
    Completed(std::process::Output),
    TimedOut,
}

/// Wait for a child process with a timeout, polling every 500ms.
fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> WaitResult {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_end(&mut stderr);
                }
                return WaitResult::Completed(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return WaitResult::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => {
                return WaitResult::TimedOut;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_level_haiku() {
        let tier = ClaudeTier::with_timeout("haiku", "low", 0.3);
        assert_eq!(tier_level(&tier), "tier1");
    }

    #[test]
    fn test_tier_level_sonnet_medium() {
        let tier = ClaudeTier::with_timeout("sonnet", "medium", 0.5);
        assert_eq!(tier_level(&tier), "tier2");
    }

    #[test]
    fn test_tier_level_sonnet_high() {
        let tier = ClaudeTier::with_timeout("sonnet", "high", 1.0);
        assert_eq!(tier_level(&tier), "tier3");
    }

    #[test]
    fn test_tier_level_opus_high() {
        let tier = ClaudeTier::with_timeout("opus", "high", 1.5);
        assert_eq!(tier_level(&tier), "tier3");
    }

    #[test]
    fn test_tier_level_opus_max() {
        let tier = ClaudeTier::with_timeout("opus", "max", 2.0);
        assert_eq!(tier_level(&tier), "tier4");
    }

    #[test]
    fn test_parse_engine_kind() {
        assert_eq!(parse_engine_kind("claude").unwrap(), EngineKind::Claude);
        assert_eq!(parse_engine_kind("gemini").unwrap(), EngineKind::Gemini);
        assert_eq!(parse_engine_kind("aider").unwrap(), EngineKind::Aider);
        assert_eq!(parse_engine_kind("Claude").unwrap(), EngineKind::Claude);
        assert!(parse_engine_kind("unknown").is_err());
    }

    #[test]
    fn test_default_engines_has_all_three() {
        let engines = default_engines();
        assert!(engines.contains_key("claude"));
        assert!(engines.contains_key("gemini"));
        assert!(engines.contains_key("aider"));
        assert!(engines["claude"].enabled);
        assert!(!engines["gemini"].enabled);
        assert!(!engines["aider"].enabled);
    }

    #[test]
    fn test_default_routing_all_claude() {
        let routing = default_routing();
        assert_eq!(routing.tier1.as_ref().unwrap().engine, "claude");
        assert_eq!(routing.tier2.as_ref().unwrap().engine, "claude");
        assert_eq!(routing.tier3.as_ref().unwrap().engine, "claude");
        assert_eq!(routing.tier4.as_ref().unwrap().engine, "claude");
    }

    #[test]
    fn test_resolve_engine_for_tier_default() {
        let config = EngineRoutingConfig::default();
        let tier = ClaudeTier::with_timeout("sonnet", "medium", 0.5);
        let invocation = resolve_engine_for_tier(&tier, &config).unwrap();
        assert_eq!(invocation.engine_kind, EngineKind::Claude);
        assert_eq!(invocation.model.as_deref(), Some("sonnet"));
        assert_eq!(invocation.effort.as_deref(), Some("medium"));
        assert_eq!(invocation.command, "claude");
    }

    #[test]
    fn test_resolve_engine_for_tier_custom_routing() {
        let mut engines = default_engines();
        engines.get_mut("aider").unwrap().enabled = true;

        let routing = RoutingConfig {
            tier1: Some(TierRouting {
                engine: "aider".to_string(),
                model: Some("qwen-coder-30b".to_string()),
                effort: None,
            }),
            ..default_routing()
        };

        let config = EngineRoutingConfig { engines, routing };
        let tier = ClaudeTier::with_timeout("haiku", "low", 0.3);
        let invocation = resolve_engine_for_tier(&tier, &config).unwrap();
        assert_eq!(invocation.engine_kind, EngineKind::Aider);
        assert_eq!(invocation.model.as_deref(), Some("qwen-coder-30b"));
        assert_eq!(invocation.command, "aider");
    }

    #[test]
    fn test_resolve_engine_missing_engine_config() {
        let config = EngineRoutingConfig {
            engines: HashMap::new(),
            routing: default_routing(),
        };
        let tier = ClaudeTier::with_timeout("sonnet", "medium", 0.5);
        assert!(resolve_engine_for_tier(&tier, &config).is_err());
    }

    #[test]
    fn test_validate_engines_default_ok() {
        // Default config only references claude, which is enabled.
        // But claude binary may not be in PATH during tests, so we test with echo.
        let mut engines = HashMap::new();
        engines.insert(
            "echo".to_string(),
            EngineConfig {
                command: "echo".to_string(),
                args: Vec::new(),
                enabled: true,
                prompt_flag: "-p".to_string(),
                prompt_via_stdin: false,
            },
        );
        let routing = RoutingConfig {
            tier1: Some(TierRouting {
                engine: "echo".to_string(),
                model: None,
                effort: None,
            }),
            tier2: None,
            tier3: None,
            tier4: None,
        };
        let config = EngineRoutingConfig { engines, routing };
        assert!(validate_engines(&config).is_ok());
    }

    #[test]
    fn test_validate_engines_disabled_engine_fails() {
        let config = EngineRoutingConfig {
            engines: default_engines(),
            routing: RoutingConfig {
                tier1: Some(TierRouting {
                    engine: "aider".to_string(),
                    model: None,
                    effort: None,
                }),
                ..Default::default()
            },
        };
        let result = validate_engines(&config);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("disabled"));
    }

    #[test]
    fn test_validate_engines_missing_command_fails() {
        let mut engines = default_engines();
        engines.get_mut("aider").unwrap().enabled = true;
        engines.get_mut("aider").unwrap().command = "nonexistent-binary-xyz".to_string();

        let config = EngineRoutingConfig {
            engines,
            routing: RoutingConfig {
                tier1: Some(TierRouting {
                    engine: "aider".to_string(),
                    model: None,
                    effort: None,
                }),
                ..Default::default()
            },
        };
        let result = validate_engines(&config);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("not found in PATH"));
    }

    #[test]
    fn test_run_engine_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let invocation = EngineInvocation {
            engine_kind: EngineKind::Claude,
            command: "echo".to_string(),
            base_args: Vec::new(),
            model: None,
            effort: None,
            prompt_flag: "-p".to_string(),
            prompt_via_stdin: false,
        };
        let result = run_engine(tmp.path(), "hello", 10, false, false, &invocation).unwrap();
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_run_engine_via_stdin() {
        let tmp = tempfile::tempdir().unwrap();
        let invocation = EngineInvocation {
            engine_kind: EngineKind::Gemini,
            command: "cat".to_string(),
            base_args: Vec::new(),
            model: None,
            effort: None,
            prompt_flag: String::new(),
            prompt_via_stdin: true,
        };
        let result = run_engine(tmp.path(), "hello from stdin", 10, false, false, &invocation).unwrap();
        assert!(result.contains("hello from stdin"));
    }

    #[test]
    fn test_run_engine_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        // Use `sh -c sleep\ 30` via stdin mode so no extra CLI args disrupt the command.
        // sleep ignores stdin, so it will genuinely sleep until the 1s timeout fires.
        let invocation = EngineInvocation {
            engine_kind: EngineKind::Claude,
            command: "sh".to_string(),
            base_args: vec!["-c".to_string(), "sleep 30".to_string()],
            model: None,
            effort: None,
            prompt_flag: String::new(),
            prompt_via_stdin: true,
        };
        let result = run_engine(tmp.path(), "test", 1, false, false, &invocation);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("timed out"),
            "Expected 'timed out' in error but got: {}",
            err_msg
        );
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        assert_eq!(truncate_str("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_str_utf8_safe() {
        // Ensure we don't panic on multi-byte characters
        let s = "café résumé";
        let result = truncate_str(s, 4);
        assert_eq!(result, "café...");
    }

    #[test]
    fn test_engine_config_default() {
        let config = EngineRoutingConfig::default();
        assert!(config.engines.contains_key("claude"));
        assert!(config.routing.tier1.is_some());
    }
}
