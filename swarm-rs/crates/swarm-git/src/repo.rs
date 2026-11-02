//! The `Repo` handle: naming conventions, git invocation, and the small
//! queries the runner needs (HEAD, dirty, branch existence, commit-all).
//!
//! Naming, copied from Go `gitutil.go`:
//!
//! ```text
//! worker branch : swarm/swarm-<worker>      (BranchPrefix + "/" + prefix + "-" + worker)
//! worker worktree: <repo>/.swarm/worktrees/<worker>
//! integration   : integration
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::floor_to_char_boundary;

/// Default timeout for one git invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Characters that are unsafe in a git ref, collapsed to `-`.
static REF_UNSAFE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^A-Za-z0-9._-]+").expect("ref regex"));

/// The gate command exited non-zero.
#[derive(Debug, thiserror::Error)]
#[error("gitutil: gate {command:?} failed: {err}\n{output}")]
pub struct ErrGate {
    pub command: String,
    pub output: String,
    pub err: String,
}

/// Anything that can resolve a conflicted file into a merged one.
///
/// The runner wires this to the LLM so a merge conflict becomes a possible
/// success instead of a guaranteed failure. The gate still runs afterwards, so
/// a bad arbitration is caught exactly like a bad hand-merge.
pub type MergeResolver =
    std::sync::Arc<dyn Fn(&ConflictFile) -> Result<String, String> + Send + Sync>;

/// One conflicted file, handed to the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    /// The base/ours/theirs markers view, truncated for the prompt.
    pub view: String,
}

/// Errors from the git layer.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("git {args}: {message}\n{output}")]
    Git {
        args: String,
        message: String,
        output: String,
    },
    #[error("merge conflict")]
    Conflict,
    #[error("gate failed: {0}")]
    Gate(#[from] ErrGate),
    #[error("{0}")]
    Other(String),
}

impl RepoError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, RepoError::Conflict)
    }

    /// A stable UPPER_SNAKE code for machine dispatch.
    ///
    /// The runner used to decide what to do by searching the error's `Display`
    /// text (`reason.contains("conflict")`). That is the pattern cc-fleet's
    /// `CLAUDE.md` explicitly forbids: a reworded message silently changes
    /// behaviour. Callers switch on this instead, so the decision survives
    /// rewording and works across the process boundary.
    ///
    /// The vocabulary is deliberately small and closed:
    ///
    /// | code | meaning | caller action |
    /// |---|---|---|
    /// | `MERGE_CONFLICT` | two workers wrote the same region | tell the next worker to re-read and append |
    /// | `GATE_FAILED` | the post-merge test command failed | requeue with the test output attached |
    /// | `GIT_ERROR` | any other git invocation failure | requeue; usually transient |
    /// | `REPO_ERROR` | unclassified | requeue; do not guess |
    pub fn code(&self) -> &'static str {
        match self {
            RepoError::Conflict => "MERGE_CONFLICT",
            RepoError::Gate(_) => "GATE_FAILED",
            RepoError::Git { .. } => "GIT_ERROR",
            RepoError::Other(_) => "REPO_ERROR",
        }
    }

    /// A one-line remedy for the next worker, keyed off the code rather than the
    /// message text.
    ///
    /// Empty when there is nothing specific to say — the caller should not pad
    /// the reason with generic advice.
    pub fn remedy(&self) -> &'static str {
        match self.code() {
            "MERGE_CONFLICT" => {
                "another worker merged into the same region; re-read the file from \
                 the integration branch, then append your change instead of \
                 rewriting the whole file from an older copy"
            }
            "GATE_FAILED" => {
                "the gate command failed on the merged result; read the full test \
                 output above before retrying, and do not repeat the same change \
                 unchanged"
            }
            _ => "",
        }
    }
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub head: String,
    pub branch: String,
    pub bare: bool,
}

