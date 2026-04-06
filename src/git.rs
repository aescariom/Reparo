use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
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

/// Create a new branch in a detached worktree from a base ref.
///
/// Worktrees start at detached HEAD. This resets to the base ref and creates
/// a new branch, so the worktree is clean and based on the correct commit.
pub fn create_branch_in_worktree(
    worktree_path: &Path,
    branch_name: &str,
    base_ref: &str,
) -> Result<()> {
    // Reset to the base ref so the worktree starts from the right commit
    let status = Command::new("git")
        .current_dir(worktree_path)
        .args(["checkout", base_ref, "--"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("Failed to checkout base ref in worktree")?;
    if !status.success() {
        anyhow::bail!(
            "Failed to checkout {} in worktree {}",
            base_ref,
            worktree_path.display()
        );
    }

    // Delete branch if it already exists (idempotency)
    let _ = Command::new("git")
        .current_dir(worktree_path)
        .args(["branch", "-D", branch_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    let status = Command::new("git")
        .current_dir(worktree_path)
        .args(["checkout", "-b", branch_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("Failed to create branch in worktree")?;
    if !status.success() {
        anyhow::bail!(
            "Failed to create branch {} in worktree {}",
            branch_name,
            worktree_path.display()
        );
    }

    info!(
        "Created branch {} in worktree {}",
        branch_name,
        worktree_path.display()
    );
    Ok(())
}

/// Clean a worktree back to detached HEAD for reuse.
///
/// Reverts all changes, removes untracked files, and detaches HEAD.
pub fn clean_worktree(worktree_path: &Path) -> Result<()> {
    // Revert tracked changes
    let _ = Command::new("git")
        .current_dir(worktree_path)
        .args(["checkout", "--", "."])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();

    // Remove untracked files
    let _ = Command::new("git")
        .current_dir(worktree_path)
        .args(["clean", "-fd"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();

    // Detach HEAD
    let _ = Command::new("git")
        .current_dir(worktree_path)
        .args(["checkout", "--detach"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();

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

/// Create a commit bypassing pre-commit hooks (`--no-verify`).
///
/// Used for temporary WIP commits that will be squashed later, so they
/// don't need to satisfy Conventional Commits or other hook validations.
pub fn commit_no_verify(project_path: &Path, message: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["commit", "--no-verify", "-m", message])
        .status()
        .context("Failed to create commit (--no-verify)")?;

    if !status.success() {
        anyhow::bail!("git commit --no-verify failed");
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

/// Unstage all staged files (git reset), keeping working tree changes.
pub fn reset_index(project_path: &Path) -> Result<()> {
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["reset"])
        .output();
    Ok(())
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

/// Ensure the working tree is completely clean: revert tracked changes,
/// remove untracked files, and unstage everything. Returns Ok(()) even
/// if the tree was already clean.
pub fn ensure_clean_state(project_path: &Path) -> Result<()> {
    // Reset index (unstage)
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["reset", "HEAD", "--", "."])
        .status();
    // Revert tracked changes (best-effort — may be nothing to revert)
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["checkout", "."])
        .status();
    // Clean untracked files
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["clean", "-fd"])
        .status();
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

/// Fetch a branch from origin.
pub fn fetch_branch(project_path: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["fetch", "origin", branch])
        .output()
        .context("Failed to fetch from origin")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git fetch origin {} failed: {}", branch, stderr);
    }
    info!("Fetched origin/{}", branch);
    Ok(())
}

/// Rebase the current branch onto `origin/<base>`.
///
/// Returns `Ok(true)` if the rebase completed cleanly, `Ok(false)` if there
/// are conflicts that need resolution.
pub fn rebase_onto(project_path: &Path, base: &str) -> Result<bool> {
    let target = format!("origin/{}", base);
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["rebase", &target])
        .output()
        .context("Failed to start rebase")?;

    if output.status.success() {
        info!("Rebase onto {} completed cleanly", target);
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Git exits with status 1 and mentions "CONFLICT" or "could not apply" on conflicts
    if stderr.contains("CONFLICT") || stderr.contains("could not apply") || stderr.contains("Merge conflict") {
        warn!("Rebase onto {} has conflicts", target);
        return Ok(false);
    }

    anyhow::bail!("git rebase {} failed: {}", target, stderr);
}

/// List files with unresolved merge conflicts.
pub fn conflict_files(project_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .context("Failed to list conflict files")?;

    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Ok(files)
}

/// Abort an in-progress rebase.
pub fn abort_rebase(project_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["rebase", "--abort"])
        .output()
        .context("Failed to abort rebase")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rebase --abort failed: {}", stderr);
    }
    info!("Rebase aborted");
    Ok(())
}

/// Stage all changes and continue an in-progress rebase.
///
/// Returns `Ok(true)` if the rebase is now complete, `Ok(false)` if there
/// are more conflicts on subsequent commits.
pub fn mark_resolved_and_continue(project_path: &Path) -> Result<bool> {
    // Stage resolved files
    add_all(project_path)?;

    let output = Command::new("git")
        .current_dir(project_path)
        .args(["rebase", "--continue"])
        .env("GIT_EDITOR", "true") // skip editor for commit messages
        .output()
        .context("Failed to continue rebase")?;

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("CONFLICT") || stderr.contains("could not apply") || stderr.contains("Merge conflict") {
        return Ok(false);
    }

    anyhow::bail!("git rebase --continue failed: {}", stderr);
}

/// Stash specific files with a descriptive message.
///
/// Uses `git stash push -m <message> -- <files>` to save only the specified
/// files to the stash, leaving other changes in the working tree.
pub fn stash_push(project_path: &Path, message: &str, files: &[&str]) -> Result<()> {
    let mut args = vec!["stash", "push", "-m", message, "--"];
    args.extend(files);

    let output = Command::new("git")
        .current_dir(project_path)
        .args(&args)
        .output()
        .context("Failed to stash files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git stash push failed: {}", stderr);
    }
    Ok(())
}

/// Pop the most recent stash entry.
pub fn stash_pop(project_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["stash", "pop"])
        .output()
        .context("Failed to pop stash")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git stash pop failed: {}", stderr);
    }
    Ok(())
}

/// Apply and drop all stash entries whose message starts with the given prefix.
///
/// Uses `apply` + `drop` instead of `pop` so that stashes are preserved on
/// failure. Stages applied changes between iterations to keep the working tree
/// clean for the next apply. Returns the number of stashes restored.
pub fn stash_pop_matching(project_path: &Path, prefix: &str) -> Result<u32> {
    let mut popped = 0u32;
    loop {
        let indices = stash_indices_matching(project_path, prefix)?;
        if indices.is_empty() {
            break;
        }
        let idx = indices[0];
        let stash_ref = format!("stash@{{{}}}", idx);

        // Apply (does not remove the stash entry)
        let output = Command::new("git")
            .current_dir(project_path)
            .args(["stash", "apply", &stash_ref])
            .output()
            .context("Failed to apply stash by index")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Check if the failure is due to merge conflicts (e.g. add/add on
            // test files generated by different waves).  In that case we can
            // resolve automatically by accepting the incoming (stash) version.
            if stdout.contains("CONFLICT") {
                warn!(
                    "git stash apply {} produced conflicts — resolving automatically by accepting incoming changes",
                    stash_ref
                );
                // Collect unmerged (conflicted) file paths
                let ls_output = Command::new("git")
                    .current_dir(project_path)
                    .args(["diff", "--name-only", "--diff-filter=U"])
                    .output()
                    .context("Failed to list unmerged files")?;
                let unmerged: Vec<&str> = std::str::from_utf8(&ls_output.stdout)
                    .unwrap_or("")
                    .lines()
                    .filter(|l| !l.is_empty())
                    .collect();
                if !unmerged.is_empty() {
                    // Accept incoming (stash) version for each conflicted file
                    let mut args = vec!["checkout", "--theirs", "--"];
                    args.extend(unmerged.iter());
                    let _ = Command::new("git")
                        .current_dir(project_path)
                        .args(&args)
                        .output();
                    // Mark conflicts as resolved
                    let mut add_args = vec!["add", "--"];
                    add_args.extend(unmerged.iter());
                    let _ = Command::new("git")
                        .current_dir(project_path)
                        .args(&add_args)
                        .output();
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "git stash apply {} failed:\nstderr: {}\nstdout: {}",
                    stash_ref, stderr, stdout
                );
            }
        }

        // Drop the stash entry now that apply succeeded
        let _ = Command::new("git")
            .current_dir(project_path)
            .args(["stash", "drop", &stash_ref])
            .output();

        popped += 1;

        // Stage applied changes so subsequent applies don't conflict
        // with overlapping files (e.g., shared test utilities).
        let _ = Command::new("git")
            .current_dir(project_path)
            .args(["add", "-A"])
            .output();
    }
    Ok(popped)
}

/// Drop all stash entries whose message starts with the given prefix.
///
/// Best-effort cleanup — logs warnings but does not fail.
pub fn stash_drop_matching(project_path: &Path, prefix: &str) -> Result<()> {
    loop {
        let indices = stash_indices_matching(project_path, prefix)?;
        if indices.is_empty() {
            break;
        }
        // Drop from highest index first to avoid shifting
        let idx = indices.last().unwrap();
        let output = Command::new("git")
            .current_dir(project_path)
            .args(["stash", "drop", &format!("stash@{{{}}}", idx)])
            .output();
        match output {
            Ok(o) if o.status.success() => {}
            _ => {
                warn!("Failed to drop stash@{{{}}}", idx);
                break;
            }
        }
    }
    Ok(())
}

/// Find stash indices whose message contains the given prefix.
pub fn stash_indices_matching(project_path: &Path, prefix: &str) -> Result<Vec<usize>> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["stash", "list"])
        .output()
        .context("Failed to list stashes")?;

    let list = String::from_utf8_lossy(&output.stdout);
    let mut indices = Vec::new();

    for line in list.lines() {
        // Format: stash@{0}: On branch: message
        if line.contains(prefix) {
            if let Some(idx_str) = line.strip_prefix("stash@{") {
                if let Some(idx_end) = idx_str.find('}') {
                    if let Ok(idx) = idx_str[..idx_end].parse::<usize>() {
                        indices.push(idx);
                    }
                }
            }
        }
    }

    Ok(indices)
}

/// Create a detached git worktree at `worktree_path`, based on the current HEAD.
///
/// The worktree shares `.git/objects` with the main repo, so creation is fast
/// and storage is lightweight. The detached HEAD avoids branch name conflicts.
pub fn worktree_add(project_path: &Path, worktree_path: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_path)
        .args(["worktree", "add", "--detach"])
        .arg(worktree_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("Failed to run git worktree add")?;
    if !status.success() {
        anyhow::bail!(
            "git worktree add failed for {}",
            worktree_path.display()
        );
    }
    Ok(())
}

/// Remove a git worktree, forcing removal even if it has uncommitted changes.
pub fn worktree_remove(project_path: &Path, worktree_path: &Path) -> Result<()> {
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();
    // Best-effort: also remove the directory if git didn't clean it
    let _ = std::fs::remove_dir_all(worktree_path);
    Ok(())
}

/// Prune stale worktree entries from `.git/worktrees`.
///
/// Called at startup to clean up leftovers from a previous crash.
/// Return the git toplevel directory for a given path.
///
/// Runs `git rev-parse --show-toplevel` and returns the canonical path.
pub fn git_toplevel(project_path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(project_path)
        .args(["rev-parse", "--show-toplevel"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("Failed to run git rev-parse --show-toplevel")?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse --show-toplevel failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(toplevel))
}

/// Compute the subdirectory offset from the git root to `project_path`.
///
/// When `project_path` is a subdirectory of the git root (e.g. a monorepo),
/// git worktrees check out from the root. To locate project files inside a
/// worktree you need: `worktree_root.join(subdir_in_worktree(project_path))`.
///
/// Returns an empty `PathBuf` if `project_path` IS the git root.
pub fn subdir_in_worktree(project_path: &Path) -> Result<PathBuf> {
    let toplevel = git_toplevel(project_path)?;
    let canon_project = std::fs::canonicalize(project_path)
        .unwrap_or_else(|_| project_path.to_path_buf());
    let canon_toplevel = std::fs::canonicalize(&toplevel)
        .unwrap_or_else(|_| toplevel.clone());
    match canon_project.strip_prefix(&canon_toplevel) {
        Ok(rel) => Ok(rel.to_path_buf()),
        Err(_) => Ok(PathBuf::new()), // shouldn't happen; treat as root
    }
}

pub fn worktree_prune(project_path: &Path) -> Result<()> {
    let _ = Command::new("git")
        .current_dir(project_path)
        .args(["worktree", "prune"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();
    Ok(())
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
    fn test_commit_no_verify() {
        let tmp = git_repo();
        fs::write(tmp.path().join("file.txt"), "hello").unwrap();
        add_all(tmp.path()).unwrap();

        commit_no_verify(tmp.path(), "wip: temp commit").unwrap();
        assert!(!has_staged_changes(tmp.path()).unwrap());

        // Verify the commit message is present in the log
        let output = Command::new("git")
            .current_dir(tmp.path())
            .args(["log", "--oneline", "-1"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(log.contains("wip: temp commit"));
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

    #[test]
    fn test_stash_push_and_pop() {
        let tmp = git_repo();
        // Create and commit a file first (stash requires a tracked file or untracked)
        fs::write(tmp.path().join("tracked.txt"), "original").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add tracked").unwrap();

        // Modify it
        fs::write(tmp.path().join("tracked.txt"), "modified").unwrap();

        // Stash the file
        stash_push(tmp.path(), "test-stash", &["tracked.txt"]).unwrap();

        // File should be reverted
        let content = fs::read_to_string(tmp.path().join("tracked.txt")).unwrap();
        assert_eq!(content, "original");

        // Pop should restore it
        stash_pop(tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("tracked.txt")).unwrap();
        assert_eq!(content, "modified");
    }

    #[test]
    fn test_stash_pop_matching() {
        let tmp = git_repo();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add a").unwrap();

        fs::write(tmp.path().join("b.txt"), "b").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add b").unwrap();

        // Create two stashes with matching prefix
        fs::write(tmp.path().join("a.txt"), "a-modified").unwrap();
        stash_push(tmp.path(), "reparo-boost-a", &["a.txt"]).unwrap();

        fs::write(tmp.path().join("b.txt"), "b-modified").unwrap();
        stash_push(tmp.path(), "reparo-boost-b", &["b.txt"]).unwrap();

        // Pop all matching "reparo-boost"
        let count = stash_pop_matching(tmp.path(), "reparo-boost").unwrap();
        assert_eq!(count, 2);

        // Both files should be restored
        assert_eq!(fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "a-modified");
        assert_eq!(fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "b-modified");
    }

    #[test]
    fn test_stash_drop_matching() {
        let tmp = git_repo();
        fs::write(tmp.path().join("file.txt"), "content").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add file").unwrap();

        // Create a stash
        fs::write(tmp.path().join("file.txt"), "modified").unwrap();
        stash_push(tmp.path(), "reparo-boost-test", &["file.txt"]).unwrap();

        // Verify stash exists
        let indices = stash_indices_matching(tmp.path(), "reparo-boost").unwrap();
        assert_eq!(indices.len(), 1);

        // Drop it
        stash_drop_matching(tmp.path(), "reparo-boost").unwrap();

        // Should be gone
        let indices = stash_indices_matching(tmp.path(), "reparo-boost").unwrap();
        assert!(indices.is_empty());
    }

    #[test]
    fn test_stash_pop_matching_no_matches() {
        let tmp = git_repo();
        let count = stash_pop_matching(tmp.path(), "nonexistent-prefix").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_stash_indices_matching() {
        let tmp = git_repo();
        fs::write(tmp.path().join("f.txt"), "v").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "init file").unwrap();

        // Create stashes with different messages
        fs::write(tmp.path().join("f.txt"), "v1").unwrap();
        stash_push(tmp.path(), "reparo-boost-1", &["f.txt"]).unwrap();

        fs::write(tmp.path().join("f.txt"), "v2").unwrap();
        stash_push(tmp.path(), "other-stash", &["f.txt"]).unwrap();

        // Only "reparo-boost" should match
        let indices = stash_indices_matching(tmp.path(), "reparo-boost").unwrap();
        assert_eq!(indices.len(), 1);

        // "other-stash" should match separately
        let indices = stash_indices_matching(tmp.path(), "other-stash").unwrap();
        assert_eq!(indices.len(), 1);

        // Clean up
        stash_drop_matching(tmp.path(), "reparo-boost").unwrap();
        stash_drop_matching(tmp.path(), "other-stash").unwrap();
    }

    #[test]
    fn test_rebase_onto_clean() {
        let tmp = git_repo();
        let base = current_branch(tmp.path()).unwrap();

        // Add a commit on the base branch
        fs::write(tmp.path().join("base.txt"), "base content").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "base commit").unwrap();

        // Create a fix branch from before that commit
        create_branch(tmp.path(), "fix/test", &base).unwrap();
        fs::write(tmp.path().join("fix.txt"), "fix content").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "fix commit").unwrap();

        // Simulate origin by adding a local ref (rebase_onto uses origin/<base>)
        // For testing without a remote, we update the ref manually
        Command::new("git")
            .current_dir(tmp.path())
            .args(["update-ref", &format!("refs/remotes/origin/{}", base),
                   &base])
            .output()
            .unwrap();

        let result = rebase_onto(tmp.path(), &base).unwrap();
        assert!(result, "Rebase should succeed cleanly (no conflicts)");
    }

    #[test]
    fn test_rebase_onto_with_conflict() {
        let tmp = git_repo();
        let base = current_branch(tmp.path()).unwrap();

        // Create a shared file
        fs::write(tmp.path().join("shared.txt"), "original").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add shared").unwrap();

        // Create fix branch and modify shared.txt
        create_branch(tmp.path(), "fix/conflict", &base).unwrap();
        fs::write(tmp.path().join("shared.txt"), "fix version").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "fix shared").unwrap();

        // Go back to base and create a divergent commit
        checkout(tmp.path(), &base).unwrap();
        fs::write(tmp.path().join("shared.txt"), "base version").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "update shared on base").unwrap();

        // Point origin/<base> to the updated base
        let rev = Command::new("git")
            .current_dir(tmp.path())
            .args(["rev-parse", &base])
            .output()
            .unwrap();
        let rev_str = String::from_utf8_lossy(&rev.stdout).trim().to_string();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["update-ref", &format!("refs/remotes/origin/{}", base), &rev_str])
            .output()
            .unwrap();

        // Switch to fix branch and attempt rebase
        checkout(tmp.path(), "fix/conflict").unwrap();
        let result = rebase_onto(tmp.path(), &base).unwrap();
        assert!(!result, "Rebase should detect conflicts");

        // Verify conflict files
        let conflicts = conflict_files(tmp.path()).unwrap();
        assert!(conflicts.contains(&"shared.txt".to_string()));

        // Abort should succeed
        abort_rebase(tmp.path()).unwrap();
    }

    #[test]
    fn test_conflict_files_empty_when_clean() {
        let tmp = git_repo();
        let files = conflict_files(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_mark_resolved_and_continue() {
        let tmp = git_repo();
        let base = current_branch(tmp.path()).unwrap();

        // Setup conflict scenario
        fs::write(tmp.path().join("shared.txt"), "original").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "add shared").unwrap();

        create_branch(tmp.path(), "fix/resolve", &base).unwrap();
        fs::write(tmp.path().join("shared.txt"), "fix version").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "fix shared").unwrap();

        checkout(tmp.path(), &base).unwrap();
        fs::write(tmp.path().join("shared.txt"), "base version").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "update shared on base").unwrap();

        let rev = Command::new("git")
            .current_dir(tmp.path())
            .args(["rev-parse", &base])
            .output()
            .unwrap();
        let rev_str = String::from_utf8_lossy(&rev.stdout).trim().to_string();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["update-ref", &format!("refs/remotes/origin/{}", base), &rev_str])
            .output()
            .unwrap();

        checkout(tmp.path(), "fix/resolve").unwrap();
        let has_conflict = !rebase_onto(tmp.path(), &base).unwrap();
        assert!(has_conflict);

        // Resolve by writing a merged version
        fs::write(tmp.path().join("shared.txt"), "resolved content").unwrap();
        let done = mark_resolved_and_continue(tmp.path()).unwrap();
        assert!(done, "Rebase should complete after resolving the single conflict");
    }

    #[test]
    fn test_stash_pop_matching_resolves_add_add_conflict() {
        let tmp = git_repo();

        // Create a file so the repo is non-empty (stash needs at least one commit)
        fs::write(tmp.path().join("base.txt"), "base").unwrap();
        add_all(tmp.path()).unwrap();
        commit(tmp.path(), "initial").unwrap();

        // Stash 1: adds "shared.txt" with content "version-a"
        fs::write(tmp.path().join("shared.txt"), "version-a").unwrap();
        fs::write(tmp.path().join("only-a.txt"), "a").unwrap();
        add_files(tmp.path(), &["shared.txt", "only-a.txt"]).unwrap();
        stash_push(tmp.path(), "reparo-boost:fileA", &["shared.txt", "only-a.txt"]).unwrap();

        // Stash 2: adds "shared.txt" with different content (add/add conflict)
        fs::write(tmp.path().join("shared.txt"), "version-b").unwrap();
        fs::write(tmp.path().join("only-b.txt"), "b").unwrap();
        add_files(tmp.path(), &["shared.txt", "only-b.txt"]).unwrap();
        stash_push(tmp.path(), "reparo-boost:fileB", &["shared.txt", "only-b.txt"]).unwrap();

        // Pop both — should resolve the conflict automatically instead of bailing
        let count = stash_pop_matching(tmp.path(), "reparo-boost").unwrap();
        assert_eq!(count, 2);

        // Both unique files should be present
        assert!(tmp.path().join("only-a.txt").exists());
        assert!(tmp.path().join("only-b.txt").exists());

        // shared.txt should exist without conflict markers
        let shared = fs::read_to_string(tmp.path().join("shared.txt")).unwrap();
        assert!(!shared.contains("<<<<<<<"), "shared.txt should not contain conflict markers");
    }

    #[test]
    fn test_ensure_clean_state_removes_untracked() {
        let tmp = git_repo();
        // Create an untracked file
        fs::write(tmp.path().join("untracked.txt"), "garbage").unwrap();
        assert!(tmp.path().join("untracked.txt").exists());

        ensure_clean_state(tmp.path()).unwrap();
        assert!(!tmp.path().join("untracked.txt").exists());
    }

    #[test]
    fn test_ensure_clean_state_reverts_tracked() {
        let tmp = git_repo();
        // Create and commit a tracked file
        fs::write(tmp.path().join("hello.txt"), "original").unwrap();
        Command::new("git").current_dir(tmp.path()).args(["add", "hello.txt"]).output().unwrap();
        Command::new("git").current_dir(tmp.path()).args(["commit", "-m", "add hello"]).output().unwrap();
        // Modify tracked file
        fs::write(tmp.path().join("hello.txt"), "modified").unwrap();
        assert!(has_changes(tmp.path()).unwrap());

        ensure_clean_state(tmp.path()).unwrap();
        assert!(!has_changes(tmp.path()).unwrap());
    }

    #[test]
    fn test_worktree_add_and_remove() {
        let tmp = git_repo();
        // Need at least one commit for worktree to attach to HEAD
        fs::write(tmp.path().join("file.txt"), "content").unwrap();
        Command::new("git").current_dir(tmp.path()).args(["add", "."]).output().unwrap();
        Command::new("git").current_dir(tmp.path()).args(["commit", "-m", "init"]).output().unwrap();

        let wt_dir = tmp.path().join("wt-0");
        worktree_add(tmp.path(), &wt_dir).unwrap();
        assert!(wt_dir.exists());
        assert!(wt_dir.join("file.txt").exists());

        worktree_remove(tmp.path(), &wt_dir).unwrap();
        assert!(!wt_dir.exists());
    }

    #[test]
    fn test_worktree_prune_is_safe() {
        let tmp = git_repo();
        fs::write(tmp.path().join("f.txt"), "x").unwrap();
        Command::new("git").current_dir(tmp.path()).args(["add", "."]).output().unwrap();
        Command::new("git").current_dir(tmp.path()).args(["commit", "-m", "init"]).output().unwrap();
        // Prune on a repo with no stale worktrees should succeed silently
        worktree_prune(tmp.path()).unwrap();
    }

    #[test]
    fn test_worktree_changes_are_independent() {
        let tmp = git_repo();
        fs::write(tmp.path().join("src.txt"), "original").unwrap();
        Command::new("git").current_dir(tmp.path()).args(["add", "."]).output().unwrap();
        Command::new("git").current_dir(tmp.path()).args(["commit", "-m", "init"]).output().unwrap();

        let wt_dir = tmp.path().join("wt-0");
        worktree_add(tmp.path(), &wt_dir).unwrap();

        // Write a file in the worktree
        fs::write(wt_dir.join("test_new.txt"), "generated test").unwrap();
        assert!(wt_dir.join("test_new.txt").exists());
        // Main tree should NOT have the file
        assert!(!tmp.path().join("test_new.txt").exists());

        worktree_remove(tmp.path(), &wt_dir).unwrap();
    }
}
