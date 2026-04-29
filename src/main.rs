mod autofix_sonar;
#[allow(dead_code)]
mod claude;
mod compliance;
mod config;
mod engine;
mod execution_log;
#[allow(dead_code)]
mod git;
mod linter;
mod orchestrator;
#[allow(dead_code)]
mod pact;
mod report;
mod retry;
#[allow(dead_code)]
mod runner;
#[allow(dead_code)]
mod sonar;
mod state;
mod usage;
mod yaml_config;

use clap::Parser;
use config::Config;
use execution_log::{ExecutionLog, RunStatus};
use orchestrator::Orchestrator;
use std::sync::{Arc, OnceLock};
use tracing_subscriber::EnvFilter;

/// Global handle to the execution log so the Ctrl+C handler can finalize it.
/// Set once at startup, read from the signal handler and from normal exit paths.
static EXECUTION_LOG: OnceLock<Arc<ExecutionLog>> = OnceLock::new();

/// Exit codes (US-012):
/// - 0: all issues fixed successfully (or none found, or dry-run)
/// - 1: fatal error (configuration, connectivity)
/// - 2: partial success (some fixed, some failed)
/// - 3: unexpected error
fn main() {
    // Disable every interactive prompt git might emit, before we spawn any
    // subprocess. `git` child processes inherit the parent's stdin, so without
    // this a prompt (credential helper, GPG passphrase, terminal username)
    // silently blocks forever on a tty read with no log output.
    //
    //   GIT_TERMINAL_PROMPT=0  — git will not read credentials from the tty
    //   GIT_ASKPASS=/bin/true  — disables fallback askpass helpers
    //   SSH_ASKPASS=/bin/true  — same for ssh-driven fetches
    //   GCM_INTERACTIVE=Never  — git-credential-manager (if present)
    //
    // Commit signing is also gated: a worker running in parallel cannot type
    // a gpg passphrase, so we force it off at the process level. Users who
    // need signed commits should run reparo sequentially or configure a cached
    // gpg-agent and set REPARO_ALLOW_GPG_SIGN=1 to opt back in.
    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_ASKPASS").is_none() {
        std::env::set_var("GIT_ASKPASS", "/bin/true");
    }
    if std::env::var_os("SSH_ASKPASS").is_none() {
        std::env::set_var("SSH_ASKPASS", "/bin/true");
    }
    std::env::set_var("GCM_INTERACTIVE", "Never");
    if std::env::var_os("REPARO_ALLOW_GPG_SIGN").is_none() {
        // Overrides any `commit.gpgsign = true` in the user's ~/.gitconfig.
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_0", "false");
    }

    // Build an explicit tokio runtime with enough worker threads to tolerate
    // sync-blocking work inside `tokio::spawn` tasks. The default
    // `#[tokio::main]` picks `num_cpus()` — on a 4-core machine with
    // `--parallel 4`, every worker thread ends up blocked in sync subprocess
    // waits, so the main thread's `handle.await` and HTTP polls cannot be
    // scheduled. We give tokio a generous floor (32) plus a margin above
    // `--parallel` so there are always threads free for async progress.
    //
    // `max_blocking_threads` governs the separate pool used by
    // `tokio::task::spawn_blocking`; lift it too in case we route blocking
    // work through it in the future.
    // Parse `--parallel N`, `--parallel=N`, `-p N`, `-p=N` without pulling in
    // the real clap parser (runtime must be built before `Config::parse()`
    // runs). Worst case we fall back to 1 and rely on the 32-thread floor.
    let args: Vec<String> = std::env::args().collect();
    let parallel_hint: usize = {
        let mut found: Option<usize> = None;
        for (i, a) in args.iter().enumerate() {
            if let Some(eq) = a.strip_prefix("--parallel=") {
                found = eq.parse().ok();
                break;
            }
            if let Some(eq) = a.strip_prefix("-p=") {
                found = eq.parse().ok();
                break;
            }
            if (a == "--parallel" || a == "-p") && i + 1 < args.len() {
                found = args[i + 1].parse().ok();
                break;
            }
        }
        found.unwrap_or(1)
    };
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let worker_threads = cpu_count.max(parallel_hint * 3).max(32);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .max_blocking_threads(512)
        .thread_name("reparo-worker")
        .build()
        .expect("Failed to build tokio runtime");

    runtime.block_on(async_main());
}

