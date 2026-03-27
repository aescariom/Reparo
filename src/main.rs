#[allow(dead_code)]
mod claude;
mod config;
mod engine;
#[allow(dead_code)]
mod git;
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
mod yaml_config;

use clap::Parser;
use config::Config;
use orchestrator::Orchestrator;
use tracing_subscriber::EnvFilter;

/// Exit codes (US-012):
/// - 0: all issues fixed successfully (or none found, or dry-run)
/// - 1: fatal error (configuration, connectivity)
/// - 2: partial success (some fixed, some failed)
/// - 3: unexpected error
#[tokio::main]
async fn main() {
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

    // Create orchestrator
    let mut orchestrator = match Orchestrator::new(validated) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Initialization error: {:#}", e);
            std::process::exit(1);
        }
    };

    // US-012: Run with optional global timeout
    let exit_code = if global_timeout > 0 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(global_timeout),
            orchestrator.run(),
        )
        .await
        {
            Ok(Ok(code)) => code,
            Ok(Err(e)) => {
                eprintln!("Error: {:#}", e);
                classify_error(&e)
            }
            Err(_) => {
                eprintln!(
                    "Global timeout reached ({}s). Generating partial report.",
                    global_timeout
                );
                orchestrator.generate_partial_report();
                2
            }
        }
    } else {
        match orchestrator.run().await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("Error: {:#}", e);
                classify_error(&e)
            }
        }
    };

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
