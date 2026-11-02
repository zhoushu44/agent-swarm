//! Gated integration: merge a worker branch into `integration`, run the gate,
//! and roll back on failure.
//!
//! Mirrors Go `internal/gitutil/integrate.go`. Three properties matter:
//!
//! 1. **Scratch artifacts are refused before merging.** The gate would reject
//!    them anyway, and leaving a half-merged tree behind is worse than refusing
//!    up front.
//! 2. **The merge is recorded with its `from` SHA**, so a gate failure can roll
//!    back to exactly that point — even when later merges already landed. Those
//!    submissions were already rejected and will be re-merged next round, so
//!    work is re-done, never lost.
//! 3. **A conflict is a possible success**, not a guaranteed failure: if a
//!    resolver is wired, the conflicted files are handed to it and the gate
//!    still runs afterwards.

use std::path::Path;

use crate::floor_to_char_boundary;

use crate::repo::{Repo, RepoError};

/// Filenames/patterns the integrator refuses outright.
///
/// A stray helper file would otherwise waste an entire submission, so the check
/// happens before any merge is attempted.
pub fn rejected_artifacts(files: &[String]) -> Vec<String> {
    let mut bad = Vec::new();
    for f in files {
        let slash = f.replace('\\', "/");
        let base = slash
            .rsplit('/')
            .next()
            .unwrap_or(&slash)
            .to_lowercase();

        if base.starts_with("probe.")
            || base.starts_with("tmp_")
            || base.starts_with("debug_")
            || base.starts_with('_')
        {
            bad.push(f.clone());
            continue;
        }

        let ext = base.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if matches!(ext, "log" | "tmp" | "bak") {
            bad.push(f.clone());
            continue;
        }

        if format!("/{}", slash).contains("/scratch/") {
            bad.push(f.clone());
        }
    }
    bad.sort();
    bad
}

/// The outcome of a merge attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeResult {
    pub branch: String,
    /// Integration HEAD *before* the merge — the rollback target.
    pub from: String,
    /// Integration HEAD after the merge.
    pub sha: String,
    /// Files the merge touched.
    pub files: Vec<String>,
    /// True when the branch brought nothing new.
    pub noop: bool,
    /// The integration tree this merge produced, i.e. the content identity.
    ///
    /// Two merges that produce the same tree are the same content even when
    /// their commit shas differ (a rebase, a re-merge, a duplicate submission).
    /// `noop` alone cannot tell "this branch added nothing" from "this
    /// submission was already integrated", and the runner used to treat both as
    /// a silent success — which is how a duplicated submission inflated the
    /// accepted count.
    pub tree_sha: String,
    /// True when a model resolved the conflict.
    pub arbitrated: bool,
}

impl Repo {
    /// Merge `branch` into the integration branch and gate it.
    ///
    /// This is the single-call form used by tests and simple callers; the runner
    /// uses `merge_only` + `gate` separately so the gate can run off the serial
    /// merge path.
    pub async fn integrate(&self, branch: &str, message: &str) -> Result<MergeResult, RepoError> {
        let res = self.merge_only(branch, message).await?;
        if res.noop {
            return Ok(res);
        }
        self.gate().await?;
        Ok(res)
    }

