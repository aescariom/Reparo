//! Real-time execution logging to a SQLite database.
//!
//! Tracks each Reparo run, its phases, steps, and AI calls, so the user can
//! inspect historical performance, token usage, and improvement per phase.
//!
//! The database is opened at `~/.reparo/executions.db` by default (configurable
//! via `--execution-db`). Writes use WAL mode and are committed immediately so
//! that even aborted runs (Ctrl+C) leave partial data.
//!
//! # Data model
//!
//! - `runs`     — one row per Reparo invocation
//! - `phases`   — top-level workflow phases (coverage_boost, fix_loop, dedup, ...)
//! - `steps`    — fine-grained steps inside a phase (one file, one issue, ...)
//! - `ai_calls` — every AI invocation with tokens, model, effort, duration
//!
//! # Lifecycle
//!
//! 1. `ExecutionLog::init()` creates the row in `runs` with `status='running'`
//! 2. Phases open/close via `start_phase`/`finish_phase`
//! 3. Steps open/close via `start_step`/`finish_step`
//! 4. Every AI call is logged via `log_ai_call`
//! 5. `finish_run` marks the run as `completed`/`failed`/`aborted` and stores a markdown summary
//!
//! If the process is killed abruptly (SIGKILL), the run row stays in `running`
//! state and is reconcilable later via `reconcile_stale_runs` (future work).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;
use tracing::warn;

use crate::usage::UsageEntry;

/// US-072: Audit information captured at run start for ENS Alto op.exp.10.
#[derive(Debug, Clone, Default)]
pub struct AuditInfo {
    pub operator_name: Option<String>,
    pub operator_email: Option<String>,
    pub hostname: Option<String>,
    pub os_info: String,
    pub working_directory: Option<String>,
    pub git_commit_before: Option<String>,
}

impl AuditInfo {
    /// Capture audit fields from the current environment.
    ///
    /// Best-effort: missing git config or hostname result in `None` rather than
    /// aborting — Reparo must still run in containers or environments without git.
    pub fn capture(project_path: &Path) -> Self {
        let (operator_name, operator_email) = capture_git_identity(project_path);
        let hostname = capture_hostname();
        let os_info = format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH);
        let working_directory = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        let git_commit_before = capture_git_head(project_path);

        Self {
            operator_name,
            operator_email,
            hostname,
            os_info,
            working_directory,
            git_commit_before,
        }
    }
}

fn capture_git_identity(project_path: &Path) -> (Option<String>, Option<String>) {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(project_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let email = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(project_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (name, email)
}

fn capture_git_head(project_path: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn capture_hostname() -> Option<String> {
    // Try $HOSTNAME first (POSIX), then the `hostname` binary, then uname -n
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() { return Some(h); }
    }
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        if !h.is_empty() { return Some(h); }
    }
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// US-072: Compute a SHA-256 hash chain over the audit-relevant fields of a run.
///
/// The hash covers: run_id, timestamps, status, operator, hostname, git commits,
/// and the previous run's hash. A break in the chain signals tampering.
fn compute_run_hash(
    run_id: &str,
    started_at: i64,
    ended_at: i64,
    status: &str,
    exit_code: Option<i32>,
    operator_email: Option<&str>,
    hostname: Option<&str>,
    git_before: Option<&str>,
    git_after: Option<&str>,
    prev_hash: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    let canonical = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        run_id,
        started_at,
        ended_at,
        status,
        exit_code.map(|c| c.to_string()).unwrap_or_default(),
        operator_email.unwrap_or(""),
        hostname.unwrap_or(""),
        git_before.unwrap_or(""),
        git_after.unwrap_or(""),
        prev_hash.unwrap_or(""),
    );
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Running` is the initial state (set via INSERT); others are set explicitly
pub enum RunStatus {
    Running,
    Completed,
    Aborted, // Ctrl+C or external signal
    Failed,  // runtime error
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Aborted => "aborted",
            RunStatus::Failed => "failed",
        }
    }
}

/// Status of a phase or step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Running`/`Aborted` set via SQL in finish_run; not constructed in Rust
pub enum ItemStatus {
    Running,
    Completed,
    Failed,
    Skipped,
    Aborted,
}

impl ItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemStatus::Running => "running",
            ItemStatus::Completed => "completed",
            ItemStatus::Failed => "failed",
            ItemStatus::Skipped => "skipped",
            ItemStatus::Aborted => "aborted",
        }
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id                TEXT PRIMARY KEY,
    project           TEXT NOT NULL,
    project_path      TEXT NOT NULL,
    sonar_project_id  TEXT,
    branch            TEXT,
    started_at        INTEGER NOT NULL,
    ended_at          INTEGER,
    status            TEXT NOT NULL,
    exit_code         INTEGER,
    reparo_version    TEXT,
    config_json       TEXT,
    summary_md        TEXT,
    -- US-072: audit fields for ENS Alto op.exp.10 + op.exp.11
    operator_name     TEXT,
    operator_email    TEXT,
    hostname          TEXT,
    os_info           TEXT,
    working_directory TEXT,
    git_commit_before TEXT,
    git_commit_after  TEXT,
    run_hash          TEXT,
    prev_run_hash     TEXT
);

CREATE TABLE IF NOT EXISTS phases (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id        TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    started_at    INTEGER NOT NULL,
    ended_at      INTEGER,
    status        TEXT NOT NULL,
    metric_before REAL,
    metric_after  REAL,
    metric_unit   TEXT,
    details       TEXT
);

