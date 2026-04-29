mod coverage;
mod dedup;
mod deterministic;
mod fix_loop;
pub(crate) mod grouping;
pub(crate) mod helpers;
mod overlap;
mod parallel;
pub(crate) mod risk_assessment;
pub(crate) mod worktree_pool;

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, warn};

use crate::claude;
use crate::config::ValidatedConfig;
use crate::execution_log::{ExecutionLog, ItemStatus};
use crate::git;
use crate::report::{self, FixStatus, IssueResult};
use crate::runner;
use crate::sonar::{self, Issue, SonarClient};

use helpers::*;

pub struct Orchestrator {
    pub(crate) config: ValidatedConfig,
    pub(crate) client: SonarClient,
    pub(crate) results: Vec<IssueResult>,
    /// Total issues found in SonarQube (before --max-issues filter)
    pub(crate) total_issues_found: usize,
    /// Prompt configuration from YAML (US-019)
    pub(crate) prompt_config: crate::yaml_config::PromptsYaml,
    /// Execution state for resume support (US-017)
    pub(crate) exec_state: Option<crate::state::ExecutionState>,
    /// Rule description cache (US-020): rule_key → description
    pub(crate) rule_cache: std::collections::HashMap<String, String>,
    /// Engine routing configuration for multi-engine AI dispatch
    pub(crate) engine_routing: crate::engine::EngineRoutingConfig,
    /// Cached test examples (computed once, reused across issues)
    pub(crate) cached_test_examples: Option<String>,
    /// Real-time execution log (SQLite). Shared with the signal handler.
    /// This is the single source of truth for token usage and phase metrics —
    /// the end-of-run summary table is computed directly from the database.
    pub(crate) exec_log: Arc<ExecutionLog>,
    /// Currently active phase id in the execution log (if any).
    pub(crate) current_phase_id: std::sync::Mutex<Option<i64>>,
    /// Currently active step id in the execution log (if any).
    /// Set by `exec_step_start`, cleared by `exec_step_finish`, used by `run_ai_at`
    /// to link AI calls to their enclosing step so the steps table shows real token counts.
    pub(crate) current_step_id: std::sync::Mutex<Option<i64>>,
    /// US-065: cache of `classify_source_file` results keyed by relative path.
    /// Avoids re-reading and re-classifying the same file on each phase/pass.
    pub(crate) file_classifications: std::sync::RwLock<std::collections::HashMap<String, String>>,
    /// Monotonic counter of fixes processed. Used with `rescan_batch_size` to
    /// decide when to run the SonarQube verification rescan (every Nth fix).
    pub(crate) fix_counter: std::sync::atomic::AtomicU32,
    /// Whether the next fix must run the `clean` command before fixing.
    /// Set to true on startup and after any build/test failure; set to false
    /// after a successful fix so subsequent fixes skip the ~2-3 s clean step
    /// (see `config.skip_clean_when_safe`).
    pub(crate) needs_clean: std::sync::atomic::AtomicBool,
    /// (file, rule) → consecutive non-Fixed outcomes seen during this run.
    /// Once a (file, rule) pair fails ≥ FAILURE_MEMORY_THRESHOLD times, further
    /// issues with the same key short-circuit straight to NeedsReview without
    /// spending an AI call. Reason: when one issue on a file/rule fails the
    /// targeted tests, the next sibling issue almost always fails the same way
    /// (e.g. `EntityServiceImpl + java:S3740` repeatedly broke
    /// `EntityServiceImplAdditionalCoverageTest` across 6 issues = ~50 min wasted).
    /// Shared `Arc` so every parallel worker sees the same counter.
    pub(crate) fix_failure_memory:
        Arc<std::sync::Mutex<std::collections::HashMap<(String, String), u32>>>,
    /// US-081: file_path → live Claude session that can be resumed.
    /// First AI call on a file opens a fresh session and captures the session
    /// id from the JSON response; subsequent calls on the same file pass that
    /// id via `--resume <id>` so Claude continues the conversation instead of
    /// re-loading system prompt + CLAUDE.md. Sessions are evicted when the
    /// turn count or wall age caps are exceeded, when a fix is reverted (the
    /// conversation contains code that was rolled back and would mislead the
    /// next turn), or when the worker moves to a different file.
    /// Shared `Arc` across parallel workers so two issues on the same file
    /// dispatched to different worktrees can still share the session — the
    /// content of the file is replicated across worktrees so the model's
    /// understanding remains valid.
    pub(crate) session_map:
        Arc<std::sync::Mutex<std::collections::HashMap<String, SessionEntry>>>,
}

/// Live Claude session associated with a single source file.
#[derive(Debug, Clone)]
pub(crate) struct SessionEntry {
    pub id: String,
    pub turns: u32,
    pub opened_at: std::time::Instant,
}

/// Hard caps on session reuse. Sessions get evicted when EITHER cap is hit.
/// `MAX_TURNS` keeps the conversation under ~50–100 K context tokens; the
/// time cap stays comfortably under the 5-min Anthropic prompt-cache TTL so
/// a fresh session's first call still hits warm cache.
pub(crate) const SESSION_MAX_TURNS: u32 = 10;
pub(crate) const SESSION_MAX_AGE_SECS: u64 = 240; // 4 minutes (cache TTL is 5)

/// Threshold of (file, rule) failures after which subsequent siblings are
/// short-circuited to NeedsReview. Empirically (run 2026-04-28) the first
/// failure on a (file, rule) pair already predicts subsequent ones reliably:
/// `S3740` on `EntityServiceImpl` failed 6 in a row, `S6813` on
/// `EmailServiceImpl` consumed ~5 min twice returning "no changes". Cutting
/// after the first failure trades ~one wasted issue-attempt per pair for the
/// guarantee that no run wastes ≥ 30 min on a single rule/file combination.
pub(crate) const FAILURE_MEMORY_THRESHOLD: u32 = 1;