/// A handle on the repository plus the swarm's branch conventions.
#[derive(Clone)]
pub struct Repo {
    /// Path of the main repository (the one holding `.git`).
    pub dir: PathBuf,
    /// Where per-worker worktrees are created.
    pub worktree_root: PathBuf,
    /// Used to build worker branch names.
    pub branch_prefix: String,
    /// The branch workers get merged into.
    pub integration_branch: String,
    /// Shell command run after a merge, e.g. `cargo test`.
    pub gate_command: String,
    /// Bounds every git / gate invocation.
    pub timeout: Duration,
    /// Asked to merge conflicted files instead of rolling back.
    pub resolver: Option<MergeResolver>,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo")
            .field("dir", &self.dir)
            .field("integration", &self.integration_branch)
            .field("gate", &self.gate_command)
            .field("has_resolver", &self.resolver.is_some())
            .finish()
    }
}

impl Repo {
    /// A repo rooted at `dir` with the swarm defaults applied.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        // `canonicalize` returns a `\\?\C:\...` verbatim path on Windows, which
        // git rejects ("could not create leading directories ... Invalid
        // argument"). Strip the prefix so the path is a plain absolute path.
        let abs = std::fs::canonicalize(dir.as_ref())
            .unwrap_or_else(|_| dir.as_ref().to_path_buf());
        let abs = strip_verbatim(abs);
        let worktree_root = abs.join(".swarm").join("worktrees");
        Self {
            dir: abs,
            worktree_root,
            branch_prefix: "swarm".to_string(),
            integration_branch: "integration".to_string(),
            gate_command: String::new(),
            timeout: DEFAULT_TIMEOUT,
            resolver: None,
        }
    }

    pub fn timeout(&self) -> Duration {
        if self.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.timeout
        }
    }

    /// The integration branch name.
    pub fn integration(&self) -> String {
        if self.integration_branch.trim().is_empty() {
            "integration".to_string()
        } else {
            self.integration_branch.clone()
        }
    }

    /// The branch name for a worker: `swarm/swarm-<worker>`.
    pub fn worker_branch(&self, worker: &str) -> String {
        let prefix = if self.branch_prefix.trim().is_empty() {
            "swarm"
        } else {
            self.branch_prefix.trim()
        };
        format!("{}/{}-{}", prefix, prefix, sanitize_ref(worker))
    }

    /// The worktree path for a worker.
    pub fn worker_path(&self, worker: &str) -> PathBuf {
        let root = if self.worktree_root.as_os_str().is_empty() {
            self.dir.join(".swarm").join("worktrees")
        } else {
            self.worktree_root.clone()
        };
        root.join(sanitize_ref(worker))
    }

    /// Path where the integration worktree lives.
    pub fn integration_path(&self) -> PathBuf {
        self.dir.join(".swarm").join("integration")
    }

    // ------------------------------------------------------------ git calls

    /// Run a git command in the repository root, returning combined output.
    pub async fn git(&self, args: &[&str]) -> Result<String, RepoError> {
        self.run(&self.dir, "git", args).await
    }

    /// Run a git command inside `dir`.
    pub async fn git_in(&self, dir: &Path, args: &[&str]) -> Result<String, RepoError> {
        self.run(dir, "git", args).await
    }

    /// Run an arbitrary command, returning combined stdout+stderr.
    async fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<String, RepoError> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        cmd.current_dir(dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let fut = cmd.output();
        let out = match tokio::time::timeout(self.timeout(), fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(RepoError::Git {
                    args: args.join(" "),
                    message: e.to_string(),
                    output: String::new(),
                })
            }
            Err(_) => {
                return Err(RepoError::Git {
                    args: args.join(" "),
                    message: "timed out".to_string(),
                    output: String::new(),
                })
            }
        };

        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));

        if !out.status.success() {
            return Err(RepoError::Git {
                args: format!("{} {}", program, args.join(" ")),
                message: out
                    .status
                    .code()
                    .map(|c| format!("exit {}", c))
                    .unwrap_or_else(|| "signal".to_string()),
                output: text.trim().to_string(),
            });
        }
        Ok(text)
    }

    /// Whether `dir` looks like a git repository.
    pub fn is_repo(dir: &Path) -> bool {
        dir.join(".git").exists()
    }

    /// The branch checked out in the repository root.
    pub async fn current_branch(&self) -> Result<String, RepoError> {
        let out = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        Ok(out.trim().to_string())
    }

    /// The commit SHA of HEAD in `dir` (repo root when empty).
    pub async fn head(&self, dir: Option<&Path>) -> Result<String, RepoError> {
        let d = dir.unwrap_or(&self.dir);
        let out = self.git_in(d, &["rev-parse", "HEAD"]).await?;
        Ok(out.trim().to_string())
    }

    /// Resolve a revision to a SHA.
    pub async fn rev_parse(&self, rev: &str) -> Result<String, RepoError> {
        let out = self
            .git(&["rev-parse", "--verify", "--quiet", rev])
            .await?;
        Ok(out.trim().to_string())
    }

    /// Whether `refs/heads/<branch>` exists.
    pub async fn branch_exists(&self, branch: &str) -> bool {
        self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch)])
            .await
            .is_ok()
    }

    /// Create `branch` at `base` (HEAD when empty) if it is missing.
    ///
    /// Idempotent: an existing branch is left untouched, and losing the creation
    /// race is treated as success because the outcome is what the caller wanted.
    /// The async-gate path needs this — it can race a merge that already created
    /// the branch.
    pub async fn ensure_branch(&self, branch: &str, base: &str) -> Result<(), RepoError> {
        if self.branch_exists(branch).await {
            return Ok(());
        }
        let base = if base.trim().is_empty() { "HEAD" } else { base };
        if let Err(e) = self.git(&["branch", branch, base]).await {
            if self.branch_exists(branch).await {
                return Ok(()); // lost the race
            }
            return Err(e);
        }
        Ok(())
    }

    /// Force-delete a local branch. Missing branches are ignored.
    pub async fn delete_branch(&self, branch: &str) -> Result<(), RepoError> {
        if !self.branch_exists(branch).await {
            return Ok(());
        }
        self.git(&["branch", "-D", branch]).await.map(|_| ())
    }

    /// Whether `dir` has uncommitted changes.
    pub async fn is_dirty(&self, dir: Option<&Path>) -> Result<bool, RepoError> {
        let d = dir.unwrap_or(&self.dir);
        let out = self.git_in(d, &["status", "--porcelain"]).await?;
        Ok(!out.trim().is_empty())
    }

    /// Stage everything and commit. Returns `(sha, committed)`.
    ///
    /// When there is nothing to commit it returns the current HEAD with
    /// `committed == false`, matching Go `CommitAll`.
    pub async fn commit_all(
        &self,
        dir: Option<&Path>,
        message: &str,
    ) -> Result<(String, bool), RepoError> {
        let d = dir.unwrap_or(&self.dir);
        self.git_in(d, &["add", "-A"]).await?;
        let status = self.git_in(d, &["status", "--porcelain"]).await?;
        if status.trim().is_empty() {
            return Ok((self.head(Some(d)).await?, false));
        }
        self.git_in(
            d,
            &[
                "-c",
                "user.name=swarm",
                "-c",
                "user.email=swarm@local",
                "commit",
                "-m",
                message,
            ],
        )
        .await?;
        Ok((self.head(Some(d)).await?, true))
    }

    /// Files changed between two revisions (two-dot diff).
    pub async fn changed_files(&self, from: &str, to: &str) -> Result<Vec<String>, RepoError> {
        let out = self
            .git(&["diff", "--name-only", &format!("{}..{}", from, to)])
            .await?;
        Ok(split_lines(&out))
    }

    /// Files that differ between `base` and `branch` (three-dot, i.e. from the
    /// merge base).
    pub async fn diff_against(&self, base: &str, branch: &str) -> Result<Vec<String>, RepoError> {
        let out = self
            .git(&["diff", "--name-only", &format!("{}...{}", base, branch)])
            .await?;
        Ok(split_lines(&out))
    }

    /// Run the gate command inside `dir`.
    pub async fn run_gate(&self, dir: &Path) -> Result<(), RepoError> {
        if self.gate_command.trim().is_empty() {
            return Ok(());
        }
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/c")
        } else {
            ("sh", "-c")
        };
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag).arg(&self.gate_command);
        cmd.current_dir(dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let fut = cmd.output();
        let out = match tokio::time::timeout(self.timeout(), fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(RepoError::Gate(ErrGate {
                    command: self.gate_command.clone(),
                    output: String::new(),
                    err: e.to_string(),
                }))
            }
            Err(_) => {
                return Err(RepoError::Gate(ErrGate {
                    command: self.gate_command.clone(),
                    output: String::new(),
                    err: "timed out".to_string(),
                }))
            }
        };
        if out.status.success() {
            return Ok(());
        }
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let mut text = text.trim().to_string();
        if text.len() > 2000 {
            // Byte-wise `truncate` panics inside a multi-byte char; a test
            // failure message in any non-ASCII language reaches it.
            floor_to_char_boundary(&mut text, 2000);
            text.push_str("...(truncated)");
        }
        Err(RepoError::Gate(ErrGate {
            command: self.gate_command.clone(),
            output: text,
            err: out
                .status
                .code()
                .map(|c| format!("exit {}", c))
                .unwrap_or_else(|| "signal".to_string()),
        }))
    }
}