CREATE TABLE IF NOT EXISTS steps (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id        TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    phase_id      INTEGER NOT NULL REFERENCES phases(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    target        TEXT,
    started_at    INTEGER NOT NULL,
    ended_at      INTEGER,
    status        TEXT NOT NULL,
    metric_before REAL,
    metric_after  REAL,
    details       TEXT
);

CREATE TABLE IF NOT EXISTS ai_calls (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id                TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    phase_id              INTEGER REFERENCES phases(id) ON DELETE SET NULL,
    step_id               INTEGER REFERENCES steps(id) ON DELETE SET NULL,
    ts                    INTEGER NOT NULL,
    step_name             TEXT NOT NULL,
    engine                TEXT NOT NULL,
    model                 TEXT,
    effort                TEXT,
    input_tokens          INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    duration_ms           INTEGER,
    unknown_usage         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_phases_run    ON phases(run_id);
CREATE INDEX IF NOT EXISTS idx_steps_run     ON steps(run_id);
CREATE INDEX IF NOT EXISTS idx_steps_phase   ON steps(phase_id);
CREATE INDEX IF NOT EXISTS idx_ai_calls_run  ON ai_calls(run_id);
CREATE INDEX IF NOT EXISTS idx_ai_calls_phase ON ai_calls(phase_id);
CREATE INDEX IF NOT EXISTS idx_ai_calls_step  ON ai_calls(step_id);

-- US-068: Final coverage snapshot per file (populated at end of coverage boost).
CREATE TABLE IF NOT EXISTS final_coverage (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id              TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    file                TEXT NOT NULL,
    line_coverage_pct   REAL NOT NULL DEFAULT 0.0,
    branch_coverage_pct REAL NOT NULL DEFAULT 0.0,
    recorded_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_final_coverage_run ON final_coverage(run_id);
CREATE INDEX IF NOT EXISTS idx_final_coverage_file ON final_coverage(run_id, file);

-- US-068/US-066: Test artifacts — one row per traced test (populated when US-066 is active).
-- Empty by default (US-066 not yet implemented). Matrix generation is graceful with 0 rows.
CREATE TABLE IF NOT EXISTS test_artifacts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    requirement  TEXT NOT NULL,
    test_file    TEXT NOT NULL,
    test_name    TEXT NOT NULL,
    test_type    TEXT NOT NULL,
    risk_class   TEXT,
    source_file  TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_test_artifacts_run ON test_artifacts(run_id);
CREATE INDEX IF NOT EXISTS idx_test_artifacts_req ON test_artifacts(requirement);

-- US-070: MC/DC gap tracking for Class C files.
CREATE TABLE IF NOT EXISTS mcdc_gaps (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id           TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    file             TEXT NOT NULL,
    line             INTEGER NOT NULL,
    condition_count  INTEGER NOT NULL,
    tests_required   INTEGER NOT NULL,
    tests_observed   INTEGER NOT NULL,
    status           TEXT NOT NULL, -- 'gap' | 'covered'
    detected_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcdc_gaps_run  ON mcdc_gaps(run_id);
CREATE INDEX IF NOT EXISTS idx_mcdc_gaps_file ON mcdc_gaps(run_id, file);
"#;

/// Shared real-time execution log. All methods are thread-safe.
pub struct ExecutionLog {
    conn: Mutex<Connection>,
    run_id: String,
    db_path: PathBuf,
    start: Instant,
    /// Incrementing sequence for phases within this run.
    phase_seq: Mutex<i64>,
    /// US-072: kept so `finish_run` can capture git HEAD after the run completes.
    project_path: PathBuf,
}

/// US-072: DB migration for existing installations — add audit columns if missing.
/// Each ALTER is wrapped in error handling so running on a fresh DB is a no-op.
fn migrate_add_audit_columns(conn: &Connection) {
    let columns = [
        "operator_name TEXT",
        "operator_email TEXT",
        "hostname TEXT",
        "os_info TEXT",
        "working_directory TEXT",
        "git_commit_before TEXT",
        "git_commit_after TEXT",
        "run_hash TEXT",
        "prev_run_hash TEXT",
    ];
    for col_def in columns {
        let col_name = col_def.split_whitespace().next().unwrap_or_default();
        // Probe with a tiny SELECT; if it fails we assume the column is missing.
        let probe = format!("SELECT {} FROM runs LIMIT 0", col_name);
        if conn.query_row(&probe, [], |_| Ok(())).is_err() {
            let sql = format!("ALTER TABLE runs ADD COLUMN {}", col_def);
            if let Err(e) = conn.execute(&sql, []) {
                // The column may already exist (race/other process) — ignore
                tracing::debug!("audit column migration ({}): {}", col_name, e);
            }
        }
    }
}

impl ExecutionLog {
    /// Default database path: `~/.reparo/executions.db`.
    pub fn default_db_path() -> PathBuf {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".reparo");
        let _ = std::fs::create_dir_all(&base);
        base.join("executions.db")
    }

    /// Generate a run ID in the format `YYYYMMDD_HHMMSS_{project}`.
    pub fn generate_run_id(project: &str) -> String {
        let now: DateTime<Utc> = Utc::now();
        let safe_project = sanitize_project(project);
        format!("{}_{}", now.format("%Y%m%d_%H%M%S"), safe_project)
    }

    /// Initialize a new execution log entry.
    ///
    /// Creates the DB (and schema) if it doesn't exist, inserts a new row in
    /// `runs` with status='running', and returns an instance ready to log
    /// phases, steps, and AI calls.
    pub fn init(
        db_path: &Path,
        project: &str,
        project_path: &Path,
        sonar_project_id: Option<&str>,
        branch: Option<&str>,
        config_json: Option<&str>,
    ) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open SQLite DB at {}", db_path.display()))?;

        // WAL mode allows readers while we write, and commits are durable.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("Failed to set journal_mode=WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("Failed to set synchronous=NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("Failed to enable foreign_keys")?;

        conn.execute_batch(SCHEMA_SQL)
            .context("Failed to apply execution log schema")?;
        // US-072: retro-compat migration for DBs created before audit columns existed.
        migrate_add_audit_columns(&conn);

        let run_id = Self::generate_run_id(project);
        let now = Utc::now().timestamp();

        // US-072: capture audit info for ENS Alto op.exp.10 compliance.
        let audit = AuditInfo::capture(project_path);

        // US-072: look up the previous run's hash for the same project to build
        // a hash chain — tampering between runs breaks the chain.
        let prev_run_hash: Option<String> = if let Some(spid) = sonar_project_id {
            conn.query_row(
                "SELECT run_hash FROM runs
                 WHERE sonar_project_id = ?1 AND run_hash IS NOT NULL
                 ORDER BY started_at DESC LIMIT 1",
                params![spid],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        } else {
            None
        };

        conn.execute(
            "INSERT INTO runs (
                id, project, project_path, sonar_project_id, branch,
                started_at, status, reparo_version, config_json,
                operator_name, operator_email, hostname, os_info,
                working_directory, git_commit_before, prev_run_hash
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                &run_id,
                project,
                project_path.display().to_string(),
                sonar_project_id,
                branch,
                now,
                env!("CARGO_PKG_VERSION"),
                config_json,
                audit.operator_name,
                audit.operator_email,
                audit.hostname,
                audit.os_info,
                audit.working_directory,
                audit.git_commit_before,
                prev_run_hash,
            ],
        )
        .context("Failed to insert run row")?;

        Ok(Self {
            conn: Mutex::new(conn),
            run_id,
            db_path: db_path.to_path_buf(),
            start: Instant::now(),
            phase_seq: Mutex::new(0),
            project_path: project_path.to_path_buf(),
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Start a new phase. Returns the phase_id for later calls to `finish_phase`
    /// and `start_step`.
    pub fn start_phase(
        &self,
        name: &str,
        metric_before: Option<f64>,
        metric_unit: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let seq = {
            let mut s = self.phase_seq.lock().unwrap();
            *s += 1;
            *s
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO phases (run_id, name, seq, started_at, status, metric_before, metric_unit)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6)",
            params![&self.run_id, name, seq, now, metric_before, metric_unit],
        )
        .context("Failed to insert phase row")?;
        Ok(conn.last_insert_rowid())
    }

    /// Close an open phase with a status and optional final metric.
    pub fn finish_phase(
        &self,
        phase_id: i64,
        status: ItemStatus,
        metric_after: Option<f64>,
        details: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE phases SET ended_at = ?1, status = ?2, metric_after = ?3, details = ?4
             WHERE id = ?5",
            params![now, status.as_str(), metric_after, details, phase_id],
        )
        .context("Failed to update phase row")?;
        Ok(())
    }

    /// Start a step within a phase (e.g. one file, one issue).
    pub fn start_step(
        &self,
        phase_id: i64,
        name: &str,
        target: Option<&str>,
        metric_before: Option<f64>,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO steps (run_id, phase_id, name, target, started_at, status, metric_before)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
            params![&self.run_id, phase_id, name, target, now, metric_before],
        )
        .context("Failed to insert step row")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_step(
        &self,
        step_id: i64,
        status: ItemStatus,
        metric_after: Option<f64>,
        details: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE steps SET ended_at = ?1, status = ?2, metric_after = ?3, details = ?4
             WHERE id = ?5",
            params![now, status.as_str(), metric_after, details, step_id],
        )
        .context("Failed to update step row")?;
        Ok(())
    }

    /// Record a single AI invocation. Safe to call concurrently from multiple
    /// workers — SQLite in WAL mode serializes writes internally.
    pub fn log_ai_call(
        &self,
        phase_id: Option<i64>,
        step_id: Option<i64>,
        entry: &UsageEntry,
        effort: Option<&str>,
        duration_ms: Option<u64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let engine = entry.engine.to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ai_calls (
                run_id, phase_id, step_id, ts, step_name, engine, model, effort,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                duration_ms, unknown_usage
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                &self.run_id,
                phase_id,
                step_id,
                now,
                &entry.step,
                &engine,
                &entry.model,
                effort,
                entry.usage.input as i64,
                entry.usage.output as i64,
                entry.usage.cache_read as i64,
                entry.usage.cache_creation as i64,
                duration_ms.map(|d| d as i64),
                if entry.unknown { 1 } else { 0 },
            ],
        )
        .context("Failed to insert ai_call row")?;
        Ok(())
    }

    /// Write the summary markdown to a file under `report_dir/execution_<run_id>.md`
    /// and additionally to `report_dir/latest.md` for quick access.
    ///
    /// `project_path` is the project's root; `report_dir` is resolved relative
    /// to it if it isn't already absolute.
    pub fn write_summary_file(&self, project_path: &Path, report_dir: &str, summary_md: &str) -> Result<PathBuf> {
        let dir = if Path::new(report_dir).is_absolute() {
            PathBuf::from(report_dir)
        } else {
            project_path.join(report_dir)
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create execution log report dir {}", dir.display()))?;
        let file = dir.join(format!("execution_{}.md", self.run_id));
        std::fs::write(&file, summary_md)
            .with_context(|| format!("Failed to write execution summary to {}", file.display()))?;
        // Maintain a convenience "latest.md" pointer
        let latest = dir.join("latest.md");
        let _ = std::fs::write(&latest, summary_md);
        Ok(file)
    }

    /// Finalize the run: mark any still-running phases/steps as aborted,
    /// commit the final status to the DB, generate the summary markdown
    /// (which now reads the correct terminal status), store it, and return it.
    ///
    /// The summary is generated **after** the status is committed so that the
    /// markdown always reflects the true final state — including after Ctrl+C.
    ///
    /// US-072: Also captures `git_commit_after` and computes `run_hash` as the
    /// SHA-256 of audit-relevant fields + `prev_run_hash`. This builds a hash
    /// chain across runs of the same project — detecting tampering after the fact.
    pub fn finish_run(
        &self,
        status: RunStatus,
        exit_code: Option<i32>,
    ) -> Result<String> {
        let now = Utc::now().timestamp();

        // US-072: capture final git commit BEFORE taking the DB lock
        // (git rev-parse can take tens of ms and we don't want to hold the lock).
        let git_commit_after = capture_git_head(&self.project_path);

        {
            let conn = self.conn.lock().unwrap();

            // Close any open phases and steps.
            let orphan_status = if matches!(status, RunStatus::Aborted) {
                "aborted"
            } else {
                "failed"
            };
            conn.execute(
                "UPDATE steps SET ended_at = ?1, status = ?2
                 WHERE run_id = ?3 AND status = 'running'",
                params![now, orphan_status, &self.run_id],
            )
            .context("Failed to close open steps")?;
            conn.execute(
                "UPDATE phases SET ended_at = ?1, status = ?2
                 WHERE run_id = ?3 AND status = 'running'",
                params![now, orphan_status, &self.run_id],
            )
            .context("Failed to close open phases")?;

            // US-072: fetch the fields needed to compute the run hash
            let (started_at, operator_email, hostname, git_before, prev_hash): (
                i64, Option<String>, Option<String>, Option<String>, Option<String>,
            ) = conn.query_row(
                "SELECT started_at, operator_email, hostname, git_commit_before, prev_run_hash
                 FROM runs WHERE id = ?1",
                params![&self.run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .context("Failed to fetch audit fields for hash computation")?;

            let run_hash = compute_run_hash(
                &self.run_id,
                started_at,
                now,
                status.as_str(),
                exit_code,
                operator_email.as_deref(),
                hostname.as_deref(),
                git_before.as_deref(),
                git_commit_after.as_deref(),
                prev_hash.as_deref(),
            );

            // Commit status first (summary_md left empty for now — filled below).
            conn.execute(
                "UPDATE runs
                 SET ended_at = ?1, status = ?2, exit_code = ?3,
                     git_commit_after = ?4, run_hash = ?5
                 WHERE id = ?6",
                params![
                    now,
                    status.as_str(),
                    exit_code,
                    git_commit_after,
                    run_hash,
                    &self.run_id,
                ],
            )
            .context("Failed to update run row")?;
        } // ← release the lock before generating the summary

        // Generate summary now that the DB reflects the final status.
        let summary = self.generate_summary_markdown()
            .unwrap_or_else(|e| format!("# Summary unavailable: {}\n", e));

        // Store the generated summary back into the run row.
        {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute(
                "UPDATE runs SET summary_md = ?1 WHERE id = ?2",
                params![&summary, &self.run_id],
            );
        }

        Ok(summary)
    }

    /// Build a markdown summary from the DB state.
    ///
    /// Includes:
    /// - Run metadata (id, project, duration, status)
    /// - Per-phase table with metrics and AI cost
    /// - Top-N steps by AI cost
    /// - Token totals
    pub fn generate_summary_markdown(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();

        // Run header
        let mut stmt = conn.prepare(
            "SELECT project, started_at, ended_at, status, exit_code
             FROM runs WHERE id = ?1",
        )?;
        let (project, started_at, ended_at, status, exit_code): (String, i64, Option<i64>, String, Option<i64>) =
            stmt.query_row(params![&self.run_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
        drop(stmt);

        let elapsed_secs = self.start.elapsed().as_secs();
        let end_ts = ended_at.unwrap_or_else(|| Utc::now().timestamp());
        let wall_secs = (end_ts - started_at).max(0);

        let mut out = String::new();
        out.push_str(&format!("# Reparo Execution Summary — {}\n\n", self.run_id));
        out.push_str(&format!("- **Project**: {}\n", project));
        out.push_str(&format!(
            "- **Started**: {}\n",
            DateTime::<Utc>::from_timestamp(started_at, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| started_at.to_string())
        ));
        out.push_str(&format!(
            "- **Ended**: {}\n",
            DateTime::<Utc>::from_timestamp(end_ts, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| end_ts.to_string())
        ));
        out.push_str(&format!(
            "- **Duration**: {} (process wall time: {}s)\n",
            fmt_duration(wall_secs as u64),
            elapsed_secs
        ));
        out.push_str(&format!("- **Status**: {}\n", status));
        if let Some(code) = exit_code {
            out.push_str(&format!("- **Exit code**: {}\n", code));
        }
        out.push('\n');

        // US-072: Audit trail section (ENS Alto op.exp.10)
        let audit_row: rusqlite::Result<(
            Option<String>, Option<String>, Option<String>, Option<String>,
            Option<String>, Option<String>, Option<String>, Option<String>,
        )> = conn.query_row(
            "SELECT operator_name, operator_email, hostname, os_info,
                    git_commit_before, git_commit_after, run_hash, prev_run_hash
             FROM runs WHERE id = ?1",
            params![&self.run_id],
            |row| Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?,
            )),
        );
        if let Ok((op_name, op_email, host, os, git_before, git_after, hash, prev_hash)) = audit_row {
            let has_any = op_name.is_some() || op_email.is_some() || host.is_some()
                || git_before.is_some() || hash.is_some();
            if has_any {
                out.push_str("## Audit trail (ENS Alto op.exp.10 / IEC 62304 §8.2.4)\n\n");
                let operator = match (op_name.as_deref(), op_email.as_deref()) {
                    (Some(n), Some(e)) => format!("{} <{}>", n, e),
                    (Some(n), None) => n.to_string(),
                    (None, Some(e)) => e.to_string(),
                    (None, None) => "unknown".to_string(),
                };
                out.push_str(&format!("- **Operator**: {}\n", operator));
                out.push_str(&format!(
                    "- **Hostname**: {}{}\n",
                    host.as_deref().unwrap_or("unknown"),
                    os.as_deref().map(|s| format!(" ({})", s)).unwrap_or_default(),
                ));
                match (git_before.as_deref(), git_after.as_deref()) {
                    (Some(b), Some(a)) if b == a => {
                        out.push_str(&format!("- **Git HEAD**: `{}` (unchanged)\n", short_sha(b)));
                    }
                    (Some(b), Some(a)) => {
                        out.push_str(&format!(
                            "- **Git commits**: `{}` → `{}`\n",
                            short_sha(b),
                            short_sha(a),
                        ));
                    }
                    (Some(b), None) => {
                        out.push_str(&format!("- **Git HEAD before**: `{}`\n", short_sha(b)));
                    }
                    _ => {}
                }
                if let Some(ref h) = hash {
                    out.push_str(&format!("- **Run hash** (SHA-256): `{}`\n", h));
                }
                if let Some(ref p) = prev_hash {
                    out.push_str(&format!("- **Previous run hash**: `{}` (hash chain)\n", p));
                }
                out.push('\n');
            }
        }

        // Phases table
        out.push_str("## Phases\n\n");
        out.push_str("| # | Phase | Status | Duration | Before | After | Δ | AI calls | Input tok | Output tok |\n");
        out.push_str("|---|-------|--------|---------:|-------:|------:|---:|---------:|----------:|-----------:|\n");

        let mut stmt = conn.prepare(
            "SELECT p.id, p.seq, p.name, p.status, p.started_at, p.ended_at,
                    p.metric_before, p.metric_after, p.metric_unit,
                    COALESCE((SELECT COUNT(*) FROM ai_calls WHERE phase_id = p.id), 0) AS ai_count,
                    COALESCE((SELECT SUM(input_tokens + cache_read_tokens + cache_creation_tokens) FROM ai_calls WHERE phase_id = p.id), 0) AS in_tok,
                    COALESCE((SELECT SUM(output_tokens) FROM ai_calls WHERE phase_id = p.id), 0) AS out_tok
             FROM phases p
             WHERE p.run_id = ?1
             ORDER BY p.seq ASC",
        )?;

        let rows = stmt.query_map(params![&self.run_id], |row| {
            Ok(PhaseRow {
                seq: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get::<_, Option<i64>>(5)?,
                metric_before: row.get::<_, Option<f64>>(6)?,
                metric_after: row.get::<_, Option<f64>>(7)?,
                metric_unit: row.get::<_, Option<String>>(8)?,
                ai_count: row.get::<_, i64>(9)?,
                in_tok: row.get::<_, i64>(10)?,
                out_tok: row.get::<_, i64>(11)?,
            })
        })?;

        let mut total_in = 0i64;
        let mut total_out = 0i64;
        let mut total_ai = 0i64;
        for r in rows {
            let r = r?;
            let duration = r
                .ended_at
                .map(|e| (e - r.started_at).max(0))
                .map(|s| fmt_duration(s as u64))
                .unwrap_or_else(|| "—".to_string());
            let unit = r.metric_unit.as_deref().unwrap_or("");
            let before = r
                .metric_before
                .map(|v| format!("{:.1}{}", v, unit))
                .unwrap_or_else(|| "—".to_string());
            let after = r
                .metric_after
                .map(|v| format!("{:.1}{}", v, unit))
                .unwrap_or_else(|| "—".to_string());
            let delta = match (r.metric_before, r.metric_after) {
                (Some(b), Some(a)) => format!("{:+.1}{}", a - b, unit),
                _ => "—".to_string(),
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.seq,
                r.name,
                r.status,
                duration,
                before,
                after,
                delta,
                r.ai_count,
                fmt_num(r.in_tok as u64),
                fmt_num(r.out_tok as u64),
            ));
            total_in += r.in_tok;
            total_out += r.out_tok;
            total_ai += r.ai_count;
        }
        out.push_str(&format!(
            "| — | **TOTAL** | — | — | — | — | — | **{}** | **{}** | **{}** |\n\n",
            total_ai,
            fmt_num(total_in as u64),
            fmt_num(total_out as u64),
        ));
        drop(stmt);

        // Breakdown by model
        out.push_str("## Token usage by model\n\n");
        out.push_str("| Engine | Model | Effort | Calls | Input | Cache read | Output |\n");
        out.push_str("|--------|-------|--------|------:|------:|-----------:|-------:|\n");
        let mut stmt = conn.prepare(
            "SELECT engine, COALESCE(model, '—'), COALESCE(effort, '—'),
                    COUNT(*), SUM(input_tokens), SUM(cache_read_tokens), SUM(output_tokens)
             FROM ai_calls
             WHERE run_id = ?1
             GROUP BY engine, model, effort
             ORDER BY SUM(input_tokens + output_tokens) DESC",
        )?;
        let rows = stmt.query_map(params![&self.run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut any_model_rows = false;
        for r in rows {
            let (engine, model, effort, calls, input, cache_read, output) = r?;
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                engine,
                model,
                effort,
                calls,
                fmt_num(input as u64),
                fmt_num(cache_read as u64),
                fmt_num(output as u64),
            ));
            any_model_rows = true;
        }
        if !any_model_rows {
            out.push_str("| — | — | — | 0 | 0 | 0 | 0 |\n");
        }
        out.push('\n');
        drop(stmt);

        // Top 10 most expensive steps (by input tokens) — now with duration
        out.push_str("## Top 10 most expensive steps (by input tokens)\n\n");
        out.push_str("| Phase | Step | Target | Duration | AI calls | Input | Output |\n");
        out.push_str("|-------|------|--------|---------:|---------:|------:|-------:|\n");
        let mut stmt = conn.prepare(
            "SELECT p.name, s.name, COALESCE(s.target, '—'),
                    s.started_at, s.ended_at,
                    COUNT(a.id),
                    COALESCE(SUM(a.input_tokens + a.cache_read_tokens + a.cache_creation_tokens), 0),
                    COALESCE(SUM(a.output_tokens), 0)
             FROM steps s
             JOIN phases p ON p.id = s.phase_id
             LEFT JOIN ai_calls a ON a.step_id = s.id
             WHERE s.run_id = ?1
             GROUP BY s.id
             ORDER BY COALESCE(SUM(a.input_tokens), 0) DESC
             LIMIT 10",
        )?;
        let rows = stmt.query_map(params![&self.run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        let mut any_step_rows = false;
        for r in rows {
            let (phase, step, target, started_at, ended_at, calls, input, output) = r?;
            let duration = ended_at
                .map(|e| fmt_duration(((e - started_at).max(0)) as u64))
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                phase,
                step,
                truncate_middle(&target, 60),
                duration,
                calls,
                fmt_num(input as u64),
                fmt_num(output as u64),
            ));
            any_step_rows = true;
        }
        if !any_step_rows {
            out.push_str("| — | — | — | — | 0 | 0 | 0 |\n");
        }
        out.push('\n');
        drop(stmt);

        // Top 20 slowest steps (by wall-clock duration)
        out.push_str("## Top 20 slowest steps (by duration)\n\n");
        out.push_str("| Phase | Step | Target | Duration | AI calls |\n");
        out.push_str("|-------|------|--------|---------:|---------:|\n");
        let mut stmt = conn.prepare(
            "SELECT p.name, s.name, COALESCE(s.target, '—'),
                    s.started_at, s.ended_at,
                    COALESCE((SELECT COUNT(*) FROM ai_calls WHERE step_id = s.id), 0)
             FROM steps s
             JOIN phases p ON p.id = s.phase_id
             WHERE s.run_id = ?1 AND s.ended_at IS NOT NULL
             ORDER BY (s.ended_at - s.started_at) DESC
             LIMIT 20",
        )?;
        let rows = stmt.query_map(params![&self.run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut any_slow_rows = false;
        for r in rows {
            let (phase, step, target, started_at, ended_at, calls) = r?;
            let duration = fmt_duration(((ended_at - started_at).max(0)) as u64);
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                phase,
                step,
                truncate_middle(&target, 60),
                duration,
                calls,
            ));
            any_slow_rows = true;
        }
        if !any_slow_rows {
            out.push_str("| — | — | — | — | 0 |\n");
        }
        out.push('\n');

        out.push_str(&format!(
            "\n_Run data persisted to `{}` (run_id=`{}`)_\n",
            self.db_path.display(),
            self.run_id
        ));

        Ok(out)
    }

    // ─── US-068: Traceability matrix ──────────────────────────────────────────

    /// Record a final coverage snapshot for a file (US-068).
    ///
    /// Called at the end of the coverage boost phase to snapshot the final
    /// line/branch coverage per file. Used by the traceability matrix query.
    #[allow(dead_code)]
    pub fn upsert_final_coverage(
        &self,
        file: &str,
        line_coverage_pct: f64,
        branch_coverage_pct: f64,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO final_coverage (run_id, file, line_coverage_pct, branch_coverage_pct, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT DO NOTHING",
            params![&self.run_id, file, line_coverage_pct, branch_coverage_pct, now],
        )
        .or_else(|_| {
            // Fallback for SQLite versions without ON CONFLICT DO NOTHING on INSERT
            conn.execute(
                "INSERT OR REPLACE INTO final_coverage
                 (run_id, file, line_coverage_pct, branch_coverage_pct, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![&self.run_id, file, line_coverage_pct, branch_coverage_pct, now],
            )
        })
        .context("Failed to upsert final_coverage row")?;
        Ok(())
    }

    /// Record a MC/DC gap (US-070).
    #[allow(dead_code)]
    pub fn upsert_mcdc_gap(
        &self,
        file: &str,
        line: u32,
        condition_count: usize,
        tests_required: usize,
        tests_observed: usize,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let status = if tests_observed >= tests_required { "covered" } else { "gap" };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mcdc_gaps
             (run_id, file, line, condition_count, tests_required, tests_observed, status, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &self.run_id, file, line as i64,
                condition_count as i64, tests_required as i64, tests_observed as i64,
                status, now,
            ],
        )
        .context("Failed to insert mcdc_gap row")?;
        Ok(())
    }

    /// Generate the traceability matrix files (US-068).
    ///
    /// Creates:
    /// - `{dir}/traceability/matrix.md`  — markdown table
    /// - `{dir}/traceability/matrix.csv` — CSV (one row per test artifact)
    /// - `{dir}/traceability/matrix.csv.sha256` — SHA-256 of the CSV
    ///
    /// Gracefully handles 0 rows in `test_artifacts` (when US-066 is not yet
    /// generating trace blocks) by producing a "no tests traced yet" notice.
    ///
    /// `run_id` defaults to the current run when `None`.
    pub fn generate_traceability_matrix(
        &self,
        run_id: Option<&str>,
        dir: &Path,
        health_mode: bool,
    ) -> Result<PathBuf> {
        let rid = run_id.unwrap_or(&self.run_id);
        let trace_dir = dir.join("traceability");
        std::fs::create_dir_all(&trace_dir)
            .with_context(|| format!("Failed to create traceability dir {}", trace_dir.display()))?;

        let conn = self.conn.lock().unwrap();

        // Query test artifacts with coverage data
        let rows: Vec<TraceRow> = {
            let mut stmt = conn.prepare(
                "SELECT
                    ta.requirement,
                    ta.test_file,
                    ta.test_name,
                    ta.test_type,
                    COALESCE(ta.risk_class, 'N/A') as risk_class,
                    COALESCE(ta.source_file, '') as source_file,
                    ta.created_at,
                    COALESCE(fc.line_coverage_pct, 0.0) as line_cov,
                    COALESCE(fc.branch_coverage_pct, 0.0) as branch_cov
                 FROM test_artifacts ta
                 LEFT JOIN final_coverage fc
                   ON fc.run_id = ta.run_id AND fc.file = ta.source_file
                 WHERE ta.run_id = ?1
                 ORDER BY ta.requirement, ta.test_file, ta.test_name",
            )?;
            stmt.query_map(params![rid], |row| {
                Ok(TraceRow {
                    requirement: row.get(0)?,
                    test_file: row.get(1)?,
                    test_name: row.get(2)?,
                    test_type: row.get(3)?,
                    risk_class: row.get(4)?,
                    source_file: row.get(5)?,
                    created_at: row.get(6)?,
                    line_coverage_pct: row.get(7)?,
                    branch_coverage_pct: row.get(8)?,
                })
            })
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };

        // Generate markdown
        let md_content = build_matrix_markdown(rid, &rows, health_mode);
        let md_path = trace_dir.join("matrix.md");

        // SHA-256 of markdown excluding the hash line
        let md_hash = {
            let filtered: String = md_content.lines()
                .filter(|l| !l.contains("SHA-256 of this file"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut hasher = Sha256::new();
            hasher.update(filtered.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let md_with_hash = md_content.replace("{sha}", &md_hash);
        std::fs::write(&md_path, &md_with_hash)
            .with_context(|| format!("Failed to write matrix.md to {}", md_path.display()))?;

        // Generate CSV
        let csv_content = build_matrix_csv(rid, &rows);
        let csv_path = trace_dir.join("matrix.csv");
        std::fs::write(&csv_path, &csv_content)
            .with_context(|| format!("Failed to write matrix.csv to {}", csv_path.display()))?;

        // SHA-256 companion file
        let mut hasher = Sha256::new();
        hasher.update(csv_content.as_bytes());
        let csv_sha = format!("{:x}", hasher.finalize());
        let sha_path = trace_dir.join("matrix.csv.sha256");
        std::fs::write(&sha_path, &csv_sha)
            .with_context(|| format!("Failed to write matrix.csv.sha256 to {}", sha_path.display()))?;

        Ok(md_path)
    }

    /// Expose the connection guard for compliance report queries.
    /// Used by `compliance::report::build_report`.
    pub fn conn_for_compliance(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap()
    }
}

struct TraceRow {
    requirement: String,
    test_file: String,
    test_name: String,
    test_type: String,
    risk_class: String,
    source_file: String,
    created_at: i64,
    line_coverage_pct: f64,
    branch_coverage_pct: f64,
}

fn build_matrix_markdown(run_id: &str, rows: &[TraceRow], health_mode: bool) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let mut out = format!("# Traceability Matrix — {}\n\n", run_id);
    out.push_str(&format!("**Date**: {}\n", now));
    out.push_str(&format!("**Execution**: {}\n\n", run_id));

    // Summary
    let total = rows.len();
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Total test artifacts traced: {}\n\n", total));

    if rows.is_empty() {
        out.push_str("_No tests traced yet. Tests are traced when `--compliance` is active \
            and tests include `@Reparo.*` trace blocks (US-066)._\n\n");
    } else {
        // Group rows by source type (SONAR:, COVERAGE:, REQ-)
        let sonar_rows: Vec<_> = rows.iter().filter(|r| r.requirement.starts_with("SONAR:")).collect();
        let cov_rows: Vec<_> = rows.iter().filter(|r| r.requirement.starts_with("COVERAGE:")).collect();
        let manual_rows: Vec<_> = rows.iter().filter(|r| {
            !r.requirement.starts_with("SONAR:") && !r.requirement.starts_with("COVERAGE:")
        }).collect();

        let risk_col = if health_mode { " Risk |" } else { "" };
        let risk_header = if health_mode { " Risk |" } else { "" };

        if !sonar_rows.is_empty() {
            out.push_str("## Requirements from SonarQube rules\n\n");
            out.push_str(&format!("| Req ID | File | Test |{risk_header} Type | Cov. |\n"));
            out.push_str(&format!("|--------|------|------|{}-------|------|\n", if health_mode { "------|" } else { "" }));
            for r in &sonar_rows {
                let risk = if health_mode { format!(" {} |", r.risk_class) } else { String::new() };
                out.push_str(&format!(
                    "| {} | {} | {} |{} {} | {:.0}% |\n",
                    r.requirement, r.source_file, r.test_name,
                    risk, r.test_type, r.line_coverage_pct,
                ));
            }
            out.push('\n');
        }

        if !cov_rows.is_empty() {
            out.push_str("## Requirements from coverage gaps\n\n");
            out.push_str(&format!("| Req ID | File | Test |{risk_col} Type | Line / Branch |\n"));
            out.push_str(&format!("|--------|------|------|{}-------|---------------|\n", if health_mode { "------|" } else { "" }));
            for r in &cov_rows {
                let risk = if health_mode { format!(" {} |", r.risk_class) } else { String::new() };
                out.push_str(&format!(
                    "| {} | {} | {} |{} {} | {:.0}% / {:.0}% |\n",
                    r.requirement, r.source_file, r.test_name,
                    risk, r.test_type, r.line_coverage_pct, r.branch_coverage_pct,
                ));
            }
            out.push('\n');
        }

        if !manual_rows.is_empty() {
            out.push_str("## Requirements from reparo.yaml (manual)\n\n");
            out.push_str(&format!("| Req ID | Test |{risk_col} Type | Status |\n"));
            out.push_str(&format!("|--------|------|{}-------|--------|\n", if health_mode { "------|" } else { "" }));
            for r in &manual_rows {
                let risk = if health_mode { format!(" {} |", r.risk_class) } else { String::new() };
                out.push_str(&format!(
                    "| {} | {} |{} {} | pass |\n",
                    r.requirement, r.test_name, risk, r.test_type,
                ));
            }
            out.push('\n');
        }
    }

    out.push_str("## Gaps\n\n");
    out.push_str("_Requirements without a linked test (non-compliant):_\n\n");
    out.push_str("(See compliance report for orphan requirement details)\n\n");

    out.push_str("## Hash\n\n");
    out.push_str("SHA-256 of this file (excluding this line): `{sha}`\n");

    out
}

fn build_matrix_csv(run_id: &str, rows: &[TraceRow]) -> String {
    let mut out = String::from(
        "run_id,requirement_id,test_file,test_name,test_type,risk_class,\
         source_file,line_coverage_pct,branch_coverage_pct,created_at\n"
    );
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{:.1},{:.1},{}\n",
            csv_escape(run_id),
            csv_escape(&r.requirement),
            csv_escape(&r.test_file),
            csv_escape(&r.test_name),
            csv_escape(&r.test_type),
            csv_escape(&r.risk_class),
            csv_escape(&r.source_file),
            r.line_coverage_pct,
            r.branch_coverage_pct,
            r.created_at,
        ));
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

impl Drop for ExecutionLog {
    fn drop(&mut self) {
        // Best-effort: if the run is still 'running' when we drop (unlikely, since
        // finish_run is called explicitly), mark it as failed so historical data
        // stays consistent.
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "UPDATE runs SET status = 'failed', ended_at = COALESCE(ended_at, ?1)
                 WHERE id = ?2 AND status = 'running'",
                params![Utc::now().timestamp(), &self.run_id],
            );
        }
    }
}

struct PhaseRow {
    seq: i64,
    name: String,
    status: String,
    started_at: i64,
    ended_at: Option<i64>,
    metric_before: Option<f64>,
    metric_after: Option<f64>,
    metric_unit: Option<String>,
    ai_count: i64,
    in_tok: i64,
    out_tok: i64,
}

fn sanitize_project(project: &str) -> String {
    let s: String = project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() {
        "project".to_string()
    } else {
        s
    }
}

fn fmt_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m{:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// US-072: shorten a git SHA to its first 10 chars for display, preserving
/// full-length output in the DB.
fn short_sha(sha: &str) -> String {
    if sha.len() > 10 {
        sha[..10].to_string()
    } else {
        sha.to_string()
    }
}

fn fmt_num(n: u64) -> String {
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

fn truncate_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = (max.saturating_sub(3)) / 2;
    let start: String = s.chars().take(keep).collect();
    let end: String = s.chars().skip(count - keep).collect();
    format!("{}...{}", start, end)
}

/// Best-effort: append a log line that a phase was started, with timestamp.
/// Used from hot paths where `Result` propagation is noisy.
#[allow(dead_code)]
pub fn try_log<E, F: FnOnce() -> Result<E>>(op: &str, f: F) -> Option<E> {
    match f() {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("execution_log: {} failed: {}", op, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::TokenUsage;
    use tempfile::tempdir;

    fn sample_entry() -> UsageEntry {
        UsageEntry {
            step: "coverage_boost".into(),
            engine: crate::engine::EngineKind::Claude,
            model: "sonnet".into(),
            usage: TokenUsage {
                input: 100,
                output: 50,
                cache_read: 10,
                cache_creation: 5,
            },
            unknown: false,
        }
    }

    #[test]
    fn run_id_format() {
        let id = ExecutionLog::generate_run_id("my-project");
        // YYYYMMDD_HHMMSS_my-project
        assert_eq!(id.len(), 8 + 1 + 6 + 1 + "my-project".len());
        assert!(id.contains("_my-project"));
    }

    #[test]
    fn run_id_sanitizes_project_name() {
        let id = ExecutionLog::generate_run_id("proyecto con espacios/y barras!");
        assert!(!id.contains(' '));
        assert!(!id.contains('/'));
        assert!(!id.contains('!'));
    }

    #[test]
    fn init_creates_schema_and_row() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("test.db");
        let project_path = tmp.path();
        let log = ExecutionLog::init(&db, "myproj", project_path, Some("sonar-id"), Some("main"), None).unwrap();
        assert!(db.exists());
        assert!(log.run_id().contains("myproj"));
    }

    #[test]
    fn phase_and_step_lifecycle() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("test.db");
        let log = ExecutionLog::init(&db, "proj", tmp.path(), None, None, None).unwrap();

        let phase = log.start_phase("coverage_boost", Some(45.0), Some("%")).unwrap();
        let step = log.start_step(phase, "boost_file", Some("src/foo.rs"), Some(30.0)).unwrap();
        log.log_ai_call(Some(phase), Some(step), &sample_entry(), Some("medium"), Some(1500)).unwrap();
        log.finish_step(step, ItemStatus::Completed, Some(80.0), None).unwrap();
        log.finish_phase(phase, ItemStatus::Completed, Some(75.0), None).unwrap();
        log.finish_run(RunStatus::Completed, Some(0)).unwrap();

        let summary = log.generate_summary_markdown().unwrap();
        assert!(summary.contains("coverage_boost"));
        assert!(summary.contains("boost_file"));
        assert!(summary.contains("sonnet"));
        assert!(summary.contains("completed"));
    }

    // -- US-072: audit fields (ENS Alto op.exp.10) --

    #[test]
    fn audit_run_hash_is_deterministic() {
        // Same inputs must always produce the same hash (verifiable by auditors).
        let h1 = compute_run_hash(
            "20260412_143022_proj", 100, 200, "completed", Some(0),
            Some("alice@example.com"), Some("host-1"),
            Some("abc123"), Some("def456"), None,
        );
        let h2 = compute_run_hash(
            "20260412_143022_proj", 100, 200, "completed", Some(0),
            Some("alice@example.com"), Some("host-1"),
            Some("abc123"), Some("def456"), None,
        );
        assert_eq!(h1, h2);
        // Basic sanity: SHA-256 hex is 64 chars
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn audit_run_hash_changes_with_any_field() {
        let base = compute_run_hash(
            "id", 100, 200, "completed", Some(0),
            Some("alice@example.com"), Some("host-1"),
            Some("abc"), Some("def"), None,
        );
        // Change exit_code → different hash
        let diff_exit = compute_run_hash(
            "id", 100, 200, "completed", Some(1),
            Some("alice@example.com"), Some("host-1"),
            Some("abc"), Some("def"), None,
        );
        assert_ne!(base, diff_exit);
        // Change operator → different hash
        let diff_op = compute_run_hash(
            "id", 100, 200, "completed", Some(0),
            Some("mallory@evil.com"), Some("host-1"),
            Some("abc"), Some("def"), None,
        );
        assert_ne!(base, diff_op);
        // Change prev_hash → different hash (chain)
        let diff_chain = compute_run_hash(
            "id", 100, 200, "completed", Some(0),
            Some("alice@example.com"), Some("host-1"),
            Some("abc"), Some("def"), Some("prev"),
        );
        assert_ne!(base, diff_chain);
    }

    #[test]
    fn audit_info_captured_in_runs_row() {
        // After init, the runs row should have hostname/os_info at minimum.
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("audit.db");
        let log = ExecutionLog::init(&db, "proj", tmp.path(), Some("SPID"), None, None).unwrap();
        log.finish_run(RunStatus::Completed, Some(0)).unwrap();

        // Directly query the DB to verify
        let conn = rusqlite::Connection::open(&db).unwrap();
        let (os_info, run_hash): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT os_info, run_hash FROM runs WHERE id = ?1",
                rusqlite::params![log.run_id()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(os_info.is_some(), "os_info should be captured");
        assert!(run_hash.is_some(), "run_hash should be computed at finish_run");
    }

    #[test]
    fn audit_hash_chain_between_sequential_runs() {
        // Two runs of the same sonar_project_id should link: run2.prev_run_hash == run1.run_hash
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("chain.db");

        let log1 = ExecutionLog::init(&db, "proj", tmp.path(), Some("SPID"), None, None).unwrap();
        log1.finish_run(RunStatus::Completed, Some(0)).unwrap();
        let run1_id = log1.run_id().to_string();
        drop(log1);

        // Sleep briefly to ensure started_at differs (1-second resolution)
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let log2 = ExecutionLog::init(&db, "proj", tmp.path(), Some("SPID"), None, None).unwrap();
        log2.finish_run(RunStatus::Completed, Some(0)).unwrap();
        let run2_id = log2.run_id().to_string();
        drop(log2);

        let conn = rusqlite::Connection::open(&db).unwrap();
        let run1_hash: String = conn
            .query_row("SELECT run_hash FROM runs WHERE id = ?1", rusqlite::params![&run1_id], |r| r.get(0))
            .unwrap();
        let run2_prev: String = conn
            .query_row("SELECT prev_run_hash FROM runs WHERE id = ?1", rusqlite::params![&run2_id], |r| r.get(0))
            .unwrap();
        assert_eq!(run1_hash, run2_prev, "hash chain must link consecutive runs");
    }

    #[test]
    fn audit_section_appears_in_summary() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("summary.db");
        let log = ExecutionLog::init(&db, "proj", tmp.path(), None, None, None).unwrap();
        log.finish_run(RunStatus::Completed, Some(0)).unwrap();
        let summary = log.generate_summary_markdown().unwrap();
        // os_info is always captured, so the audit section must appear
        assert!(summary.contains("Audit trail"), "summary should include audit section: {}", summary);
        assert!(summary.contains("Run hash"));
    }

    #[test]
    fn write_summary_file_creates_report_dir_and_latest() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("exec.db");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let log = ExecutionLog::init(&db, "proj", &project, None, None, None).unwrap();

        let summary = "# Test summary\n\nBody content.";
        let written = log
            .write_summary_file(&project, ".reparo", summary)
            .unwrap();

        // execution_<run_id>.md written
        assert!(written.exists());
        let content = std::fs::read_to_string(&written).unwrap();
        assert!(content.contains("Test summary"));

        // latest.md also written
        let latest = project.join(".reparo").join("latest.md");
        assert!(latest.exists());
        let latest_content = std::fs::read_to_string(&latest).unwrap();
        assert_eq!(latest_content, summary);
    }

    #[test]
    fn write_summary_file_accepts_absolute_path() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("exec.db");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let log = ExecutionLog::init(&db, "proj", &project, None, None, None).unwrap();

        let abs_dir = tmp.path().join("reports-absolute");
        let written = log
            .write_summary_file(&project, abs_dir.to_str().unwrap(), "# abs")
            .unwrap();
        assert!(written.starts_with(&abs_dir));
    }

    #[test]
    fn finish_run_closes_orphan_phases() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("test.db");
        let log = ExecutionLog::init(&db, "proj", tmp.path(), None, None, None).unwrap();
        let _phase = log.start_phase("fix_loop", None, None).unwrap();
        // Simulate Ctrl+C: finish run without finishing phase
        log.finish_run(RunStatus::Aborted, Some(130)).unwrap();

        let summary = log.generate_summary_markdown().unwrap();
        assert!(summary.contains("aborted"));
    }

    #[test]
    fn summary_contains_token_breakdown() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("test.db");
        let log = ExecutionLog::init(&db, "proj", tmp.path(), None, None, None).unwrap();
        let phase = log.start_phase("coverage_boost", Some(50.0), Some("%")).unwrap();
        log.log_ai_call(Some(phase), None, &sample_entry(), Some("medium"), Some(1000)).unwrap();
        log.finish_phase(phase, ItemStatus::Completed, Some(75.0), None).unwrap();
        log.finish_run(RunStatus::Completed, Some(0)).unwrap();

        let summary = log.generate_summary_markdown().unwrap();
        assert!(summary.contains("Token usage by model"));
        assert!(summary.contains("claude"));
        assert!(summary.contains("sonnet"));
    }

    #[test]
    fn fmt_duration_formats() {
        assert_eq!(fmt_duration(5), "5s");
        assert_eq!(fmt_duration(65), "1m05s");
        assert_eq!(fmt_duration(3661), "1h01m01s");
    }

    #[test]
    fn truncate_middle_keeps_start_and_end() {
        let s = "src/main/java/com/example/service/OrderProcessorServiceImpl.java";
        let t = truncate_middle(s, 30);
        assert!(t.len() <= 31); // +1 for the "..."
        assert!(t.contains("..."));
        assert!(t.starts_with("src/main"));
    }
}
