//! Git worktree pool for parallel operations (coverage boost + issue fixing).
//!
//! Creates N git worktrees that share the main repo's `.git/objects`, so each
//! is a lightweight, independent checkout. Used for both parallel coverage
//! boost (US-009) and parallel issue processing (US-018).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// A pool of reusable git worktrees for concurrent test generation.
///
/// Worktrees are created on construction and removed on drop, even if the
/// owning function returns early or panics.
pub(crate) struct WorktreePool {
    main_path: PathBuf,
    /// All worktree paths that were created (for cleanup).
    all: Vec<PathBuf>,
    /// Currently available (not in use) worktree paths.
    available: std::sync::Mutex<Vec<PathBuf>>,
    /// Subdirectory offset from git root to project path.
    /// Empty when the project IS the git root.
    subdir: PathBuf,
}

impl WorktreePool {
    /// Create `parallelism` detached worktrees.
    ///
    /// Worktrees live in a `.reparo-worktrees/` directory next to the project
    /// (i.e. `<project>/../.reparo-worktrees/wt-0`). This avoids appearing as
    /// untracked files inside the project.
    pub fn new(main_path: &Path, parallelism: usize) -> Result<Self> {
        assert!(parallelism > 0, "parallelism must be >= 1");

        // Compute subdirectory offset (empty if project IS git root).
        let subdir = crate::git::subdir_in_worktree(main_path)
            .unwrap_or_default();
        if !subdir.as_os_str().is_empty() {
            info!(
                "Project is in subdirectory {:?} of git root — worktrees will use this offset",
                subdir
            );
        }

        // Prune stale entries from a previous crashed run.
        crate::git::worktree_prune(main_path)?;

        // Use the project directory name + a hash suffix to avoid collisions when
        // multiple repos (or tests) share the same parent directory.
        let dir_name = main_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            main_path.hash(&mut h);
            format!("{:016x}", h.finish())
        };
        let base = main_path
            .parent()
            .unwrap_or(main_path)
            .join(format!(".reparo-wt-{}-{}", dir_name, &hash[..8]));
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Cannot create worktree base dir {}", base.display()))?;

        let mut all = Vec::with_capacity(parallelism);
        for i in 0..parallelism {
            let wt_path = base.join(format!("wt-{}", i));

            // If a worktree from a previous run exists at this path, remove it first.
            if wt_path.exists() {
                let _ = crate::git::worktree_remove(main_path, &wt_path);
            }

            crate::git::worktree_add(main_path, &wt_path)
                .with_context(|| format!("Failed to create worktree wt-{}", i))?;
            all.push(wt_path);
        }

        info!(
            "Created {} worktrees in {} for parallel coverage boost",
            parallelism,
            base.display()
        );