impl Orchestrator {
    pub fn new(config: ValidatedConfig, exec_log: Arc<ExecutionLog>) -> Result<Self> {
        let client = SonarClient::new(&config);

        // US-019: Load prompt config from YAML
        let prompt_config = crate::yaml_config::load_yaml_config(
            &config.path,
            None,
        )?
        .map(|y| y.prompts)
        .unwrap_or_default();

        // US-017: Load existing state if resuming
        let exec_state = if config.resume {
            match crate::state::load_state(&config.path)? {
                Some(state) => {
                    if state.is_compatible(&config.sonar_project_id, &config.branch) {
                        info!(
                            "Resuming: {} issues already processed",
                            state.processed.len()
                        );
                        Some(state)
                    } else {
                        warn!("State file exists but config changed (project/branch). Starting fresh.");
                        None
                    }
                }
                None => {
                    info!("No previous state found. Starting fresh.");
                    None
                }
            }
        } else {
            None
        };

        let engine_routing = config.engine_routing.clone();

        Ok(Self {
            config,
            client,
            results: Vec::new(),
            total_issues_found: 0,
            prompt_config,
            exec_state,
            rule_cache: std::collections::HashMap::new(),
            engine_routing,
            cached_test_examples: None,
            exec_log,
            current_phase_id: std::sync::Mutex::new(None),
            current_step_id: std::sync::Mutex::new(None),
            file_classifications: std::sync::RwLock::new(std::collections::HashMap::new()),
            fix_counter: std::sync::atomic::AtomicU32::new(0),
            needs_clean: std::sync::atomic::AtomicBool::new(true),
            fix_failure_memory: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_map: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Create a lightweight worker Orchestrator for parallel issue processing (US-018).
    ///
    /// Shares SonarQube client, rule cache, engine routing, and prompt config with the parent.
    /// The worker uses a different project path (a git worktree) and a shared usage tracker.
    /// `fix_failure_memory` is cloned from the parent `Arc` so all workers in a run
    /// see the same (file, rule) failure counters.
    pub(crate) fn new_worker(
        config: ValidatedConfig,
        client: SonarClient,
        rule_cache: std::collections::HashMap<String, String>,
        engine_routing: crate::engine::EngineRoutingConfig,
        prompt_config: crate::yaml_config::PromptsYaml,
        cached_test_examples: Option<String>,
        exec_log: Arc<ExecutionLog>,
        fix_failure_memory: Arc<
            std::sync::Mutex<std::collections::HashMap<(String, String), u32>>,
        >,
        session_map: Arc<
            std::sync::Mutex<std::collections::HashMap<String, SessionEntry>>,
        >,
    ) -> Self {
        Self {
            config,
            client,
            results: Vec::new(),
            total_issues_found: 0,
            prompt_config,
            exec_state: None,
            rule_cache,
            engine_routing,
            cached_test_examples,
            exec_log,
            current_phase_id: std::sync::Mutex::new(None),
            current_step_id: std::sync::Mutex::new(None),
            file_classifications: std::sync::RwLock::new(std::collections::HashMap::new()),
            fix_counter: std::sync::atomic::AtomicU32::new(0),
            needs_clean: std::sync::atomic::AtomicBool::new(true),
            fix_failure_memory,
            session_map,
        }
    }

    /// US-065: cached wrapper around `runner::classify_source_file`.
    ///
    /// The cache is keyed by the file path (relative to the main project); a
    /// file's classification depends only on its content, which doesn't change
    /// during a single run.  Used by all main-tree code paths in coverage boost
    /// and fix loop.  The parallel worktree path classifies from its own
    /// worktree copy and does not use this cache.
    pub(crate) fn classify_source_file_cached(&self, file: &str) -> String {
        if let Ok(cache) = self.file_classifications.read() {
            if let Some(c) = cache.get(file) {
                return c.clone();
            }
        }
        let result = crate::runner::classify_source_file(file, &self.config.path);
        if let Ok(mut cache) = self.file_classifications.write() {
            cache.insert(file.to_string(), result.clone());
        }
        result
    }

    /// Generate a partial report from whatever results are available (US-012).
    /// Called when global timeout is reached or execution is interrupted.
    pub fn generate_partial_report(&self) {
        info!("Generating partial report with {} results so far", self.results.len());
        report::generate_report(
            &self.config.path,
            &self.results,
            self.total_issues_found,
            0, // elapsed unknown in timeout case
        );
    }

    /// Run an AI engine against the main project directory.
    ///
    /// Shorthand for `run_ai_at(step, prompt, tier, &self.config.path)`.
    pub(crate) fn run_ai(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
    ) -> anyhow::Result<String> {
        self.run_ai_at(step, prompt, tier, &self.config.path)
    }

    /// US-086/087: variant of `run_ai` that pins session reuse to a file path
    /// (typically the test file or source file under edit). Used by
    /// coverage_boost, documentation, test_generation, contract_test_generation
    /// and rebase_conflict so consecutive AI calls on the same file amortise
    /// the system-prompt + CLAUDE.md + initial file-read cost.
    pub(crate) fn run_ai_keyed(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        session_key: Option<&str>,
    ) -> anyhow::Result<String> {
        self.run_ai_at_keyed(step, prompt, tier, &self.config.path, session_key)
    }

    /// Lean variant of run_ai for focused single-file fixes.
    /// Adds `--tools Read,Edit,Write` (no Grep/Glob/Bash) to the Claude
    /// invocation to restrict the fix to a targeted file edit.
    /// Falls back to the default invocation when `lean_ai` config is disabled
    /// or when the resolved engine isn't Claude.
    ///
    /// `--bare` was previously added here but was removed because it also
    /// bypasses auth loading, causing "Not logged in" failures in sessions
    /// that rely on Claude Code's shared auth.
    /// Lean variant of `run_ai` for focused single-file fixes; pins session
    /// reuse to `session_key` so successive fixes on it amortise the system
    /// prompt + initial file-read cost.
    pub(crate) fn run_ai_lean_keyed(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        session_key: Option<&str>,
    ) -> anyhow::Result<String> {
        if !self.config.lean_ai {
            return self.run_ai(step, prompt, tier);
        }
        self.run_ai_lean_at_keyed(step, prompt, tier, &self.config.path, session_key)
    }

    /// Lean run with explicit `project_path` (may be a worktree); participates
    /// in the per-file Claude session reuse map (US-081). `session_key` is
    /// the file path of the issue being fixed; `None` opts out (use for
    /// non-fix calls like risk assessment or coverage boost).
    pub(crate) fn run_ai_lean_at_keyed(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        project_path: &std::path::Path,
        session_key: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut invocation = crate::engine::resolve_engine_for_tier(tier, &self.engine_routing)?;
        if matches!(invocation.engine_kind, crate::engine::EngineKind::Claude) {
            invocation.extra_args.extend([
                "--tools".to_string(),
                "Read,Edit,Write".to_string(),
            ]);
        }
        self.run_ai_with_invocation_keyed(step, prompt, tier, project_path, invocation, session_key)
    }

    /// US-081: drop any live session associated with this file. Called by
    /// fix_loop after a revert (the conversation contains rolled-back code
    /// and would mislead the next turn) and at the end of a successful but
    /// turn-limit-reaching fix.
    pub(crate) fn evict_session(&self, file: &str) {
        let mut map = self.session_map.lock().expect("session_map poisoned");
        if let Some(e) = map.remove(file) {
            tracing::info!(
                "Evicted Claude session {} for {} (turns={})",
                e.id, file, e.turns
            );
        }
    }

    /// Drop any live session for `file` at issue-boundary. Each call to
    /// `process_issue` is for a distinct SonarQube issue, and resuming a
    /// session that was opened for a previous issue lets Claude believe the
    /// work is already done — observed in run 2026-04-28 waves 17 & 18,
    /// where two consecutive issues on `EmailServiceImpl` returned "Claude
    /// made no changes" after wasting ~3 min each. Repair cycles within
    /// the same `process_issue_impl` still benefit from session reuse
    /// because they happen between this evict and the next.
    pub(crate) fn evict_session_for_new_issue(&self, file: &str, incoming_issue_key: &str) {
        let mut map = self.session_map.lock().expect("session_map poisoned");
        if let Some(e) = map.remove(file) {
            tracing::info!(
                "Evicting Claude session {} for {} at issue boundary (new_issue={}, turns={})",
                e.id, file, incoming_issue_key, e.turns
            );
        }
    }

    /// Run an AI engine with an explicit `project_path` (may be a worktree).
    ///
    /// Resolves which engine to use from the routing config, computes timeout,
    /// dispatches to `engine::run_engine_full`, and stores a `UsageEntry` keyed
    /// by `step` so the end-of-run table can attribute tokens correctly.
    pub(crate) fn run_ai_at(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        project_path: &std::path::Path,
    ) -> anyhow::Result<String> {
        self.run_ai_at_keyed(step, prompt, tier, project_path, None)
    }

    pub(crate) fn run_ai_at_keyed(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        project_path: &std::path::Path,
        session_key: Option<&str>,
    ) -> anyhow::Result<String> {
        let invocation = crate::engine::resolve_engine_for_tier(tier, &self.engine_routing)?;
        self.run_ai_with_invocation_keyed(
            step,
            prompt,
            tier,
            project_path,
            invocation,
            session_key,
        )
    }

    /// Run an AI engine with an explicit invocation, participating in the
    /// per-file Claude session reuse map (US-081). When `session_key` is
    /// `Some(file)`, the orchestrator looks up a live session for that file,
    /// applies `--resume <id>` to the invocation, captures the new session id
    /// from the response, and updates the bookkeeping (turns + age). On any
    /// error or non-Claude engine, falls back to the legacy fresh-session
    /// behaviour transparently — failures here must NEVER abort the issue.
    pub(crate) fn run_ai_with_invocation_keyed(
        &self,
        step: &str,
        prompt: &str,
        tier: &claude::ClaudeTier,
        project_path: &std::path::Path,
        mut invocation: crate::engine::EngineInvocation,
        session_key: Option<&str>,
    ) -> anyhow::Result<String> {
        // Resolve session reuse before computing timeout — whether we resume
        // or open fresh doesn't change the timeout policy.
        let session_active = matches!(
            invocation.engine_kind,
            crate::engine::EngineKind::Claude
        ) && session_key.is_some();
        if session_active {
            if let Some(file) = session_key {
                let entry = {
                    let map = self.session_map.lock().expect("session_map poisoned");
                    map.get(file).cloned()
                };
                if let Some(e) = entry {
                    let age = e.opened_at.elapsed().as_secs();
                    if e.turns < SESSION_MAX_TURNS && age < SESSION_MAX_AGE_SECS {
                        invocation.session_id = Some(e.id.clone());
                        tracing::info!(
                            "Reusing Claude session {} for {} (turn {}/{}, age {}s)",
                            e.id, file, e.turns + 1, SESSION_MAX_TURNS, age
                        );
                    } else {
                        // Aged out / over turn cap → drop and let a fresh
                        // session open below.
                        let mut map = self.session_map.lock().expect("session_map poisoned");
                        map.remove(file);
                        tracing::info!(
                            "Evicting Claude session {} for {} (turns={} age={}s)",
                            e.id, file, e.turns, age
                        );
                    }
                }
            }
        }

        let tier_timeout = tier.effective_timeout(self.config.claude_timeout);

        // Prompt-aware timeout floor: larger prompts indicate more complex tasks
        // requiring more reasoning and output generation time.  Use 120s base +
        // ~100ms per prompt character, capped at 3× the configured base timeout.
        //
        // Skip the floor on `fix_*_error` repair steps. Empirically (run
        // 2026-04-28) successful repairs complete in 17–58 s; the floor was
        // boosting tier_timeout from 240 s up to 400 s for typical 2.8 KB
        // repair prompts and that extra 160 s was burnt almost exclusively
        // by stuck loops that the 80%-budget fast-fail couldn't catch in time.
        // Repair tiers already encode their reasoning needs via the
        // multiplier; the floor is appropriate for first-shot fix prompts
        // where the model loads more file context.
        let is_repair_step = step.starts_with("fix_") && step.ends_with("_error");
        let prompt_floor = if is_repair_step {
            0
        } else {
            ((prompt.len() as u64) / 10 + 120)
                .min(self.config.claude_timeout.saturating_mul(3))
        };
        let timeout = tier_timeout.max(prompt_floor);

        if timeout > tier_timeout {
            tracing::info!(
                "Timeout boosted by prompt size: {}s → {}s (prompt {} chars)",
                tier_timeout, timeout, prompt.len()
            );
        }

        let model_label = invocation.model.clone().unwrap_or_else(|| "default".to_string());
        let engine_kind = invocation.engine_kind.clone();
        let effort_label = invocation.effort.clone();

        let call_started = std::time::Instant::now();
        let result = crate::engine::run_engine_full(
            project_path,
            prompt,
            timeout,
            self.config.dangerously_skip_permissions,
            self.config.show_prompts,
            &invocation,
        );
        let call_duration_ms = call_started.elapsed().as_millis() as u64;

        // US-081: persist the session id reported by Claude for follow-up
        // calls on the same file. We only update the map on success — a
        // failed call may have died before fully establishing context.
        if session_active {
            if let (Some(file), Ok(ref out)) = (session_key, &result) {
                if let Some(ref new_sid) = out.session_id {
                    let mut map = self.session_map.lock().expect("session_map poisoned");
                    let entry = map.entry(file.to_string())
                        .or_insert_with(|| SessionEntry {
                            id: new_sid.clone(),
                            turns: 0,
                            opened_at: std::time::Instant::now(),
                        });
                    // Claude returns a stable session id across turns of the
                    // same conversation, but cope defensively if it changes.
                    if entry.id != *new_sid {
                        entry.id = new_sid.clone();
                        entry.opened_at = std::time::Instant::now();
                        entry.turns = 0;
                    }
                    entry.turns = entry.turns.saturating_add(1);
                }
            }
        }

        // Record usage directly in the execution log (SQLite).  Failed calls
        // still cost tokens if the engine got far enough to report them; on
        // outright spawn/timeout errors the output has no usage, so we skip.
        if let Ok(ref out) = result {
            let (usage, unknown) = match out.usage {
                Some(u) => (u, false),
                None => (crate::usage::TokenUsage::default(), true),
            };
            let entry = crate::usage::UsageEntry {
                step: step.to_string(),
                engine: engine_kind,
                model: model_label,
                usage,
                unknown,
            };
            let current_phase = *self.current_phase_id.lock().unwrap();
            let current_step = *self.current_step_id.lock().unwrap();
            if let Err(e) = self.exec_log.log_ai_call(
                current_phase,
                current_step,
                &entry,
                effort_label.as_deref(),
                Some(call_duration_ms),
            ) {
                tracing::debug!("execution_log: log_ai_call failed: {}", e);
            }
        }

        result.map(|o| o.stdout)
    }

    // === Execution log helpers ===

    /// Open a new phase in the execution log and remember its id as "current" so
    /// subsequent `run_ai` calls get associated with it automatically.
    pub(crate) fn exec_phase_start(
        &self,
        name: &str,
        metric_before: Option<f64>,
        unit: Option<&str>,
    ) -> Option<i64> {
        match self.exec_log.start_phase(name, metric_before, unit) {
            Ok(id) => {
                *self.current_phase_id.lock().unwrap() = Some(id);
                Some(id)
            }
            Err(e) => {
                tracing::debug!("execution_log: start_phase({}) failed: {}", name, e);
                None
            }
        }
    }

    pub(crate) fn exec_phase_finish(
        &self,
        phase_id: Option<i64>,
        status: ItemStatus,
        metric_after: Option<f64>,
        details: Option<&str>,
    ) {
        if let Some(id) = phase_id {
            if let Err(e) = self
                .exec_log
                .finish_phase(id, status, metric_after, details)
            {
                tracing::debug!("execution_log: finish_phase failed: {}", e);
            }
            let mut cur = self.current_phase_id.lock().unwrap();
            if *cur == Some(id) {
                *cur = None;
            }
        }
    }

    pub(crate) fn exec_step_start(
        &self,
        phase_id: Option<i64>,
        name: &str,
        target: Option<&str>,
        metric_before: Option<f64>,
    ) -> Option<i64> {
        let pid = phase_id?;
        match self.exec_log.start_step(pid, name, target, metric_before) {
            Ok(id) => {
                *self.current_step_id.lock().unwrap() = Some(id);
                Some(id)
            }
            Err(e) => {
                tracing::debug!("execution_log: start_step({}) failed: {}", name, e);
                None
            }
        }
    }

    pub(crate) fn exec_step_finish(
        &self,
        step_id: Option<i64>,
        status: ItemStatus,
        metric_after: Option<f64>,
        details: Option<&str>,
    ) {
        if let Some(id) = step_id {
            if let Err(e) = self
                .exec_log
                .finish_step(id, status, metric_after, details)
            {
                tracing::debug!("execution_log: finish_step failed: {}", e);
            }
            let mut cur = self.current_step_id.lock().unwrap();
            if *cur == Some(id) {
                *cur = None;
            }
        }
    }

    /// Run the full Reparo flow (US-010).
    ///
    /// Returns an exit code:
    /// - 0: all issues fixed (or none found, or dry-run)
    /// - 1: fatal error (config, connectivity)
    /// - 2: partial success (some fixed, some failed)
    pub async fn run(&mut self) -> Result<i32> {
        let start = Instant::now();

        // Step 0: Ensure clean git working tree (or absorb WIP changes if allowed)
        info!("=== Step 0: Checking git status ===");
        match git::has_changes(&self.config.path) {
            Ok(true) => {
                if self.config.allow_wip {
                    // --allow-wip: stage all pending changes so the first commit
                    // Reparo creates (format/coverage/fix) folds them in.
                    let wip_files = git::changed_files(&self.config.path)
                        .unwrap_or_default();
                    warn!(
                        "Working tree has {} uncommitted change(s) — running in --allow-wip mode. \
                         Pending changes will be absorbed into the first commit Reparo creates.",
                        wip_files.len()
                    );
                    for f in wip_files.iter().take(20) {
                        warn!("  WIP: {}", f);
                    }
                    if wip_files.len() > 20 {
                        warn!("  ... and {} more", wip_files.len() - 20);
                    }
                    git::add_all(&self.config.path)
                        .context("Failed to stage WIP changes for --allow-wip mode")?;
                    info!("WIP changes staged and will be included in the first commit");
                } else {
                    anyhow::bail!(
                        "Working tree has uncommitted changes. Commit or stash them before running Reparo,\n\
                         or pass --allow-wip to absorb them into the first commit.\n\
                         Run `git status` in {} to see what's changed.",
                        self.config.path.display()
                    );
                }
            }
            Ok(false) => {
                info!("Git working tree is clean");
            }
            Err(e) => {
                warn!("Could not check git status: {} — proceeding anyway", e);
            }
        }

        // Step 0b: Preflight — build and tests MUST pass before anything else.
        // Detect test command here (will also be used later in the main flow).
        {
            let preflight_test_cmd = self.config.test_command.clone()
                .or_else(|| self.config.commands.test.clone())
                .or_else(|| runner::detect_test_command(&self.config.path));

            let preflight_build_cmd = self.config.commands.build.clone();

            info!("=== Step 0b: Preflight build + test validation ===");

            // Preflight is flaky for Maven (~/.m2 lock contention, transient network),
            // so retry transient failures up to PREFLIGHT_MAX_ATTEMPTS times before
            // aborting. A real broken build will fail consistently across attempts.
            const PREFLIGHT_MAX_ATTEMPTS: u32 = 3;
            const PREFLIGHT_BACKOFF_SECS: u64 = 5;

            if let Some(ref build_cmd) = preflight_build_cmd {
                info!("Preflight: running build...");
                let mut last_failure: Option<String> = None;
                let mut last_error: Option<anyhow::Error> = None;
                let mut succeeded = false;
                for attempt in 1..=PREFLIGHT_MAX_ATTEMPTS {
                    match runner::run_shell_command(&self.config.path, build_cmd, "preflight build") {
                        Ok((true, _)) => {
                            if attempt > 1 {
                                warn!("✓ Preflight build passed on attempt {}/{} — first attempt was flaky", attempt, PREFLIGHT_MAX_ATTEMPTS);
                            } else {
                                info!("✓ Preflight build passed");
                            }
                            succeeded = true;
                            break;
                        }
                        Ok((false, output)) => {
                            last_failure = Some(output);
                            if attempt < PREFLIGHT_MAX_ATTEMPTS {
                                warn!(
                                    "Preflight build failed on attempt {}/{} — retrying in {}s (Maven flakes are common)",
                                    attempt, PREFLIGHT_MAX_ATTEMPTS, PREFLIGHT_BACKOFF_SECS
                                );
                                std::thread::sleep(std::time::Duration::from_secs(PREFLIGHT_BACKOFF_SECS));
                            }
                        }
                        Err(e) => {
                            last_error = Some(anyhow::anyhow!("{}", e));
                            if attempt < PREFLIGHT_MAX_ATTEMPTS {
                                warn!(
                                    "Preflight build error on attempt {}/{}: {} — retrying in {}s",
                                    attempt, PREFLIGHT_MAX_ATTEMPTS, e, PREFLIGHT_BACKOFF_SECS
                                );
                                std::thread::sleep(std::time::Duration::from_secs(PREFLIGHT_BACKOFF_SECS));
                            }
                        }
                    }
                }
                if !succeeded {
                    error!("╔═══════════════════════════════════════════════════════════════╗");
                    error!("║            ✗  PREFLIGHT BUILD FAILED — ABORTING  ✗           ║");
                    error!("║  Fix the build before running Reparo. Nothing was modified.  ║");
                    error!("║  (failed {} consecutive attempts)                              ║", PREFLIGHT_MAX_ATTEMPTS);
                    error!("╚═══════════════════════════════════════════════════════════════╝");
                    if let Some(output) = last_failure {
                        error!("Build output:\n{}", truncate_tail(&output, 3000));
                        anyhow::bail!("Preflight build failed after {} attempts — project does not compile.", PREFLIGHT_MAX_ATTEMPTS);
                    } else if let Some(e) = last_error {
                        anyhow::bail!("Preflight build error after {} attempts: {}", PREFLIGHT_MAX_ATTEMPTS, e);
                    }
                }
            }

            if let Some(ref test_cmd) = preflight_test_cmd {
                info!("Preflight: running test suite...");
                let mut last_failure: Option<String> = None;
                let mut last_error: Option<anyhow::Error> = None;
                let mut succeeded = false;
                for attempt in 1..=PREFLIGHT_MAX_ATTEMPTS {
                    match runner::run_tests(&self.config.path, test_cmd, self.config.test_timeout) {
                        Ok((true, _)) => {
                            if attempt > 1 {
                                warn!("✓ Preflight tests passed on attempt {}/{} — first attempt was flaky", attempt, PREFLIGHT_MAX_ATTEMPTS);
                            } else {
                                info!("✓ Preflight tests passed");
                            }
                            succeeded = true;
                            break;
                        }
                        Ok((false, output)) => {
                            last_failure = Some(output);
                            if attempt < PREFLIGHT_MAX_ATTEMPTS {
                                warn!(
                                    "Preflight tests failed on attempt {}/{} — retrying in {}s",
                                    attempt, PREFLIGHT_MAX_ATTEMPTS, PREFLIGHT_BACKOFF_SECS
                                );
                                std::thread::sleep(std::time::Duration::from_secs(PREFLIGHT_BACKOFF_SECS));
                            }
                        }
                        Err(e) => {
                            last_error = Some(anyhow::anyhow!("{}", e));
                            if attempt < PREFLIGHT_MAX_ATTEMPTS {
                                warn!(
                                    "Preflight tests error on attempt {}/{}: {} — retrying in {}s",
                                    attempt, PREFLIGHT_MAX_ATTEMPTS, e, PREFLIGHT_BACKOFF_SECS
                                );
                                std::thread::sleep(std::time::Duration::from_secs(PREFLIGHT_BACKOFF_SECS));
                            }
                        }
                    }
                }
                if !succeeded {
                    error!("╔═══════════════════════════════════════════════════════════════╗");
                    error!("║            ✗  PREFLIGHT TESTS FAILED — ABORTING  ✗           ║");
                    error!("║  Fix failing tests before running Reparo. Nothing modified.  ║");
                    error!("║  (failed {} consecutive attempts)                              ║", PREFLIGHT_MAX_ATTEMPTS);
                    error!("╚═══════════════════════════════════════════════════════════════╝");
                    if let Some(output) = last_failure {
                        error!("Test output:\n{}", truncate_tail(&output, 3000));
                        anyhow::bail!("Preflight tests failed after {} attempts.", PREFLIGHT_MAX_ATTEMPTS);
                    } else if let Some(e) = last_error {
                        anyhow::bail!("Preflight test error after {} attempts: {}", PREFLIGHT_MAX_ATTEMPTS, e);
                    }
                }
            } else {
                warn!("No test command detected for preflight check. Use --test-command to configure one.");
            }
        }

        // Step 0.5: Cache test examples once (avoids re-globbing per issue)
        self.cached_test_examples = Some(runner::find_test_examples(&self.config.path).join("\n\n"));

        // Step 0.5: Validate pact configuration unless the user opted out.
        // validate() hard-errors when the section is missing or required commands
        // are absent — the program exits before any Sonar work happens.
        if !self.config.skip_pact {
            self.config.pact.validate()?;

            if self.config.pact.enabled {
                // Detect and log framework info
                let fw_info = crate::pact::detect_pact_framework_info(&self.config.path);
                if fw_info.name == "unknown" {
                    warn!("Could not detect pact framework — Claude will infer from project context");
                } else if !fw_info.installed {
                    warn!(
                        "Pact framework '{}' declared but may not be installed. Run: {}",
                        fw_info.name, fw_info.install_hint
                    );
                } else {
                    info!("Detected pact framework: {} (installed)", fw_info.name);
                }

                // Detect project role for better prompts
                let role = crate::pact::detect_project_role(&self.config.path);
                info!("Detected project role: {:?}", role);
            }
        }

        // Step 1: Validate SonarQube connectivity (US-001, US-016: with retry)
        info!("=== Step 1: Checking SonarQube connectivity ===");
        crate::retry::retry_async(3, 3, "SonarQube connection check", || {
            self.client.check_connection()
        }).await?;

        self.client.detect_edition().await;

        // Detect test command early — needed for pre-flight and processing
        // Priority: CLI --test-command > YAML commands.test > auto-detection
        let test_command = self.config.test_command.clone()
            .or_else(|| self.config.commands.test.clone())
            .or_else(|| runner::detect_test_command(&self.config.path));
        let test_command = match test_command {
            Some(cmd) => cmd,
            None => {
                warn!("Could not detect test command. Use --test-command to specify one.");
                warn!("Continuing without test validation.");
                String::new()
            }
        };

        // Step 2: Create fix branch from current branch (whatever it is)
        info!("=== Step 2: Creating fix branch ===");
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let branch_name = format!("fix/sonar-{}", ts);

        if let Err(e) = git::create_branch(&self.config.path, &branch_name, &self.config.branch) {
            error!("Failed to create branch {}: {}", branch_name, e);
            anyhow::bail!("Cannot create fix branch: {}", e);
        }
        info!("Created branch: {} (from {})", branch_name, self.config.branch);

        // Step 2a: Setup — run setup command (e.g., npm install) before anything else
        if let Some(ref setup_cmd) = self.config.commands.setup {
            info!("=== Step 2a: Setup ===");
            info!("Running setup: {}", setup_cmd);
            match runner::run_shell_command(&self.config.path, setup_cmd, "setup") {
                Ok((true, _)) => info!("Setup completed successfully"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Setup command failed:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Setup command error: {}", e);
                }
            }
        }

        // Step 2b: Initial formatting — run formatter and commit separately before fixes
        if self.config.skip_format {
            info!("=== Step 2b: Initial format SKIPPED (--skip-format) ===");
        } else if let Some(ref fmt_cmd) = self.config.commands.format {
            info!("=== Step 2b: Initial formatting ===");
            match runner::run_shell_command(&self.config.path, fmt_cmd, "initial format") {
                Ok((true, _)) => {
                    info!("Formatter ran successfully");
                    // Check if formatting produced any changes
                    match git::has_changes(&self.config.path) {
                        Ok(true) => {
                            info!("Formatting produced changes — committing...");
                            if let Err(e) = git::commit_all(
                                &self.config.path,
                                &format_commit_message(&self.config, "style", "sonar", "apply code formatting before sonar fixes", "", "", ""),
                            ) {
                                warn!("Failed to commit formatting changes: {}", e);
                            } else {
                                info!("Formatting changes committed");
                            }
                        }
                        Ok(false) => {
                            info!("No formatting changes needed");
                        }
                        Err(e) => {
                            warn!("Could not check git status: {}", e);
                        }
                    }
                }
                Ok((false, output)) => {
                    warn!("Formatter failed (non-blocking): {}", truncate(&output, 200));
                }
                Err(e) => {
                    warn!("Formatter error (non-blocking): {}", e);
                }
            }
        }

        // Step 3: Pre-flight checks — build and tests must pass before any fixes
        info!("=== Step 3: Pre-flight checks ===");
        if let Some(ref build_cmd) = self.config.commands.build {
            info!("Pre-flight: running build...");
            match runner::run_shell_command(&self.config.path, build_cmd, "pre-flight build") {
                Ok((true, _)) => info!("Pre-flight build passed"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight build fails — fix the build before running Reparo:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight build error: {}", e);
                }
            }
        }
        if !test_command.is_empty() {
            info!("Pre-flight: running tests...");
            match runner::run_tests(&self.config.path, &test_command, self.config.test_timeout) {
                Ok((true, _)) => info!("Pre-flight tests passed"),
                Ok((false, output)) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight tests fail — fix tests before running Reparo:\n{}", truncate_tail(&output, 2000));
                }
                Err(e) => {
                    let _ = git::checkout(&self.config.path, &self.config.branch);
                    git::delete_branch(&self.config.path, &branch_name);
                    anyhow::bail!("Pre-flight test error: {}", e);
                }
            }
        }

        // Step 3a: Test overlap detection — advisory, read-only, no AI involved.
        // Runs each test file in isolation through the coverage command, compares the
        // resulting per-file source-line sets pairwise, and emits warnings for any
        // pair that covers identical lines (flagging the one with fewer covered lines
        // as the removal candidate).  The phase is skipped when --skip-overlap is
        // passed or when no coverage command is resolvable.
        if self.config.skip_overlap {
            info!("=== Step 3a: Test overlap detection SKIPPED (--skip-overlap) ===");
        } else {
            let cov_cmd = self
                .config
                .coverage_command
                .clone()
                .or_else(|| self.config.commands.coverage.clone())
                .or_else(|| runner::detect_coverage_command(&self.config.path));

            if let Some(ref cmd) = cov_cmd {
                info!("=== Step 3a: Test overlap detection ===");
                let pid = self.exec_phase_start("test_overlap", None, None);
                match self.analyze_test_overlap(cmd) {
                    Ok(_) => self.exec_phase_finish(pid, ItemStatus::Completed, None, None),
                    Err(e) => {
                        warn!("Test overlap detection failed (non-critical): {}", e);
                        self.exec_phase_finish(
                            pid,
                            ItemStatus::Failed,
                            None,
                            Some(&format!("{}", e)),
                        );
                    }
                }
            } else {
                info!("=== Step 3a: Test overlap detection SKIPPED (no coverage command) ===");
            }
        }

        // Step 3b: Coverage boosting — generate tests until min_coverage is reached
        if self.config.skip_coverage {
            info!("=== Step 3b: Coverage boost SKIPPED (--skip-coverage) ===");
            let pid = self.exec_phase_start("coverage_boost", None, Some("%"));
            self.exec_phase_finish(pid, ItemStatus::Skipped, None, Some("--skip-coverage"));
        } else if self.config.min_coverage > 0.0 {
            let pid = self.exec_phase_start("coverage_boost", None, Some("%"));
            let result = self.boost_coverage_to_threshold(&test_command).await;
            match &result {
                Ok(_) => self.exec_phase_finish(pid, ItemStatus::Completed, None, None),
                Err(e) => self.exec_phase_finish(
                    pid,
                    ItemStatus::Failed,
                    None,
                    Some(&format!("{}", e)),
                ),
            }
            result?;
        }

        // Step 3c: Freeze baseline coverage snapshot. Captured before the fix
        // loop so every per-issue coverage check references the same immutable
        // lcov — otherwise parallel worktrees contaminate each other's view.
        {
            let pid = self.exec_phase_start("baseline_coverage_snapshot", None, None);
            match coverage::snapshot_baseline_lcov(
                &self.config.path,
                self.config.commands.coverage_report.as_deref(),
            ) {
                Some(path) => {
                    self.config.baseline_lcov_path = Some(path);
                    self.exec_phase_finish(pid, ItemStatus::Completed, None, None);
                }
                None => {
                    info!(
                        "Baseline coverage snapshot unavailable — per-issue coverage will fall back to SonarQube"
                    );
                    self.exec_phase_finish(
                        pid,
                        ItemStatus::Skipped,
                        None,
                        Some("no lcov report on disk"),
                    );
                }
            }
        }

        // Step 3d: Local linter scan. Findings are normalized into Issue
        // records with synthetic `lint:<format>:<rule>` keys and merged into
        // the fix queue alongside SonarQube issues (Step 4 below).
        let mut linter_issues: Vec<Issue> = Vec::new();
        if self.config.skip_linter_scan {
            info!("=== Step 3d: Linter scan SKIPPED (--skip-linter-scan) ===");
            let pid = self.exec_phase_start("linter_scan", None, None);
            self.exec_phase_finish(pid, ItemStatus::Skipped, None, Some("--skip-linter-scan"));
        } else {
            info!("=== Step 3d: Local linter scan ===");
            let pid = self.exec_phase_start("linter_scan", None, Some(" findings"));
            // Prefer the dedicated scan command; fall back to the per-fix
            // `lint` gate when no scan-specific command is configured. This
            // lets users keep a fast per-fix gate (mvn validate) alongside a
            // heavier scan (mvn checkstyle:checkstyle).
            let lint_cmd = self
                .config
                .commands
                .lint_scan
                .as_deref()
                .or(self.config.commands.lint.as_deref());
            let lint_format = self.config.commands.lint_format.as_deref();
            match crate::linter::run_lint_scan(
                &self.config.path,
                lint_cmd,
                lint_format,
                self.config.linter_autofix,
                self.config.max_linter_findings,
                &self.config.sonar_project_id,
            ) {
                Ok(issues) => {
                    info!(
                        "Linter scan produced {} queueable finding(s)",
                        issues.len()
                    );
                    // If autofix made changes, commit them so the fix loop starts
                    // from a clean tree (WIP commits are absorbed into the branch).
                    if self.config.linter_autofix {
                        match git::has_changes(&self.config.path) {
                            Ok(true) => {
                                if let Err(e) = git::commit_all(
                                    &self.config.path,
                                    &format_commit_message(
                                        &self.config,
                                        "style",
                                        "linter",
                                        "apply linter autofix before sonar fixes",
                                        "",
                                        "",
                                        "",
                                    ),
                                ) {
                                    warn!("Failed to commit linter autofix changes: {}", e);
                                } else {
                                    info!("Linter autofix changes committed");
                                }
                            }
                            Ok(false) => {}
                            Err(e) => warn!("Could not check git status after autofix: {}", e),
                        }
                    }
                    let count = issues.len() as f64;
                    linter_issues = issues;
                    self.exec_phase_finish(pid, ItemStatus::Completed, Some(count), None);
                }
                Err(e) => {
                    warn!("Linter scan failed (non-critical): {}", e);
                    self.exec_phase_finish(
                        pid,
                        ItemStatus::Failed,
                        None,
                        Some(&format!("{}", e)),
                    );
                }
            }
        }

        // Step 4: Initial SonarQube scan
        // Run coverage command first so the scanner picks up fresh lcov data
        if let Some(ref cov_cmd) = self.config.coverage_command
            .clone()
            .or_else(|| self.config.commands.coverage.clone())
        {
            info!("Generating coverage report before initial scan...");
            match runner::run_shell_command(&self.config.path, &cov_cmd, "pre-scan coverage") {
                Ok((true, output)) => {
                    if output.contains("Skipping JaCoCo execution due to missing execution data") {
                        warn!(
                            "JaCoCo skipped report generation — no execution data (jacoco.exec) was produced. \
                             Check that jacoco-maven-plugin is configured in the POM with prepare-agent execution."
                        );
                    }
                    if runner::find_lcov_report_with_hint(&self.config.path, self.config.commands.coverage_report.as_deref()).is_some() {
                        info!("Coverage report generated");
                    } else {
                        warn!("Coverage command succeeded but no report file was produced");
                    }
                }
                Ok((false, output)) => warn!("Coverage command failed: {}", truncate(&output, 200)),
                Err(e) => warn!("Coverage command error: {}", e),
            }
        }

        if let Some(ref scanner) = self.config.scanner {
            info!("=== Step 4: Initial SonarQube scan ===");
            let ce_task_id = self.client.run_scanner(
                &self.config.path,
                scanner,
                &self.config.branch,
            )?;
            self.client
                .wait_for_analysis(ce_task_id.as_deref())
                .await?;
        } else {
            info!("=== Step 4: Skipping scanner (--skip-scan) ===");
        }

        // Fetch initial issues to get total count and dry-run info
        let mut sonar_issues = self.client.fetch_issues().await?;
        if self.config.reverse_severity {
            sonar_issues.reverse();
            info!("Reversed severity order: processing least severe issues first");
        }

        // Merge linter findings into the sonar queue. Linter issues go through
        // the same fix loop — they're ordered severity-interleaved so a
        // BLOCKER lint finding is processed before a MAJOR sonar issue.
        let linter_count = linter_issues.len();
        let sonar_count = sonar_issues.len();
        let mut initial_issues = helpers::merge_lint_and_sonar_issues(
            linter_issues,
            sonar_issues,
            self.config.reverse_severity,
        );
        // C2: drop overlapping issues of the same (file, rule) so we don't
        // pay an AI call per containment-nested finding. One fix usually
        // resolves the whole stack.
        let pre_dedup_len = initial_issues.len();
        initial_issues = grouping::dedup_overlapping(initial_issues);
        let dropped = pre_dedup_len.saturating_sub(initial_issues.len());
        if dropped > 0 {
            info!(
                "Pre-queue dedup: dropped {} overlapping issues ({} → {})",
                dropped, pre_dedup_len, initial_issues.len()
            );
        }

        // A3: bucket LINT findings by (file, rule) and collapse each bucket
        // into one synthetic issue whose message enumerates every occurrence.
        // One AI call then resolves N findings. We deliberately restrict to
        // `lint:*` rules because Sonar issues are re-fetched mid-loop for
        // freshness, and re-fetching would resurrect individual Sonar findings
        // outside their batch. Lint findings are synthetic and never come
        // back from the Sonar re-fetch, so grouping them is safe.
        if !self.config.skip_issue_grouping {
            let (lint_part, sonar_part): (Vec<_>, Vec<_>) = initial_issues
                .into_iter()
                .partition(|i| i.rule.starts_with("lint:"));
            let pre_group_len = lint_part.len();
            let groups = grouping::group_issues(lint_part, self.config.max_group_size);
            let batched: usize = groups.iter().filter(|g| g.is_batched()).count();
            let mut grouped: Vec<Issue> = groups.into_iter().map(|g| g.into_representative()).collect();
            if batched > 0 {
                info!(
                    "Lint grouping: collapsed {} lint findings into {} groups ({} batched)",
                    pre_group_len,
                    grouped.len(),
                    batched
                );
            }
            // Re-merge sonar + grouped-lint, preserving severity ordering.
            initial_issues = helpers::merge_lint_and_sonar_issues(
                grouped.split_off(0),
                sonar_part,
                self.config.reverse_severity,
            );
        }

        // === Sonar autofix fast-path (OpenRewrite) ===
        //
        // Before the AI fix loop, run one `mvn rewrite:run` that activates
        // every recipe in our static rule map for which the queue contains
        // a matching issue. Files OpenRewrite touches are accepted as
        // mechanically-fixed: the corresponding issues get FixStatus::Fixed
        // and are dropped from the queue. The remaining queue flows through
        // the AI path unchanged.
        //
        // This is Maven-specific today (OpenRewrite has Gradle support; can
        // be added later behind a scanner-kind branch).
        if !self.config.skip_autofix_sonar {
            let eligible: bool = initial_issues
                .iter()
                .any(|i| crate::autofix_sonar::RULE_TO_RECIPES.iter().any(|(k, _)| *k == i.rule));
            let pom_exists = self.config.path.join("pom.xml").exists();
            if !pom_exists {
                info!("autofix-sonar: skipping (no pom.xml — Maven-only for now)");
            } else if !eligible {
                info!("autofix-sonar: skipping (no queued rule matches the recipe map)");
            } else {
                info!("=== Step 4b: Sonar autofix fast-path (OpenRewrite) ===");
                match crate::autofix_sonar::run(&self.config.path, &initial_issues) {
                    Ok(outcome) if !outcome.resolved_keys.is_empty() => {
                        // Commit the autofix changes as a single, labeled commit.
                        let _ = git::add_all(&self.config.path);
                        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                            let recipe_summary = outcome
                                .activated_recipes
                                .iter()
                                .take(5)
                                .map(|s| s.rsplit('.').next().unwrap_or(s))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let msg = format_commit_message(
                                &self.config,
                                "refactor",
                                "sonar",
                                &format!(
                                    "autofix {} issue(s) via OpenRewrite ({}{})",
                                    outcome.resolved_keys.len(),
                                    recipe_summary,
                                    if outcome.activated_recipes.len() > 5 { ", …" } else { "" },
                                ),
                                "",
                                "",
                                "",
                            );
                            let _ = git::commit(&self.config.path, &msg);
                        }
                        // Materialize Fixed results for each resolved key and
                        // filter them out of the AI queue.
                        let resolved_set: std::collections::HashSet<String> =
                            outcome.resolved_keys.iter().cloned().collect();
                        let before_len = initial_issues.len();
                        let (resolved, remaining): (Vec<Issue>, Vec<Issue>) = initial_issues
                            .into_iter()
                            .partition(|i| resolved_set.contains(&i.key));
                        for issue in resolved {
                            self.results.push(IssueResult {
                                issue_key: issue.key.clone(),
                                rule: issue.rule.clone(),
                                severity: issue.severity.clone(),
                                issue_type: issue.issue_type.clone(),
                                message: issue.message.clone(),
                                file: sonar::component_to_path(&issue.component),
                                lines: format_lines(&issue.text_range),
                                status: FixStatus::Fixed,
                                change_description: "OpenRewrite autofix (no AI)".to_string(),
                                tests_added: Vec::new(),
                                pr_url: None,
                                diff_summary: None,
                            });
                        }
                        initial_issues = remaining;
                        info!(
                            "autofix-sonar: queue shrank from {} → {} (saved ~{} AI calls)",
                            before_len,
                            initial_issues.len(),
                            before_len.saturating_sub(initial_issues.len())
                        );
                    }
                    Ok(_) => {}
                    Err(e) => warn!("autofix-sonar: run failed (continuing with AI path): {}", e),
                }
            }
        }

        self.total_issues_found = initial_issues.len();
        if linter_count > 0 {
            info!(
                "Found {} issues ({} sonar + {} linter)",
                self.total_issues_found, sonar_count, linter_count
            );
        } else {
            info!("Found {} issues", self.total_issues_found);
        }
        // Keep the binding name `initial_issues` for the rest of the loop.
        let _ = &mut initial_issues;

        if initial_issues.is_empty() {
            info!("No issues to fix. Congratulations!");
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }

        self.print_issue_summary(&initial_issues);
        self.print_issue_listing(&initial_issues);

        if self.config.dry_run {
            info!("=== Dry run mode — no fixes will be applied ===");
            info!("{} issues would be processed", initial_issues.len());
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }

        // US-017: Filter out already-processed issues if resuming
        let already_processed = self
            .exec_state
            .as_ref()
            .map(|s| s.processed_keys())
            .unwrap_or_default();

        // US-017: Initialize state if not resuming
        if self.exec_state.is_none() {
            self.exec_state = Some(crate::state::ExecutionState::new(
                &self.config.sonar_project_id,
                &self.config.branch,
                self.config.batch_size,
                self.total_issues_found,
            ));
        }

        // Pre-fix coverage: if no coverage report exists on disk (e.g. --skip-coverage was
        // passed and the pre-scan run produced nothing, or the report was cleaned between
        // steps), run the coverage command once now so that check_coverage() has data
        // during the fix loop. Always use auto-detection as the final fallback so this
        // works even when the command is not explicitly configured.
        if !self.config.skip_fixes {
            let report_exists = runner::find_lcov_report_with_hint(
                &self.config.path,
                self.config.commands.coverage_report.as_deref(),
            ).is_some();

            if !report_exists {
                let cov_cmd = self.config.coverage_command.clone()
                    .or_else(|| self.config.commands.coverage.clone())
                    .or_else(|| runner::detect_coverage_command(&self.config.path));

                if let Some(ref cmd) = cov_cmd {
                    info!("=== Pre-fix coverage: no report on disk — generating coverage data ===");
                    match runner::run_shell_command(&self.config.path, cmd, "pre-fix coverage") {
                        Ok((true, output)) => {
                            if output.contains("Skipping JaCoCo execution due to missing execution data") {
                                warn!(
                                    "JaCoCo skipped report generation — no execution data (jacoco.exec) found. \
                                     Ensure jacoco-maven-plugin prepare-agent is configured in the POM."
                                );
                            }
                            if runner::find_lcov_report_with_hint(
                                &self.config.path,
                                self.config.commands.coverage_report.as_deref(),
                            ).is_some() {
                                info!("Pre-fix coverage report generated");
                            } else {
                                warn!("Pre-fix coverage command succeeded but no report file was produced");
                            }
                        }
                        Ok((false, output)) => {
                            warn!("Pre-fix coverage command failed: {}", truncate(&output, 300));
                        }
                        Err(e) => warn!("Pre-fix coverage command error: {}", e),
                    }
                } else {
                    warn!(
                        "Pre-fix coverage: no coverage report found and no coverage command available — \
                         check_coverage will run without per-file data"
                    );
                }
            }
        }

        // Step 5: Fix loop — only fix issues from the initial scan
        info!("=== Step 5: Fix loop ===");
        let fix_loop_phase = self.exec_phase_start(
            "fix_loop",
            Some(self.total_issues_found as f64),
            Some(" issues"),
        );
        if self.config.skip_fixes {
            info!("Skipping fix loop (--skip-fixes)");
            self.exec_phase_finish(
                fix_loop_phase,
                ItemStatus::Skipped,
                Some(self.total_issues_found as f64),
                Some("--skip-fixes"),
            );
            let _ = git::checkout(&self.config.path, &self.config.branch);
            return Ok(0);
        }
        let max_issues = if self.config.max_issues > 0 {
            self.config.max_issues
        } else {
            usize::MAX
        };

        // Track original issue keys — we only fix issues that existed before we started.
        // Issues introduced by our fixes are NOT our responsibility.
        let original_issue_keys: std::collections::HashSet<String> =
            initial_issues.iter().map(|i| i.key.clone()).collect();
        info!("Tracking {} original issues", original_issue_keys.len());

        // US-018: Parallel mode — dispatch to worktrees
        //
        // Two sub-modes:
        //   batch_size == 1  →  per-issue parallel: each issue gets its own branch
        //                       and PR; processed concurrently.
        //   batch_size != 1  →  wave-parallel: issues touching different files are
        //                       processed in parallel within the shared batch branch;
        //                       a single PR is created at the end (same as sequential).
        // Hint / auto-opt-in when running sequentially over many issues:
        // per-issue parallelism with worktrees can cut wall-clock 3-4×.
        //
        // - >10 issues : emit the hint (same as before).
        // - >20 issues AND batch_size==1 AND user did not pass --parallel
        //   explicitly: auto-bump to 2 workers. Two is conservative (minimal
        //   disk + memory pressure from extra worktrees) but still halves
        //   wall-clock for the common case. Users can always set
        //   --parallel 1 explicitly to opt out, or --parallel 4 for more.
        if self.config.parallel <= 1 && initial_issues.len() > 10 {
            info!(
                "Hint: {} issues queued. For faster runs, try `--parallel 4 --batch-size 1` (per-issue worktree parallelism).",
                initial_issues.len()
            );
        }
        if self.config.parallel <= 1
            && self.config.batch_size == 1
            && initial_issues.len() > 20
            && std::env::var("REPARO_PARALLEL").is_err()
        {
            info!(
                "Auto-enabling --parallel 2 for {} issues (set REPARO_PARALLEL=1 or pass --parallel 1 to opt out)",
                initial_issues.len()
            );
            self.config.parallel = 2;
        }

        if self.config.parallel > 1 && self.config.batch_size == 1 {
            info!("=== Parallel mode (per-issue): {} workers ===", self.config.parallel);
            self.run_parallel_fix_loop(
                &initial_issues,
                &original_issue_keys,
                &already_processed,
                max_issues,
                &test_command,
            )
            .await?;

            // Skip sequential loop counters — go straight to post-processing
            let total_fixed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
            let total_failed = self.results.iter().filter(|r| !matches!(r.status, FixStatus::Fixed | FixStatus::Skipped(_))).count();

            info!(
                "Processing complete: {} fixed, {} failed/review",
                total_fixed, total_failed
            );

            self.exec_phase_finish(
                fix_loop_phase,
                ItemStatus::Completed,
                Some(total_fixed as f64),
                Some(&format!("parallel: {} fixed, {} failed", total_fixed, total_failed)),
            );

            // In per-issue parallel mode each issue has its own PR.
            // Jump to report generation.
            let report_phase = self.exec_phase_start("report", None, None);
            info!("=== Step 6: Generating report ===");
            let elapsed = start.elapsed().as_secs();
            report::generate_report(
                &self.config.path,
                &self.results,
                self.total_issues_found,
                elapsed,
            );

            let exit_code = self.print_summary(elapsed);
            crate::state::remove_state(&self.config.path);
            self.exec_phase_finish(report_phase, ItemStatus::Completed, None, None);

            return Ok(exit_code);
        }

        let wave_parallel = self.config.parallel > 1;
        if wave_parallel {
            info!(
                "=== Parallel mode (wave-based): {} workers, batch-size={} ===",
                self.config.parallel, self.config.batch_size
            );
            self.run_wave_parallel_fixes(
                &initial_issues,
                &original_issue_keys,
                &already_processed,
                max_issues,
                &test_command,
                &self.config.branch.clone(),
            )
            .await?;

            let wp_fixed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
            let wp_failed = self.results.iter().filter(|r| !matches!(r.status, FixStatus::Fixed | FixStatus::Skipped(_) | FixStatus::RiskSkipped(_))).count();
            info!("Wave-parallel complete: {} fixed, {} failed/review", wp_fixed, wp_failed);
            self.exec_phase_finish(
                fix_loop_phase,
                ItemStatus::Completed,
                Some(wp_fixed as f64),
                Some(&format!("wave-parallel: {} fixed, {} failed", wp_fixed, wp_failed)),
            );
        }

        let mut total_fixed = 0usize;
        let mut total_failed = 0usize;

        if !wave_parallel {
        let mut issue_num = 0usize;
        let mut consecutive_build_failures = 0usize;
        const MAX_CONSECUTIVE_FAILURES: usize = 3;

        // Batch-commit state: accumulate (key, message) of WIP-committed fixes and
        // squash them once the batch size is reached (or at the end of the loop).
        // Active only when fix_commit_batch != 1.
        let fix_batch_size: usize = if self.config.fix_commit_batch == 0 {
            usize::MAX // 0 = one commit per branch → squash all at the very end
        } else {
            self.config.fix_commit_batch as usize
        };
        let use_fix_batching = self.config.fix_commit_batch != 1;
        let mut wip_fix_issues: Vec<(String, String)> = Vec::new();

        // Per-fix validation uses targeted tests only when batch_size != 1.
        // The full suite runs once every `batch_size` fixes, amortizing the
        // ~78s cost across the batch. batch_size = 1 preserves the old
        // per-fix full suite; batch_size = 0 defers to final_validation.
        let full_suite_batch: usize = self.config.batch_size.max(1);
        let mut fixes_since_full_suite: usize = 0;

        loop {
            if issue_num >= max_issues {
                info!("Reached --max-issues limit ({})", max_issues);
                break;
            }

            // Circuit breaker: stop if too many consecutive build failures
            if consecutive_build_failures >= MAX_CONSECUTIVE_FAILURES {
                warn!(
                    "Stopping: {} consecutive build failures — likely a systemic issue (e.g. Node.js version, broken dependency). Fix the build manually and re-run.",
                    consecutive_build_failures
                );
                break;
            }

            // Pre-flight: verify build still passes before attempting next fix
            if let Some(ref build_cmd) = self.config.commands.build {
                match runner::run_shell_command(&self.config.path, build_cmd, "pre-fix build check") {
                    Ok((true, _)) => {}
                    Ok((false, output)) => {
                        error!(
                            "Build is broken before attempting next fix — stopping. Output:\n{}",
                            truncate(&output, 300)
                        );
                        break;
                    }
                    Err(e) => {
                        error!("Build check error: {} — stopping", e);
                        break;
                    }
                }
            }

            // Fetch fresh issues from SonarQube
            let issues = match self.client.fetch_issues().await {
                Ok(mut issues) => {
                    if self.config.reverse_severity {
                        issues.reverse();
                    }
                    // Linter issues never appear in SonarQube. Re-inject any
                    // still-pending linter issues so the picker below can
                    // consider them alongside fresh sonar data.
                    let pending_linter: Vec<Issue> = initial_issues
                        .iter()
                        .filter(|i| i.rule.starts_with("lint:"))
                        .filter(|i| !already_processed.contains(&i.key))
                        .filter(|i| !self.results.iter().any(|r| r.issue_key == i.key))
                        .cloned()
                        .collect();
                    helpers::merge_lint_and_sonar_issues(
                        pending_linter,
                        issues,
                        self.config.reverse_severity,
                    )
                }
                Err(e) => {
                    error!("Failed to fetch issues: {}", e);
                    break;
                }
            };

            if issues.is_empty() {
                info!("No more issues to fix!");
                break;
            }

            // Pick the most critical issue that:
            // 1. Was in the original scan (not introduced by our fixes)
            // 2. Hasn't been processed yet
            let issue = match issues.into_iter().find(|i| {
                original_issue_keys.contains(&i.key)
                    && !already_processed.contains(&i.key)
                    && !self.results.iter().any(|r| r.issue_key == i.key)
            }) {
                Some(i) => i,
                None => {
                    info!("All remaining issues already processed");
                    break;
                }
            };

            issue_num += 1;
            info!(
                "--- [{}/{}] Processing: {} ({} {}) in {} ---",
                issue_num,
                max_issues.min(self.total_issues_found),
                issue.key,
                issue.severity,
                issue.issue_type,
                sonar::component_to_path(&issue.component)
            );

            // Pre-fetch rule description if not cached
            if !self.rule_cache.contains_key(&issue.rule) {
                if let Ok(desc) = self.client.get_rule_description(&issue.rule).await {
                    self.rule_cache.insert(issue.rule.clone(), desc);
                }
            }

            // exec log: open a per-issue step
            let current_phase = *self.current_phase_id.lock().unwrap();
            let step_target = format!("{} ({})", issue.key, sonar::component_to_path(&issue.component));
            let issue_step = self.exec_step_start(
                current_phase,
                "fix_issue",
                Some(&step_target),
                None,
            );

            self.fix_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = self.process_issue(&issue, &test_command).await;
            // Update needs_clean based on outcome: clean stays skipped after a
            // successful fix, but must run again after any kind of failure
            // (build/test/repair) since the tree may be dirty or in a weird
            // state that only a clean build can recover from.
            if !matches!(result.status, FixStatus::Fixed) {
                self.needs_clean
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let (step_status, step_details) = match &result.status {
                FixStatus::Fixed => {
                    total_fixed += 1;
                    consecutive_build_failures = 0; // Reset on success
                    info!("Issue {} fixed successfully ({} fixed so far)", issue.key, total_fixed);
                    if use_fix_batching {
                        wip_fix_issues.push((issue.key.clone(), issue.message.clone()));
                        if wip_fix_issues.len() >= fix_batch_size {
                            let _ = self.squash_fix_commits(wip_fix_issues.len(), &wip_fix_issues);
                            wip_fix_issues.clear();
                        }
                    }
                    // Batch-boundary full-suite validation: when batch_size > 1,
                    // the per-fix validation is targeted-only, so we amortize the
                    // full suite cost by running it once every `batch_size` fixes.
                    fixes_since_full_suite += 1;
                    if self.config.batch_size > 1 && fixes_since_full_suite >= full_suite_batch {
                        if !test_command.is_empty() {
                            info!(
                                "Running full test suite after batch of {} fixes (batch_size={})...",
                                fixes_since_full_suite, self.config.batch_size
                            );
                            match runner::run_tests(&self.config.path, &test_command, self.config.test_timeout) {
                                Ok((true, _)) => info!("Batch full-suite validation PASSED"),
                                Ok((false, output)) => warn!(
                                    "Batch full-suite validation FAILED after {} fixes — final_validation will retry at end. Output:\n{}",
                                    fixes_since_full_suite, truncate(&output, 500)
                                ),
                                Err(e) => warn!("Batch full-suite validation error (non-blocking): {}", e),
                            }
                        }
                        fixes_since_full_suite = 0;
                    }
                    (ItemStatus::Completed, "fixed".to_string())
                }
                FixStatus::NeedsReview(reason) => {
                    total_failed += 1;
                    consecutive_build_failures = 0; // Review != build failure
                    warn!("Issue {} needs manual review: {}", issue.key, reason);
                    (ItemStatus::Failed, format!("needs review: {}", reason))
                }
                FixStatus::Failed(err) => {
                    total_failed += 1;
                    if err.contains("Build fails") || err.contains("Build command error") {
                        consecutive_build_failures += 1;
                    } else {
                        consecutive_build_failures = 0;
                    }
                    error!("Issue {} failed: {}", issue.key, err);
                    (ItemStatus::Failed, format!("failed: {}", err))
                }
                FixStatus::Skipped(reason) => {
                    info!("Issue {} skipped: {}", issue.key, reason);
                    (ItemStatus::Skipped, format!("skipped: {}", reason))
                }
                FixStatus::RiskSkipped(reason) => {
                    info!("Issue {} risk-skipped (cross-cutting impact): {}", issue.key, reason);
                    (ItemStatus::Skipped, format!("risk-skipped: {}", reason))
                }
            };
            self.exec_step_finish(issue_step, step_status, None, Some(&step_details));

            // US-013: Document in changelog immediately
            report::append_changelog(&self.config.path, &result);

            // US-017: Save state after each issue
            if let Some(ref mut state) = self.exec_state {
                let status_str = match &result.status {
                    FixStatus::Fixed => "fixed",
                    FixStatus::NeedsReview(_) => "needs_review",
                    FixStatus::Failed(_) => "failed",
                    FixStatus::Skipped(_) => "skipped",
                    FixStatus::RiskSkipped(_) => "risk_skipped",
                };
                let reason = match &result.status {
                    FixStatus::Failed(r) | FixStatus::NeedsReview(r) | FixStatus::Skipped(r) | FixStatus::RiskSkipped(r) => Some(r.as_str()),
                    _ => None,
                };
                state.add_processed(&result.issue_key, status_str, result.pr_url.as_deref(), reason);
                let _ = crate::state::save_state(&self.config.path, state);
            }

            self.results.push(result);
        } // end sequential issue loop

        // Final squash: flush any remaining WIP fix commits that didn't fill a full batch.
        if use_fix_batching && !wip_fix_issues.is_empty() {
            let _ = self.squash_fix_commits(wip_fix_issues.len(), &wip_fix_issues);
            wip_fix_issues.clear();
        }

        info!(
            "Sequential processing complete: {} fixed, {} failed/review",
            total_fixed, total_failed
        );

        // Close the fix_loop phase with the final fix count as the "after" metric
        self.exec_phase_finish(
            fix_loop_phase,
            ItemStatus::Completed,
            Some(total_fixed as f64),
            Some(&format!("{} fixed, {} failed", total_fixed, total_failed)),
        );
        } // end if !wave_parallel

        // Compute final counts from self.results for the shared post-processing path
        let total_fixed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
        let _total_failed = self.results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_) | FixStatus::Failed(_))).count();