    /// Merge `branch` into integration **without** running the gate.
    ///
    /// On gate-less use the caller is responsible for `gate()` + `rollback_to()`;
    /// that split is what lets the runner keep merges serial and gates parallel.
    pub async fn merge_only(
        &self,
        branch: &str,
        message: &str,
    ) -> Result<MergeResult, RepoError> {
        let integration = self.integration();
        // Idempotent: concurrent callers may both see "missing" and both try to
        // create it; the second must not fail the merge.
        let _ = self.ensure_branch(&integration, "HEAD").await;

        if !self.branch_exists(branch).await {
            return Err(RepoError::Other(format!(
                "gitutil: branch {:?} does not exist",
                branch
            )));
        }

        let wt = self.integration_worktree().await?;
        let from = self.head(Some(&wt)).await?;
        let mut res = MergeResult {
            branch: branch.to_string(),
            from: from.clone(),
            ..Default::default()
        };

        // Refuse scratch artifacts before touching the tree.
        let files = self.diff_against(&from, branch).await?;
        let bad = rejected_artifacts(&files);
        if !bad.is_empty() {
            return Err(RepoError::Other(format!(
                "gitutil: branch {:?} contains scratch artifacts: {}",
                branch,
                bad.join(", ")
            )));
        }

        // Nothing new? Then there is nothing to integrate.
        if self
            .git_in(&wt, &["merge-base", "--is-ancestor", branch, "HEAD"])
            .await
            .is_ok()
        {
            res.noop = true;
            res.sha = from.clone();
            // Still record the tree: the caller compares it against the last
            // accepted tree to tell "nothing new" from "already integrated".
            res.tree_sha = self.tree_of(&wt, &from).await.unwrap_or_default();
            return Ok(res);
        }

        let merge_msg = if message.trim().is_empty() {
            format!("integrate {}", branch)
        } else {
            message.to_string()
        };

        if self
            .git_in(&wt, &["merge", "--no-ff", "--no-edit", "-m", &merge_msg, branch])
            .await
            .is_err()
        {
            // Conflict: try arbitration before giving up. A rejected arbitration
            // falls through to the same rollback, so this can only turn a
            // guaranteed failure into a possible success.
            let has_conflicts = self.has_conflicts(&wt).await;
            if self.resolver.is_some() && has_conflicts {
                if self.arbitrate(&wt, &from, branch).await.is_ok() {
                    if self.git_in(&wt, &["commit", "--no-edit"]).await.is_ok() {
                        let merged = self.head(Some(&wt)).await?;
                        res.sha = merged.clone();
                        res.arbitrated = true;
                        res.files = self.changed_files(&from, &merged).await.unwrap_or_default();
                        res.tree_sha = self.tree_of(&wt, &merged).await.unwrap_or_default();
                        if let Err(e) = self.run_gate(&wt).await {
                            self.rollback(&wt, &from).await;
                            return Err(e);
                        }
                        return Ok(res);
                    }
                }
            }
            self.rollback(&wt, &from).await;
            return Err(RepoError::Conflict);
        }

        let merged = self.head(Some(&wt)).await?;
        res.sha = merged.clone();
        res.files = self.changed_files(&from, &merged).await.unwrap_or_default();
        res.tree_sha = self.tree_of(&wt, &merged).await.unwrap_or_default();
        Ok(res)
    }

    /// The tree object a commit points at — the content identity of a merge.
    ///
    /// The commit sha changes on every merge (different parents, different
    /// timestamp), so it cannot answer "is this the same content?". The tree
    /// sha can: two merges of the same content have the same tree even when
    /// their commits differ.
    pub async fn tree_of(&self, wt: &Path, commit: &str) -> Result<String, RepoError> {
        let out = self
            .git_in(wt, &["rev-parse", &format!("{}^{{tree}}", commit)])
            .await?;
        Ok(out.trim().to_string())
    }

    /// Run the gate in the integration worktree.
    pub async fn gate(&self) -> Result<(), RepoError> {
        let wt = self.integration_worktree().await?;
        self.run_gate(&wt).await
    }

    /// Hard-reset the integration worktree back to `sha`.
    ///
    /// Used by the async-gate path when a gate fails after the merge was already
    /// committed.
    pub async fn rollback_to(&self, sha: &str) -> Result<(), RepoError> {
        let wt = self.integration_dir().await;
        self.rollback(&wt, sha).await;
        Ok(())
    }

    /// `merge --abort` + `reset --hard` + `clean -fdx`.
    ///
    /// The clean is not gratuitous: a gate failure must leave a pristine tree,
    /// otherwise the next merge inherits stray files.
    async fn rollback(&self, wt: &Path, sha: &str) {
        let _ = self.git_in(wt, &["merge", "--abort"]).await;
        let _ = self.git_in(wt, &["reset", "--hard", sha]).await;
        let _ = self.git_in(wt, &["clean", "-fdx"]).await;
    }

    /// Whether the worktree has unmerged paths.
    pub async fn has_conflicts(&self, wt: &Path) -> bool {
        self.git_in(wt, &["diff", "--name-only", "--diff-filter=U"])
            .await
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
    }

