use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

/// Git hosting platform detected from the remote URL.
#[derive(Debug, Clone, PartialEq)]
pub enum GitPlatform {
    GitHub,
    GitLab,
}

/// Detect the git hosting platform by inspecting the `origin` remote URL.
/// Falls back to [`GitPlatform::GitHub`] if the URL cannot be read or is unrecognised.
pub fn detect_platform(project_path: &Path) -> GitPlatform {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["remote", "get-url", "origin"])
        .output();

    if let Ok(out) = output {
        let url = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if url.contains("gitlab") {
            return GitPlatform::GitLab;
        }
    }
    GitPlatform::GitHub
}

/// Get the current branch name
pub fn current_branch(project_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("Failed to get current branch")?;

    if !output.status.success() {
        anyhow::bail!("Not a git repository: {}", project_path.display());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Create and checkout a new branch from the base branch
pub fn create_branch(project_path: &Path, branch_name: &str, base_branch: &str) -> Result<()> {
    // First ensure we're on the base branch
    checkout(project_path, base_branch)?;

    // Delete branch if it already exists (idempotency)
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["branch", "-D", branch_name])
        .output();

    let status = Command::new("git")
        .current_dir(project_path)
        .args(["checkout", "-b", branch_name])
        .status()
        .context("Failed to create branch")?;

    if !status.success() {
        anyhow::bail!("Failed to create branch: {}", branch_name);
    }

    info!("Created branch: {}", branch_name);
    Ok(())
}

/// Checkout an existing branch
pub fn checkout(project_path: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["checkout", branch])
        .status()
        .context("Failed to checkout branch")?;

    if !status.success() {
        anyhow::bail!("Failed to checkout branch: {}", branch);
    }
    Ok(())
}

/// Stage specific files
pub fn add_files(project_path: &Path, files: &[&str]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(project_path).arg("add");
    for f in files {
        cmd.arg(f);
    }

    let status = cmd.status().context("Failed to git add")?;
    if !status.success() {
        anyhow::bail!("git add failed");
    }
    Ok(())
}

/// Stage all changed files
pub fn add_all(project_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["add", "-A"])
        .status()
        .context("Failed to git add -A")?;

    if !status.success() {
        anyhow::bail!("git add -A failed");
    }
    Ok(())
}

/// Check if there are staged changes
pub fn has_staged_changes(project_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .context("Failed to check staged changes")?;

    // exit code 1 means there are differences
    Ok(!output.success())
}

/// Create a commit with the given message
pub fn commit(project_path: &Path, message: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["commit", "-m", message])
        .status()
        .context("Failed to create commit")?;

    if !status.success() {
        anyhow::bail!("git commit failed");
    }
    Ok(())
}

/// Push the current branch to origin
pub fn push(project_path: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["push", "-u", "origin", branch])
        .status()
        .context("Failed to push branch")?;

    if !status.success() {
        anyhow::bail!("git push failed for branch: {}", branch);
    }

    info!("Pushed branch: {}", branch);
    Ok(())
}

/// Create a pull/merge request using the appropriate CLI for the detected platform.
///
/// - GitHub → `gh pr create`
/// - GitLab → `glab mr create`
///
/// `labels` is a slice of label names to apply. Labels that don't exist on the
/// repo will cause a warning but won't fail the MR/PR creation.
pub fn create_pr(
    project_path: &Path,
    title: &str,
    body: &str,
    base_branch: &str,
    labels: &[&str],
) -> Result<String> {
    match detect_platform(project_path) {
        GitPlatform::GitHub => create_pr_github(project_path, title, body, base_branch, labels),
        GitPlatform::GitLab => create_mr_gitlab(project_path, title, body, base_branch, labels),
    }
}

fn create_pr_github(
    project_path: &Path,
    title: &str,
    body: &str,
    base_branch: &str,
    labels: &[&str],
) -> Result<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
        "--base".to_string(),
        base_branch.to_string(),
    ];

    if !labels.is_empty() {
        args.push("--label".to_string());
        args.push(labels.join(","));
    }

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = Command::new("gh")
        .current_dir(project_path)
        .args(&args_ref)
        .output()
        .context("Failed to create PR (is gh CLI installed and authenticated?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("label") && !stderr.contains("fatal") {
            warn!("PR label warning: {}", stderr.trim());
        } else {
            anyhow::bail!("gh pr create failed: {}", stderr);
        }
    }

    let pr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Created PR: {}", pr_url);
    Ok(pr_url)
}

fn create_mr_gitlab(
    project_path: &Path,
    title: &str,
    body: &str,
    base_branch: &str,
    labels: &[&str],
) -> Result<String> {
    let mut args = vec![
        "mr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        title.to_string(),
        "--description".to_string(),
        body.to_string(),
        "--target-branch".to_string(),
        base_branch.to_string(),
        "--yes".to_string(), // non-interactive
    ];

    if !labels.is_empty() {
        args.push("--label".to_string());
        args.push(labels.join(","));
    }

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = Command::new("glab")
        .current_dir(project_path)
        .args(&args_ref)
        .output()
        .context("Failed to create MR (is glab CLI installed and authenticated?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("label") && !stderr.contains("fatal") {
            warn!("MR label warning: {}", stderr.trim());
        } else {
            anyhow::bail!("glab mr create failed: {}", stderr);
        }
    }

    // glab outputs the MR URL on stdout
    let mr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Created MR: {}", mr_url);
    Ok(mr_url)
}

/// Revert all uncommitted changes in the working directory
pub fn revert_changes(project_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["checkout", "."])
        .status()
        .context("Failed to revert changes")?;

    // Also clean untracked files that might have been created (new test files)
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["clean", "-fd"])
        .status();

    if !status.success() {
        anyhow::bail!("Failed to revert changes");
    }
    Ok(())
}