async fn async_main() {
    let config = Config::parse();

    // Set up logging before validation so errors are visible
    let log_json = config.log_format == "json";
    if log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(
                EnvFilter::from_default_env()
                    .add_directive("reparo=info".parse().unwrap()),
            )
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env()
                    .add_directive("reparo=info".parse().unwrap()),
            )
            .init();
    }

    // Handle --restore-personal-yaml before validation
    if config.restore_personal_yaml {
        match yaml_config::restore_personal_config() {
            Ok(()) => {
                println!(
                    "Personal config (~/.config/reparo/config.yaml) restored to defaults for v{}",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Failed to restore personal config: {:#}", e);
                std::process::exit(1);
            }
        }
    }

    // US-001: Validate all local configuration
    let global_timeout = config.timeout;
    let validated = match config.validate() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Configuration error: {:#}", e);
            std::process::exit(1);
        }
    };

    // Initialize execution log (real-time SQLite persistence)
    let project_name = validated
        .sonar_project_id
        .split(':')
        .last()
        .unwrap_or(&validated.sonar_project_id)
        .to_string();
    let db_path = ExecutionLog::default_db_path();
    let exec_log = match ExecutionLog::init(
        &db_path,
        &project_name,
        &validated.path,
        Some(&validated.sonar_project_id),
        Some(&validated.branch),
        None,
    ) {
        Ok(l) => {
            tracing::info!(
                "Execution log initialized: {} (db={})",
                l.run_id(),
                l.db_path().display()
            );
            Arc::new(l)
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize execution log: {:#}. Continuing without persistence.",
                e
            );
            // Create a throwaway in-memory log so the rest of the code has a handle
            Arc::new(
                ExecutionLog::init(
                    std::path::Path::new(":memory:"),
                    &project_name,
                    &validated.path,
                    None,
                    None,
                    None,
                )
                .expect("in-memory fallback should always succeed"),
            )
        }
    };
    let _ = EXECUTION_LOG.set(exec_log.clone());

    // Install Ctrl+C handler — finalize the run as aborted, write the report, exit 130.
    let exec_log_for_signal = exec_log.clone();
    let project_path_for_signal = validated.path.clone();
    let report_dir_for_signal = validated.execution_log_report_dir.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            eprintln!("\nCtrl+C received — finalizing execution log and exiting…");
            let summary = exec_log_for_signal
                .finish_run(RunStatus::Aborted, Some(130))
                .unwrap_or_else(|e| format!("# Summary unavailable: {}\n", e));
            match exec_log_for_signal.write_summary_file(
                &project_path_for_signal,
                &report_dir_for_signal,
                &summary,
            ) {
                Ok(path) => eprintln!("Execution summary written to {}", path.display()),
                Err(e) => eprintln!("Failed to write execution summary: {}", e),
            }
            eprintln!("{}", summary);
            std::process::exit(130);
        }
    });

    // Capture values needed after `validated` is moved into the orchestrator
    let project_path_final = validated.path.clone();
    let report_dir_final = validated.execution_log_report_dir.clone();

    // Create orchestrator (shares the execution log)
    let mut orchestrator = match Orchestrator::new(validated, exec_log.clone()) {
        Ok(o) => o,
        Err(e) => {
            let msg = format!("Initialization error: {:#}", e);
            eprintln!("{}", msg);
            let summary = exec_log.finish_run(RunStatus::Failed, Some(1)).unwrap_or_default();
            let _ = exec_log.write_summary_file(&project_path_final, &report_dir_final, &summary);
            std::process::exit(1);
        }
    };

    // US-012: Run with optional global timeout
    let (exit_code, run_status) = if global_timeout > 0 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(global_timeout),
            orchestrator.run(),
        )
        .await
        {
            Ok(Ok(code)) => {
                let status = if code == 0 { RunStatus::Completed } else { RunStatus::Failed };
                (code, status)
            }
            Ok(Err(e)) => {
                eprintln!("Error: {:#}", e);
                (classify_error(&e), RunStatus::Failed)
            }
            Err(_) => {
                eprintln!(
                    "Global timeout reached ({}s). Generating partial report.",
                    global_timeout
                );
                orchestrator.generate_partial_report();
                (2, RunStatus::Failed)
            }
        }
    } else {
        match orchestrator.run().await {
            Ok(code) => {
                let status = if code == 0 { RunStatus::Completed } else { RunStatus::Failed };
                (code, status)
            }
            Err(e) => {
                eprintln!("Error: {:#}", e);
                (classify_error(&e), RunStatus::Failed)
            }
        }
    };

    // Finalize the execution log with the summary (DB + markdown file)
    let summary = exec_log
        .finish_run(run_status, Some(exit_code))
        .unwrap_or_else(|e| format!("# Summary unavailable: {}\n", e));
    match exec_log.write_summary_file(&project_path_final, &report_dir_final, &summary) {
        Ok(path) => println!("\nExecution summary written to {}", path.display()),
        Err(e) => eprintln!("\nFailed to write execution summary: {}", e),
    }
    println!("\n{}", summary);

    std::process::exit(exit_code);
}

/// Classify an error into an exit code.
fn classify_error(e: &anyhow::Error) -> i32 {
    let msg = format!("{:#}", e);
    if msg.contains("connect")
        || msg.contains("SonarQube")
        || msg.contains("not accessible")
        || msg.contains("sonar-scanner")
        || msg.contains("Pre-flight")
        || msg.contains("Setup command")
    {
        1
    } else {
        3
    }
}