    /// Ask the resolver to merge each conflicted file, then stage the results.
    ///
    /// Returns the number of files resolved. Any resolver failure aborts the
    /// whole arbitration (the caller then rolls back).
    pub async fn arbitrate(
        &self,
        wt: &Path,
        _from: &str,
        _branch: &str,
    ) -> Result<usize, RepoError> {
        let resolver = match &self.resolver {
            Some(r) => r.clone(),
            None => return Err(RepoError::Other("no resolver configured".into())),
        };

        let conflicted = self.conflicted_files(wt).await?;
        let mut resolved = 0usize;
        for cf in conflicted {
            let merged = resolver(&cf).map_err(RepoError::Other)?;
            let abs = wt.join(&cf.path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).map_err(|e| RepoError::Other(e.to_string()))?;
            }
            std::fs::write(&abs, merged.as_bytes()).map_err(|e| RepoError::Other(e.to_string()))?;
            self.git_in(wt, &["add", "--", &cf.path]).await?;
            resolved += 1;
        }
        Ok(resolved)
    }

    /// Conflicted files with a marker view for the resolver's prompt.
    pub async fn conflicted_files(&self, wt: &Path) -> Result<Vec<crate::repo::ConflictFile>, RepoError> {
        let out = self
            .git_in(wt, &["diff", "--name-only", "--diff-filter=U"])
            .await?;
        let mut files = Vec::new();
        for path in crate::repo::split_lines(&out) {
            let abs = wt.join(&path);
            let view = std::fs::read_to_string(&abs).unwrap_or_default();
            let mut view = view;
            if view.len() > MAX_CONFLICT_FILE_BYTES {
                // Byte-wise `truncate` panics inside a multi-byte char; a source
                // file with any non-ASCII comment reaches it.
                floor_to_char_boundary(&mut view, MAX_CONFLICT_FILE_BYTES);                view.push_str("\n...(truncated)");
            }
            files.push(crate::repo::ConflictFile { path, view });
            if files.len() >= MAX_CONFLICT_FILES {
                break;
            }
        }
        Ok(files)
    }
}

/// Largest conflicted file handed to the resolver.
pub const MAX_CONFLICT_FILE_BYTES: usize = 64 * 1024;

/// Most conflicted files arbitrated in one merge.
pub const MAX_CONFLICT_FILES: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_tmp_prefix() {
        assert_eq!(rejected_artifacts(&v(&["tmp_patch.mjs"])), v(&["tmp_patch.mjs"]));
        assert_eq!(rejected_artifacts(&v(&["src/tmp_x.js"])), v(&["src/tmp_x.js"]));
    }

    #[test]
    fn rejects_probe_and_debug_and_underscore() {
        assert_eq!(rejected_artifacts(&v(&["probe.js"])), v(&["probe.js"]));
        assert_eq!(rejected_artifacts(&v(&["debug_run.js"])), v(&["debug_run.js"]));
        assert_eq!(rejected_artifacts(&v(&["_helper.js"])), v(&["_helper.js"]));
    }

    #[test]
    fn rejects_log_tmp_bak_extensions() {
        for f in ["out.log", "x.tmp", "config.bak"] {
            assert_eq!(rejected_artifacts(&v(&[f])), v(&[f]), "{f} must be rejected");
        }
    }

    #[test]
    fn rejects_anything_under_scratch() {
        assert_eq!(rejected_artifacts(&v(&["scratch/a.js"])), v(&["scratch/a.js"]));
        assert_eq!(
            rejected_artifacts(&v(&["src/scratch/deep/a.js"])),
            v(&["src/scratch/deep/a.js"])
        );
    }

    #[test]
    fn allows_real_deliverables() {
        let ok = v(&["src/server.js", "tests/a_test.go", "README.md", "Cargo.toml"]);
        assert!(rejected_artifacts(&ok).is_empty(), "got {:?}", rejected_artifacts(&ok));
    }

    #[test]
    fn windows_paths_are_handled() {
        assert_eq!(
            rejected_artifacts(&v(&[r"src\scratch\a.js"])),
            v(&[r"src\scratch\a.js"])
        );
        assert!(rejected_artifacts(&v(&[r"src\main.rs"])).is_empty());
    }

    #[test]
    fn results_are_sorted() {
        let out = rejected_artifacts(&v(&["tmp_b.js", "tmp_a.js"]));
        assert_eq!(out, v(&["tmp_a.js", "tmp_b.js"]));
    }
}