/// Strip the Windows `\\?\` verbatim prefix that `canonicalize` adds.
///
/// Git cannot create directories under a verbatim path, so every path handed to
/// a git command must be de-verbatim'd first.
pub fn strip_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share` maps back to `\\server\share`.
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return std::path::PathBuf::from(format!(r"\\{}", unc));
        }
        return std::path::PathBuf::from(rest);
    }
    p
}

/// Sanitize a worker name into a safe git ref component.
pub fn sanitize_ref(s: &str) -> String {
    let cleaned = REF_UNSAFE.replace_all(s.trim(), "-");
    let trimmed = cleaned.trim_matches(|c: char| c == '-' || c == '.');
    if trimmed.is_empty() {
        "worker".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Split command output into non-empty trimmed lines.
pub fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Whether two paths point at the same location (case-insensitive on Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let ca = strip_verbatim(std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf()));
    let cb = strip_verbatim(std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf()));
    if cfg!(windows) {
        ca.to_string_lossy().to_lowercase() == cb.to_string_lossy().to_lowercase()
    } else {
        ca == cb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_branch_shape_matches_go() {
        let r = Repo::new(".");
        assert_eq!(r.worker_branch("w3"), "swarm/swarm-w3");
    }

    #[test]
    fn sanitize_ref_replaces_unsafe_chars() {
        assert_eq!(sanitize_ref("w1"), "w1");
        assert_eq!(sanitize_ref("a/b c"), "a-b-c");
        assert_eq!(sanitize_ref("  x  "), "x");
        assert_eq!(sanitize_ref(""), "worker");
        assert_eq!(sanitize_ref("---"), "worker");
        assert_eq!(sanitize_ref("a..b"), "a..b", "dots are legal in refs");
    }

    #[test]
    fn worker_path_is_under_swarm_worktrees() {
        let r = Repo::new(".");
        let p = r.worker_path("w1");
        assert!(p.ends_with(Path::new(".swarm").join("worktrees").join("w1")));
    }

    #[test]
    fn integration_defaults() {
        let mut r = Repo::new(".");
        assert_eq!(r.integration(), "integration");
        r.integration_branch = "  ".into();
        assert_eq!(r.integration(), "integration");
    }

    #[test]
    fn empty_branch_prefix_falls_back() {
        let mut r = Repo::new(".");
        r.branch_prefix = "".into();
        assert_eq!(r.worker_branch("w1"), "swarm/swarm-w1");
    }

    #[test]
    fn timeout_defaults_when_zero() {
        let mut r = Repo::new(".");
        r.timeout = Duration::ZERO;
        assert_eq!(r.timeout(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn split_lines_drops_blanks() {
        assert_eq!(split_lines("a\n\nb\r\n  \nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn same_path_handles_missing_paths() {
        assert!(same_path(Path::new("/nope/a"), Path::new("/nope/a")));
        assert!(!same_path(Path::new("/nope/a"), Path::new("/nope/b")));
    }
}
