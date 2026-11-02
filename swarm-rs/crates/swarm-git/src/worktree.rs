//! Worktree management: one worktree per worker, checked out on its branch.
//!
//! Mirrors Go `internal/gitutil/worktree.go`. The awkward parts are all about
//! recovering from a dirty state — a previous run may have left a directory that
//! is not a registered worktree, or a registration pointing at a path that no
//! longer exists. Every operation here is written to converge rather than fail.

use std::path::{Path, PathBuf};

use crate::repo::{same_path, Repo, RepoError, Worktree};

impl Repo {
    /// List every worktree registered in the repository.
    pub async fn worktrees(&self) -> Result<Vec<Worktree>, RepoError> {
        let out = self.git(&["worktree", "list", "--porcelain"]).await?;
        let mut res = Vec::new();
        let mut cur = Worktree::default();
        let flush = |cur: &mut Worktree, res: &mut Vec<Worktree>| {
            if !cur.path.is_empty() {
                res.push(cur.clone());
            }
            *cur = Worktree::default();
        };
        for line in out.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                flush(&mut cur, &mut res);
                continue;
            }
            let (key, val) = match line.split_once(' ') {
                Some((k, v)) => (k, v),
                None => (line, ""),
            };
            match key {
                "worktree" => cur.path = val.to_string(),
                "HEAD" => cur.head = val.to_string(),
                "branch" => {
                    cur.branch = val
                        .strip_prefix("refs/heads/")
                        .unwrap_or(val)
                        .to_string()
                }
                "bare" => cur.bare = true,
                _ => {}
            }
        }
        flush(&mut cur, &mut res);
        Ok(res)
    }

    /// Create (or reuse) the worktree for `worker`, checking out its branch.
    ///
    /// The branch is created from `base` when it does not exist yet. A stale
    /// registration on the wrong branch is dropped and re-added; a directory
    /// that exists but is not a registered worktree is removed first.
    pub async fn add_worktree(&self, worker: &str, base: &str) -> Result<PathBuf, RepoError> {
        let path = self.worker_path(worker);
        let branch = self.worker_branch(worker);

        if let Ok(wts) = self.worktrees().await {
            for wt in wts {
                if !same_path(Path::new(&wt.path), &path) {
                    continue;
                }
                if wt.branch == branch {
                    return Ok(path);
                }
                // Stale registration on the wrong branch: drop and re-add.
                let p = wt.path.clone();
                let _ = self.git(&["worktree", "remove", "--force", &p]).await;
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| RepoError::Other(e.to_string()))?;
        }
        if path.exists() {
            // Directory exists but is not a registered worktree: clean it up.
            std::fs::remove_dir_all(&path).map_err(|e| RepoError::Other(e.to_string()))?;
        }

        self.ensure_branch(&branch, base).await?;
        let p = path.to_string_lossy().to_string();
        if self.git(&["worktree", "add", &p, &branch]).await.is_err() {
            self.git(&["worktree", "add", "--force", &p, &branch])
                .await?;
        }
        Ok(path)
    }

    /// Remove the worker's worktree, keeping its branch.
    pub async fn remove_worktree(&self, worker: &str) -> Result<(), RepoError> {
        let path = self.worker_path(worker);
        if !path.exists() {
            let _ = self.git(&["worktree", "prune"]).await;
            return Ok(());
        }
        let p = path.to_string_lossy().to_string();
        if self.git(&["worktree", "remove", "--force", &p]).await.is_err() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                return Err(RepoError::Other(format!(
                    "remove worktree {}: {}",
                    p, e
                )));
            }
            let _ = self.git(&["worktree", "prune"]).await;
        }
        Ok(())
    }

    /// Drop registrations for worktrees whose directory vanished.
    pub async fn prune_worktrees(&self) -> Result<(), RepoError> {
        self.git(&["worktree", "prune"]).await.map(|_| ())
    }

    /// The on-disk path of the worktree that has `branch` checked out, or empty.
    pub async fn worktree_path(&self, branch: &str) -> String {
        match self.worktrees().await {
            Ok(wts) => wts
                .into_iter()
                .find(|w| w.branch == branch)
                .map(|w| w.path)
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// The integration worktree path, creating it when missing.
    ///
    /// Concurrency-safe: "already exists" from either `git branch` or `git
    /// worktree add` is treated as success, because the outcome (the worktree
    /// exists) is what the caller wanted. The async-gate path calls this from
    /// several tasks at once.
    pub async fn integration_worktree(&self) -> Result<PathBuf, RepoError> {
        let integration = self.integration();
        let existing = self.worktree_path(&integration).await;
        if !existing.is_empty() {
            return Ok(PathBuf::from(existing));
        }

        let dir = self.integration_path();
        let _ = self.ensure_branch(&integration, "").await;
        let d = dir.to_string_lossy().to_string();
        if self.git(&["worktree", "add", &d, &integration]).await.is_err() {
            // Either another task won the race, or a previous run left it.
            let again = self.worktree_path(&integration).await;
            if !again.is_empty() {
                return Ok(PathBuf::from(again));
            }
            if self
                .git(&["worktree", "add", "--force", &d, &integration])
                .await
                .is_err()
            {
                let third = self.worktree_path(&integration).await;
                if !third.is_empty() {
                    return Ok(PathBuf::from(third));
                }
                return Err(RepoError::Other(format!(
                    "add integration worktree {}",
                    d
                )));
            }
        }
        Ok(dir)
    }

    /// The integration worktree path without creating anything.
    pub async fn integration_dir(&self) -> PathBuf {
        let p = self.worktree_path(&self.integration()).await;
        if !p.is_empty() {
            PathBuf::from(p)
        } else {
            self.integration_path()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch repo with one commit, so HEAD resolves.
    async fn scratch_repo() -> (tempfile::TempDir, Repo) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let repo = Repo::new(dir);
        repo.git(&["init", "-q"]).await.unwrap();
        repo.git(&["config", "user.email", "t@local"]).await.unwrap();
        repo.git(&["config", "user.name", "t"]).await.unwrap();
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        repo.git(&["add", "-A"]).await.unwrap();
        repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
        (tmp, repo)
    }

    #[tokio::test]
    async fn add_worktree_creates_branch_and_dir() {
        let (_tmp, repo) = scratch_repo().await;
        let path = repo.add_worktree("w1", "HEAD").await.unwrap();
        assert!(path.exists(), "worktree dir must exist");
        assert!(repo.branch_exists("swarm/swarm-w1").await);
    }

    #[tokio::test]
    async fn add_worktree_is_idempotent() {
        let (_tmp, repo) = scratch_repo().await;
        let a = repo.add_worktree("w1", "HEAD").await.unwrap();
        let b = repo.add_worktree("w1", "HEAD").await.unwrap();
        assert_eq!(a, b);
        let wts = repo.worktrees().await.unwrap();
        let mine: Vec<_> = wts.iter().filter(|w| w.branch == "swarm/swarm-w1").collect();
        assert_eq!(mine.len(), 1, "must not register twice");
    }

    #[tokio::test]
    async fn remove_worktree_keeps_branch() {
        let (_tmp, repo) = scratch_repo().await;
        let path = repo.add_worktree("w1", "HEAD").await.unwrap();
        repo.remove_worktree("w1").await.unwrap();
        assert!(!path.exists(), "worktree dir must be gone");
        assert!(
            repo.branch_exists("swarm/swarm-w1").await,
            "branch must survive"
        );
    }

    #[tokio::test]
    async fn remove_missing_worktree_is_ok() {
        let (_tmp, repo) = scratch_repo().await;
        repo.remove_worktree("ghost").await.unwrap();
    }

    #[tokio::test]
    async fn worktree_path_finds_the_checkout() {
        let (_tmp, repo) = scratch_repo().await;
        let path = repo.add_worktree("w2", "HEAD").await.unwrap();
        let found = repo.worktree_path("swarm/swarm-w2").await;
        assert_eq!(PathBuf::from(found), path);
        assert_eq!(repo.worktree_path("nope").await, "");
    }

    #[tokio::test]
    async fn integration_worktree_is_created_once() {
        let (_tmp, repo) = scratch_repo().await;
        let a = repo.integration_worktree().await.unwrap();
        let b = repo.integration_worktree().await.unwrap();
        assert_eq!(a, b);
        assert!(repo.branch_exists("integration").await);
    }

    #[tokio::test]
    async fn stale_directory_is_replaced() {
        let (_tmp, repo) = scratch_repo().await;
        // Create the worker path as a plain directory (not a worktree).
        let p = repo.worker_path("w9");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("junk.txt"), "x").unwrap();

        let path = repo.add_worktree("w9", "HEAD").await.unwrap();
        assert!(path.exists());
        assert!(
            !path.join("junk.txt").exists(),
            "stale content must be cleared"
        );
    }
}