/// Delete a local branch (best-effort, ignores errors)
pub fn delete_branch(project_path: &Path, branch: &str) {
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["branch", "-D", branch])
        .output();
}

/// Check if a branch exists locally
pub fn branch_exists(project_path: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(project_path)
        .args(["rev-parse", "--verify", branch])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get list of changed files (staged + unstaged)
pub fn changed_files(project_path: &Path) -> Result<Vec<String>> {
    // Use --relative so paths are relative to project_path, not the repo root.
    // This is critical when project_path is a subdirectory of the git repo.
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["diff", "--name-only", "--relative", "HEAD"])
        .output()
        .context("Failed to get changed files")?;

    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    // Also include untracked files (--relative is implicit with current_dir for ls-files)
    let output2 = Command::new("git")
        .current_dir(project_path)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .context("Failed to get untracked files")?;

    let mut all_files = files;
    all_files.extend(
        String::from_utf8_lossy(&output2.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from),
    );

    Ok(all_files)
}

/// Check if the working tree has any changes (staged, unstaged, or untracked).
pub fn has_changes(project_path: &Path) -> Result<bool> {
    let files = changed_files(project_path)?;
    Ok(!files.is_empty())
}

/// Stage all changes and create a commit.
pub fn commit_all(project_path: &Path, message: &str) -> Result<()> {
    add_all(project_path)?;
    commit(project_path, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a temp dir with an initialized git repo.
    fn git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["init"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["commit", "--allow-empty", "-m", "init"])
            .output()
            .unwrap();
        tmp
    }

    #[test]
    fn test_current_branch() {
        let tmp = git_repo();
        let branch = current_branch(tmp.path()).unwrap();
        // Default branch is usually "main" or "master"
        assert!(!branch.is_empty());
    }

    #[test]
    fn test_create_and_checkout_branch() {
        let tmp = git_repo();
        let base = current_branch(tmp.path()).unwrap();
        create_branch(tmp.path(), "fix/test-branch", &base).unwrap();
        assert_eq!(current_branch(tmp.path()).unwrap(), "fix/test-branch");

        checkout(tmp.path(), &base).unwrap();
        assert_eq!(current_branch(tmp.path()).unwrap(), base);
    }

    #[test]
    fn test_branch_exists() {
        let tmp = git_repo();
        let base = current_branch(tmp.path()).unwrap();
        assert!(branch_exists(tmp.path(), &base));
        assert!(!branch_exists(tmp.path(), "nonexistent-branch"));
    }

    #[test]
    fn test_commit_and_staged_changes() {
        let tmp = git_repo();
        fs::write(tmp.path().join("file.txt"), "hello").unwrap();

        // No staged changes yet
        assert!(!has_staged_changes(tmp.path()).unwrap());

        add_all(tmp.path()).unwrap();
        assert!(has_staged_changes(tmp.path()).unwrap());

        commit(tmp.path(), "add file").unwrap();
        assert!(!has_staged_changes(tmp.path()).unwrap());
    }

    #[test]
    fn test_changed_files() {
        let tmp = git_repo();
        fs::write(tmp.path().join("new.txt"), "content").unwrap();

        let files = changed_files(tmp.path()).unwrap();
        assert!(files.contains(&"new.txt".to_string()));
    }

    #[test]
    fn test_revert_changes() {
        let tmp = git_repo();
        // Create and commit a file
        fs::write(tmp.path().join("file.txt"), "original").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add file").unwrap();

        // Modify it
        fs::write(tmp.path().join("file.txt"), "modified").unwrap();
        assert!(!changed_files(tmp.path()).unwrap().is_empty());

        // Revert
        revert_changes(tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "original");
    }

    #[test]
    fn test_add_specific_files() {
        let tmp = git_repo();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::write(tmp.path().join("b.txt"), "b").unwrap();

        add_files(tmp.path(), &["a.txt"]).unwrap();
        assert!(has_staged_changes(tmp.path()).unwrap());

        // b.txt should still be untracked
        let output = Command::new("git")
            .current_dir(tmp.path())
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        let staged = String::from_utf8_lossy(&output.stdout);
        assert!(staged.contains("a.txt"));
        assert!(!staged.contains("b.txt"));
    }

    #[test]
    fn test_detect_platform_defaults_to_github_without_remote() {
        let tmp = git_repo();
        // No remote set — should default to GitHub
        assert_eq!(detect_platform(tmp.path()), GitPlatform::GitHub);
    }

    #[test]
    fn test_detect_platform_github() {
        let tmp = git_repo();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["remote", "add", "origin", "https://github.com/org/repo.git"])
            .output()
            .unwrap();
        assert_eq!(detect_platform(tmp.path()), GitPlatform::GitHub);
    }

    #[test]
    fn test_detect_platform_gitlab_https() {
        let tmp = git_repo();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["remote", "add", "origin", "https://gitlab.com/org/repo.git"])
            .output()
            .unwrap();
        assert_eq!(detect_platform(tmp.path()), GitPlatform::GitLab);
    }

    #[test]
    fn test_detect_platform_gitlab_ssh() {
        let tmp = git_repo();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["remote", "add", "origin", "git@gitlab.com:org/repo.git"])
            .output()
            .unwrap();
        assert_eq!(detect_platform(tmp.path()), GitPlatform::GitLab);
    }

    #[test]
    fn test_detect_platform_self_hosted_gitlab() {
        let tmp = git_repo();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["remote", "add", "origin", "https://gitlab.mycompany.com/org/repo.git"])
            .output()
            .unwrap();
        assert_eq!(detect_platform(tmp.path()), GitPlatform::GitLab);
    }
}
