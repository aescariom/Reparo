//! Execution state persistence for resume support (US-017).

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const STATE_FILE: &str = ".reparo-state.json";
const STATE_TMP: &str = ".reparo-state.json.tmp";

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutionState {
    pub version: u32,
    pub started_at: String,
    pub sonar_project_id: String,
    pub branch: String,
    pub batch_size: usize,
    pub total_issues: usize,
    pub processed: Vec<ProcessedIssue>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProcessedIssue {
    pub key: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ExecutionState {
    pub fn new(sonar_project_id: &str, branch: &str, batch_size: usize, total_issues: usize) -> Self {
        Self {
            version: 1,
            started_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            sonar_project_id: sonar_project_id.to_string(),
            branch: branch.to_string(),
            batch_size,
            total_issues,
            processed: Vec::new(),
        }
    }

    /// Add a processed issue to the state.
    pub fn add_processed(&mut self, key: &str, status: &str, pr_url: Option<&str>, reason: Option<&str>) {
        self.processed.push(ProcessedIssue {
            key: key.to_string(),
            status: status.to_string(),
            pr_url: pr_url.map(String::from),
            reason: reason.map(String::from),
        });
    }

    /// Get all processed issue keys as a set.
    pub fn processed_keys(&self) -> HashSet<String> {
        self.processed.iter().map(|p| p.key.clone()).collect()
    }

    /// Check if this state is compatible with the given config.
    pub fn is_compatible(&self, sonar_project_id: &str, branch: &str) -> bool {
        self.sonar_project_id == sonar_project_id && self.branch == branch
    }
}

/// Save execution state atomically (write to tmp, rename).
pub fn save_state(project_path: &Path, state: &ExecutionState) -> Result<()> {
    let tmp_path = project_path.join(STATE_TMP);
    let final_path = project_path.join(STATE_FILE);

    let json = serde_json::to_string_pretty(state)
        .context("Failed to serialize execution state")?;
    std::fs::write(&tmp_path, &json)
        .context("Failed to write state temp file")?;
    std::fs::rename(&tmp_path, &final_path)
        .context("Failed to rename state file")?;

    Ok(())
}

/// Load existing execution state, if any.
pub fn load_state(project_path: &Path) -> Result<Option<ExecutionState>> {
    let path = project_path.join(STATE_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .context("Failed to read state file")?;
    let state: ExecutionState = serde_json::from_str(&content)
        .context("Failed to parse state file")?;

    Ok(Some(state))
}

/// Remove the state file (called on successful completion).
pub fn remove_state(project_path: &Path) {
    let path = project_path.join(STATE_FILE);
    let _ = std::fs::remove_file(path);
    let tmp = project_path.join(STATE_TMP);
    let _ = std::fs::remove_file(tmp);
}

/// Get the state file path for display.
#[allow(dead_code)]
pub fn state_file_path(project_path: &Path) -> PathBuf {
    project_path.join(STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_state() {
        let state = ExecutionState::new("my-proj", "main", 1, 50);
        assert_eq!(state.version, 1);
        assert_eq!(state.sonar_project_id, "my-proj");
        assert_eq!(state.branch, "main");
        assert!(state.processed.is_empty());
    }

    #[test]
    fn test_add_processed() {
        let mut state = ExecutionState::new("proj", "main", 1, 10);
        state.add_processed("AX-1", "fixed", Some("https://pr/1"), None);
        state.add_processed("AX-2", "failed", None, Some("Claude error"));
        assert_eq!(state.processed.len(), 2);
        assert_eq!(state.processed[0].key, "AX-1");
        assert_eq!(state.processed[1].reason.as_deref(), Some("Claude error"));
    }

    #[test]
    fn test_processed_keys() {
        let mut state = ExecutionState::new("proj", "main", 1, 10);
        state.add_processed("AX-1", "fixed", None, None);
        state.add_processed("AX-2", "failed", None, None);
        let keys = state.processed_keys();
        assert!(keys.contains("AX-1"));
        assert!(keys.contains("AX-2"));
        assert!(!keys.contains("AX-3"));
    }

    #[test]
    fn test_is_compatible() {
        let state = ExecutionState::new("proj", "main", 1, 10);
        assert!(state.is_compatible("proj", "main"));
        assert!(!state.is_compatible("other-proj", "main"));
        assert!(!state.is_compatible("proj", "develop"));
    }

    #[test]
    fn test_save_and_load_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = ExecutionState::new("proj", "main", 5, 100);
        state.add_processed("AX-1", "fixed", Some("https://pr/1"), None);

        save_state(tmp.path(), &state).unwrap();

        let loaded = load_state(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.sonar_project_id, "proj");
        assert_eq!(loaded.processed.len(), 1);
        assert_eq!(loaded.processed[0].key, "AX-1");
    }

    #[test]
    fn test_load_state_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_state(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_remove_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = ExecutionState::new("proj", "main", 1, 10);
        save_state(tmp.path(), &state).unwrap();
        assert!(tmp.path().join(".reparo-state.json").exists());

        remove_state(tmp.path());
        assert!(!tmp.path().join(".reparo-state.json").exists());
    }

    #[test]
    fn test_state_serialization_roundtrip() {
        let mut state = ExecutionState::new("proj", "dev", 3, 50);
        state.add_processed("K1", "fixed", Some("url"), None);
        state.add_processed("K2", "needs_review", None, Some("tests fail"));
        state.add_processed("K3", "skipped", None, None);

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.processed.len(), 3);
        assert!(parsed.processed[2].reason.is_none());
        assert!(parsed.processed[2].pr_url.is_none());
    }
}