        let available = all.clone();
        Ok(Self {
            main_path: main_path.to_path_buf(),
            all,
            available: std::sync::Mutex::new(available),
            subdir,
        })
    }

    /// Return the project-level directory inside a worktree.
    ///
    /// When the project is a subdirectory of the git root, worktrees check out
    /// from the root. This method appends the subdirectory offset so callers
    /// get the path equivalent to `config.path` inside the worktree.
    pub fn project_dir(&self, wt_root: &Path) -> PathBuf {
        if self.subdir.as_os_str().is_empty() {
            wt_root.to_path_buf()
        } else {
            wt_root.join(&self.subdir)
        }
    }

    /// Borrow an available worktree path. Blocks (spins) if all are in use.
    ///
    /// In practice this is called from `spawn_blocking` tasks, so spinning
    /// briefly is acceptable — the pool is sized to match concurrency.
    pub fn acquire(&self) -> PathBuf {
        loop {
            if let Ok(mut avail) = self.available.lock() {
                if let Some(path) = avail.pop() {
                    return path;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Borrow an available worktree path, yielding to the tokio runtime while waiting.
    ///
    /// Unlike `acquire()` which uses `thread::sleep` (blocking the OS thread),
    /// this version uses `tokio::time::sleep` so other tasks can progress.
    pub async fn acquire_async(&self) -> PathBuf {
        loop {
            if let Ok(mut avail) = self.available.lock() {
                if let Some(path) = avail.pop() {
                    return path;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }

    /// Return a worktree to the pool after use.
    ///
    /// Callers should revert the worktree to clean state before releasing
    /// (`git checkout -- .` / `git clean -fd`).
    pub fn release(&self, path: PathBuf) {
        if let Ok(mut avail) = self.available.lock() {
            avail.push(path);
        }
    }

    /// Remove all worktrees. Called automatically via `Drop`.
    pub fn destroy(&self) {
        for wt in &self.all {
            if let Err(e) = crate::git::worktree_remove(&self.main_path, wt) {
                warn!("Failed to remove worktree {}: {}", wt.display(), e);
            }
        }
        // Remove the base directory if empty.
        if let Some(base) = self.all.first().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir(base); // succeeds only if empty
        }
    }
}

impl Drop for WorktreePool {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Copy test files from a worktree back to the main working tree.
///
/// `file_paths` should be relative to the project root (as returned by
/// `git::changed_files`). Returns the list of successfully copied paths.
pub(crate) fn copy_worktree_results(
    worktree_path: &Path,
    main_path: &Path,
    file_paths: &[String],
) -> Result<Vec<String>> {
    let mut copied = Vec::new();
    for rel in file_paths {
        let src = worktree_path.join(rel);
        let dst = main_path.join(rel);
        if !src.exists() {
            warn!("Worktree file {} does not exist — skipping", src.display());
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create directory for {}", dst.display()))?;
        }
        std::fs::copy(&src, &dst)
            .with_context(|| format!("Failed to copy {} → {}", src.display(), dst.display()))?;
        copied.push(rel.clone());
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["init"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["config", "user.email", "test@test.com"])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        fs::write(tmp.path().join("src.txt"), "hello").unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .current_dir(tmp.path())
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        tmp
    }

    #[test]
    fn test_pool_create_and_drop() {
        let tmp = git_repo();
        let pool = WorktreePool::new(tmp.path(), 2).unwrap();
        assert_eq!(pool.all.len(), 2);
        for wt in &pool.all {
            assert!(wt.exists());
            assert!(wt.join("src.txt").exists());
        }
        let paths: Vec<PathBuf> = pool.all.clone();
        drop(pool);
        // After drop, worktrees should be cleaned up.
        for wt in &paths {
            assert!(!wt.exists());
        }
    }

    #[test]
    fn test_pool_acquire_and_release() {
        let tmp = git_repo();
        let pool = WorktreePool::new(tmp.path(), 2).unwrap();

        let p1 = pool.acquire();
        let p2 = pool.acquire();
        assert_ne!(p1, p2);

        // Both acquired — available should be empty.
        {
            let avail = pool.available.lock().unwrap();
            assert!(avail.is_empty());
        }

        pool.release(p1.clone());
        let p3 = pool.acquire();
        assert_eq!(p3, p1); // got the same one back
    }

    #[test]
    fn test_copy_worktree_results() {
        let tmp = git_repo();
        let pool = WorktreePool::new(tmp.path(), 1).unwrap();
        let wt = pool.acquire();

        // Create a test file in the worktree
        let test_dir = wt.join("tests");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(test_dir.join("test_foo.py"), "def test_foo(): pass").unwrap();

        let copied = copy_worktree_results(
            &wt,
            tmp.path(),
            &["tests/test_foo.py".to_string()],
        )
        .unwrap();

        assert_eq!(copied, vec!["tests/test_foo.py"]);
        assert!(tmp.path().join("tests/test_foo.py").exists());
        assert_eq!(
            fs::read_to_string(tmp.path().join("tests/test_foo.py")).unwrap(),
            "def test_foo(): pass"
        );
    }
}