        // Step 5b: Deduplication — reduce duplicated code after fixes
        let dedup_phase = self.exec_phase_start("dedup", None, None);
        if self.config.skip_dedup {
            info!("=== Step 5b: Deduplication SKIPPED (--skip-dedup) ===");
            self.exec_phase_finish(dedup_phase, ItemStatus::Skipped, None, Some("--skip-dedup"));
        } else if let Some(ref scanner) = self.config.scanner {
            let dedup_result = self.reduce_duplications(&test_command, scanner).await;
            match &dedup_result {
                Ok(_) => self.exec_phase_finish(dedup_phase, ItemStatus::Completed, None, None),
                Err(e) => self.exec_phase_finish(
                    dedup_phase,
                    ItemStatus::Failed,
                    None,
                    Some(&format!("{}", e)),
                ),
            }
            dedup_result?;
        } else {
            info!("=== Step 5b: Deduplication SKIPPED (no scanner) ===");
            self.exec_phase_finish(dedup_phase, ItemStatus::Skipped, None, Some("no scanner"));
        }

        // Step 5c: Final validation — run the FULL test suite; iterate with Claude until ALL tests pass
        let final_phase = self.exec_phase_start("final_validation", None, None);
        if self.config.skip_final_validation {
            info!("=== Step 5c: Final validation SKIPPED (disabled) ===");
            self.exec_phase_finish(final_phase, ItemStatus::Skipped, None, Some("disabled"));
        } else {
        info!("=== Step 5c: Final validation (all tests must pass) ===");
        }
        if !self.config.skip_final_validation && !test_command.is_empty() {
            let max_final_attempts = self.config.final_validation_attempts;
            let mut final_ok = false;

            for attempt in 1..=max_final_attempts {
                // Build check
                if let Some(ref build_cmd) = self.config.commands.build {
                    match runner::run_shell_command(&self.config.path, build_cmd, "final build check") {
                        Ok((true, _)) => info!("Final build check passed"),
                        Ok((false, output)) => {
                            warn!("Final build check FAILED (attempt {}/{})", attempt, max_final_attempts);
                            if attempt < max_final_attempts {
                                info!("Asking Claude to fix the build error...");
                                let repair_prompt = format!(
                                    r#"The project build is failing. Fix the build error WITHOUT modifying any test files.

## Build output:
```
{}
```

## Instructions:
1. Fix the build error
2. Do NOT modify any test files (*.spec.ts, *.test.ts, etc.)
3. Do NOT change test logic or assertions
4. Ensure the project compiles successfully

Apply the fix now."#,
                                    truncate(&output, 3000)
                                );
                                let repair_tier = claude::classify_repair_tier();
                                let _ = self.run_ai("final_validation_build_repair", &repair_prompt, &repair_tier);
                                if let Some(ref fmt_cmd) = self.config.commands.format {
                                    let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                                }
                                continue;
                            } else {
                                error!("Build still failing after {} repair attempts", max_final_attempts);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Build command error: {}", e);
                            break;
                        }
                    }
                }

                // Full test suite check — ALL tests must pass, not just per-issue tests
                info!("Running full test suite (attempt {}/{})...", attempt, max_final_attempts);
                match runner::run_tests(&self.config.path, &test_command, self.config.test_timeout) {
                    Ok((true, _)) => {
                        info!("Final validation PASSED — all tests green after {} attempt(s)", attempt);
                        final_ok = true;
                        break;
                    }
                    Ok((false, output)) => {
                        warn!("Full test suite FAILED (attempt {}/{})", attempt, max_final_attempts);
                        if attempt < max_final_attempts {
                            info!("Iterating: asking Claude to fix failures without modifying test files...");
                            let repair_prompt = format!(
                                r#"The full test suite is failing after applying SonarQube fixes. ALL tests must pass before we can accept the changes. Fix the SOURCE CODE to make every test pass. Do NOT modify any test files.

## Test output:
```
{}
```

## Instructions:
1. Analyze the failing tests and identify which source code changes broke them
2. Fix the source code to make ALL tests pass — not just the ones related to the current fix
3. Do NOT modify any test files (*.spec.ts, *.test.ts, *_test.go, test_*.py, etc.)
4. Do NOT change test logic or assertions — the tests define the expected behavior
5. Ensure the project compiles and the entire test suite passes

Apply the fix now."#,
                                truncate(&output, 3000)
                            );
                            let repair_tier = claude::classify_repair_tier();
                            let _ = self.run_ai("final_validation_test_repair", &repair_prompt, &repair_tier);
                            if let Some(ref fmt_cmd) = self.config.commands.format {
                                let _ = runner::run_shell_command(&self.config.path, fmt_cmd, "format");
                            }
                        } else {
                            error!("Full test suite still failing after {} repair attempts — manual intervention needed", max_final_attempts);
                        }
                    }
                    Err(e) => {
                        error!("Test command error during final validation: {}", e);
                        break;
                    }
                }
            }

            // Commit any final fixes
            if final_ok {
                let _ = git::add_all(&self.config.path);
                if git::has_staged_changes(&self.config.path).unwrap_or(false) {
                    let msg = format_commit_message(&self.config, "fix", "sonar", "repair build/test issues from accumulated changes", "", "", "");
                    let _ = git::commit(&self.config.path, &msg);
                    info!("Committed final validation fixes");
                }
            }
            self.exec_phase_finish(
                final_phase,
                if final_ok { ItemStatus::Completed } else { ItemStatus::Failed },
                None,
                None,
            );
        }

        // Step 5d: Traceability matrix (US-068) — runs after final_validation, only when --compliance
        if self.config.compliance_enabled {
            let trace_phase = self.exec_phase_start("traceability_report", None, None);
            info!("=== Step 5d: Generating traceability matrix ===");
            let report_dir = if self.config.execution_log_report_dir == ".reparo" {
                self.config.path.join(".reparo")
            } else {
                let p = std::path::PathBuf::from(&self.config.execution_log_report_dir);
                if p.is_absolute() { p } else { self.config.path.join(&self.config.execution_log_report_dir) }
            };
            let trace_dir = self.config.compliance.traceability_dir.as_ref()
                .map(|d| self.config.path.join(d))
                .unwrap_or_else(|| report_dir.clone());
            match self.exec_log.generate_traceability_matrix(None, &trace_dir, self.config.health_mode) {
                Ok(path) => {
                    info!("Traceability matrix written to {}", path.display());
                    self.exec_phase_finish(trace_phase, ItemStatus::Completed, None, None);
                }
                Err(e) => {
                    warn!("Failed to generate traceability matrix: {} — continuing", e);
                    self.exec_phase_finish(trace_phase, ItemStatus::Failed, None, Some(&format!("{}", e)));
                }
            }
        }

        // Step 5e: Compliance report (US-071) — runs after traceability_report, only when --compliance
        if self.config.compliance_enabled {
            let comp_phase = self.exec_phase_start("compliance_report", None, None);
            info!("=== Step 5e: Generating compliance report ===");
            let report_dir = if self.config.execution_log_report_dir == ".reparo" {
                self.config.path.join(".reparo")
            } else {
                let p = std::path::PathBuf::from(&self.config.execution_log_report_dir);
                if p.is_absolute() { p } else { self.config.path.join(&self.config.execution_log_report_dir) }
            };
            match crate::compliance::report::build_report(
                &self.exec_log,
                self.exec_log.run_id(),
                &self.config.compliance,
                self.config.health_mode,
            ) {
                Ok(report) => {
                    match crate::compliance::report::write_compliance_file(&report, &report_dir) {
                        Ok(path) => {
                            info!("Compliance report written to {}", path.display());
                            self.exec_phase_finish(comp_phase, ItemStatus::Completed, None, Some(path.display().to_string().as_str()));
                        }
                        Err(e) => {
                            warn!("Failed to write compliance report: {} — continuing", e);
                            self.exec_phase_finish(comp_phase, ItemStatus::Failed, None, Some(&format!("{}", e)));
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to build compliance report: {} — continuing", e);
                    self.exec_phase_finish(comp_phase, ItemStatus::Failed, None, Some(&format!("{}", e)));
                }
            }
        }

        // Step 5c: Documentation quality — ensure code documentation meets standards
        let docs_phase = self.exec_phase_start("documentation", None, None);
        if self.config.skip_docs {
            info!("=== Step 5c: Documentation SKIPPED (--skip-docs) ===");
            self.exec_phase_finish(docs_phase, ItemStatus::Skipped, None, Some("--skip-docs"));
        } else if self.config.documentation.enabled {
            let docs_result = self.improve_documentation(&test_command).await;
            match &docs_result {
                Ok(_) => self.exec_phase_finish(docs_phase, ItemStatus::Completed, None, None),
                Err(e) => self.exec_phase_finish(
                    docs_phase,
                    ItemStatus::Failed,
                    None,
                    Some(&format!("{}", e)),
                ),
            }
            docs_result?;
        } else {
            info!("=== Step 5c: Documentation SKIPPED (not enabled in YAML) ===");
            self.exec_phase_finish(docs_phase, ItemStatus::Skipped, None, Some("disabled in YAML"));
        }

        // Step 5f: End-of-run SonarQube verification (B4).
        // When `--rescan-batch-size 0` is set we skipped all per-issue rescans
        // during the fix loop. Run a single scan now, diff against the
        // originally queued issue keys, and retroactively flag any fixes whose
        // issue Sonar still reports as OPEN — those go to manual review.
        if self.config.rescan_batch_size == 0 {
            if let Some(ref scanner) = self.config.scanner {
                info!("=== Step 5f: End-of-run SonarQube verification ===");
                match self.client.run_scanner(&self.config.path, scanner, &self.config.branch) {
                    Ok(ce_task_id) => {
                        if let Err(e) = self.client.wait_for_analysis(ce_task_id.as_deref()).await {
                            warn!("End-of-run Sonar analysis wait failed: {} — skipping verification", e);
                        } else {
                            match self.client.fetch_issues().await {
                                Ok(remaining) => {
                                    let remaining_keys: std::collections::HashSet<String> =
                                        remaining.iter().map(|i| i.key.clone()).collect();
                                    let mut still_open = 0usize;
                                    for r in self.results.iter_mut() {
                                        if matches!(r.status, FixStatus::Fixed)
                                            && remaining_keys.contains(&r.issue_key)
                                        {
                                            r.status = FixStatus::NeedsReview(
                                                "End-of-run SonarQube scan still reports this issue — manual review required".to_string()
                                            );
                                            still_open += 1;
                                        }
                                    }
                                    info!(
                                        "End-of-run verification: {} fixes downgraded to NeedsReview ({} still-open issues)",
                                        still_open,
                                        remaining_keys.len()
                                    );
                                }
                                Err(e) => warn!("End-of-run fetch_issues failed: {}", e),
                            }
                        }
                    }
                    Err(e) => warn!("End-of-run run_scanner failed: {}", e),
                }
            } else {
                info!("=== Step 5f: End-of-run verification SKIPPED (no scanner configured) ===");
            }
        }

        // Step 6: Generate report (on the fix branch)
        let report_phase = self.exec_phase_start("report", None, None);
        info!("=== Step 6: Generating report ===");
        let elapsed = start.elapsed().as_secs();
        report::generate_report(
            &self.config.path,
            &self.results,
            self.total_issues_found,
            elapsed,
        );

        // Commit report files to the fix branch
        let _ = git::add_all(&self.config.path);
        if git::has_staged_changes(&self.config.path).unwrap_or(false) {
            let msg = format_commit_message(&self.config, "docs", "sonar", "add REPORT.md and TECHDEBT_CHANGELOG.md", "", "", "");
            let _ = git::commit(&self.config.path, &msg);
        }
        self.exec_phase_finish(report_phase, ItemStatus::Completed, None, None);

        // Step 7: Create PR if enabled and there are fixes
        if self.config.pr && total_fixed > 0 {
            info!("=== Step 7: Creating PR ===");
            match self.create_pr(&branch_name) {
                Ok(pr_url) => {
                    info!("PR created: {}", pr_url);
                    for r in self.results.iter_mut() {
                        if matches!(r.status, FixStatus::Fixed) {
                            r.pr_url = Some(pr_url.clone());
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create PR: {}", e);
                }
            }
        } else if !self.config.pr {
            info!("PR creation disabled (--no-pr)");
        } else if total_fixed == 0 {
            info!("No fixes — skipping PR creation");
        }

        let exit_code = self.print_summary(elapsed);

        // Stay on the fix branch so the user can review changes
        info!("Staying on branch '{}' for review", branch_name);

        // US-017: Clean up state file on successful completion
        crate::state::remove_state(&self.config.path);

        Ok(exit_code)
    }

    /// Print a structured summary of issues by severity and type (US-003).
    fn print_issue_summary(&self, issues: &[Issue]) {
        // Counts by severity (in priority order)
        let severity_order = ["BLOCKER", "CRITICAL", "MAJOR", "MINOR", "INFO"];
        let type_order = ["BUG", "VULNERABILITY", "SECURITY_HOTSPOT", "CODE_SMELL"];

        let mut by_severity: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut by_type: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();

        for issue in issues {
            *by_severity.entry(&issue.severity).or_default() += 1;
            *by_type.entry(&issue.issue_type).or_default() += 1;
        }

        info!("┌─────────────────────────────────────────────────┐");
        info!("│              Issue Summary ({:>5} total)          │", issues.len());
        info!("├─────────────────────────────────────────────────┤");
        info!("│ By severity:                                    │");
        for sev in &severity_order {
            if let Some(&count) = by_severity.get(sev) {
                let bar = "█".repeat(count.min(30));
                info!("│   {:>8}: {:>4}  {:<30}│", sev, count, bar);
            }
        }
        info!("│ By type:                                        │");
        for typ in &type_order {
            if let Some(&count) = by_type.get(typ) {
                info!("│   {:>16}: {:>4}                            │", typ, count);
            }
        }
        info!("└─────────────────────────────────────────────────┘");
    }

    /// Print per-issue listing: severity, type, file, line, rule, message (US-003).
    fn print_issue_listing(&self, issues: &[Issue]) {
        info!("Issue listing (sorted by priority):");
        info!(
            "  {:<10} {:<18} {:<40} {:<8} {:<25} {}",
            "SEVERITY", "TYPE", "FILE", "LINE", "RULE", "MESSAGE"
        );
        info!("  {}", "-".repeat(120));
        for issue in issues {
            let file = sonar::component_to_path(&issue.component);
            let line = match &issue.text_range {
                Some(tr) => {
                    if tr.start_line == tr.end_line {
                        format!("{}", tr.start_line)
                    } else {
                        format!("{}-{}", tr.start_line, tr.end_line)
                    }
                }
                None => "?".to_string(),
            };
            // Truncate long fields for readable console output
            let file_char_count = file.chars().count();
            let file_display = if file_char_count > 38 {
                let suffix: String = file.chars().skip(file_char_count - 35).collect();
                format!("...{}", suffix)
            } else {
                file
            };
            let msg_char_count = issue.message.chars().count();
            let msg_display = if msg_char_count > 60 {
                let prefix: String = issue.message.chars().take(57).collect();
                format!("{}...", prefix)
            } else {
                issue.message.clone()
            };
            info!(
                "  {:<10} {:<18} {:<40} {:<8} {:<25} {}",
                issue.severity,
                issue.issue_type,
                file_display,
                line,
                issue.rule,
                msg_display,
            );
        }
    }

    /// Print the final summary and return the appropriate exit code (US-010).
    ///
    /// Exit codes:
    /// - 0: all issues fixed successfully (or no issues found)
    /// - 2: partial success (some fixes, some failures)
    fn print_summary(&self, elapsed: u64) -> i32 {
        let total = self.results.len();
        let fixed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Fixed)).count();
        let review = self.results.iter().filter(|r| matches!(r.status, FixStatus::NeedsReview(_))).count();
        let failed = self.results.iter().filter(|r| matches!(r.status, FixStatus::Failed(_))).count();
        let skipped = self.results.iter().filter(|r| matches!(r.status, FixStatus::Skipped(_))).count();
        let prs_created: usize = self
            .results
            .iter()
            .filter_map(|r| r.pr_url.as_ref())
            .collect::<std::collections::HashSet<_>>()
            .len();

        info!("╔══════════════════════════════════════════════╗");
        info!("║           Reparo — Final Summary          ║");
        info!("╠══════════════════════════════════════════════╣");
        info!("║  Total issues processed:  {:>5}              ║", total);
        info!("║  Fixed:                   {:>5}              ║", fixed);
        info!("║  Needs manual review:     {:>5}              ║", review);
        info!("║  Failed:                  {:>5}              ║", failed);
        info!("║  Skipped (idempotent):    {:>5}              ║", skipped);
        info!("║  PRs created:            {:>5}              ║", prs_created);
        info!("║  Time elapsed:         {:>3}m {:>2}s              ║", elapsed / 60, elapsed % 60);
        info!("╚══════════════════════════════════════════════╝");

        if prs_created > 0 {
            info!("PRs:");
            for url in self.results.iter().filter_map(|r| r.pr_url.as_ref()).collect::<std::collections::HashSet<_>>() {
                info!("  {}", url);
            }
        }

        // Determine exit code
        if total == 0 || fixed == total {
            0 // all good
        } else if fixed > 0 {
            2 // partial success
        } else {
            2 // all failed but not a config error
        }
    }
}
