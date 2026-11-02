//! End-to-end tests for gated integration.
//!
//! These exercise the three properties that make the integrator safe:
//!
//! 1. a clean merge lands on `integration` and passes the gate;
//! 2. a failing gate rolls the integration branch back to the pre-merge SHA;
//! 3. a branch carrying scratch artifacts is refused *before* any merge, so the
//!    tree is never left half-merged.

use std::path::Path;

use swarm_git::Repo;

/// Read a file with line endings normalized.
///
/// Git on Windows may check out with CRLF (`core.autocrlf`), so byte-exact
/// comparisons would fail for reasons unrelated to the merge logic.
fn read_normalized(p: &Path) -> String {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

/// A scratch repo with one commit and an `integration` branch.
async fn scratch() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let repo = Repo::new(dir);
    repo.git(&["init", "-q"]).await.unwrap();
    repo.git(&["config", "user.email", "t@local"]).await.unwrap();
    repo.git(&["config", "user.name", "t"]).await.unwrap();
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    repo.git(&["add", "-A"]).await.unwrap();
    repo.git(&["commit", "-q", "-m", "init"]).await.unwrap();
    repo.ensure_branch("integration", "HEAD").await.unwrap();
    (tmp, repo)
}

/// Write a file inside a worktree and commit it on that worktree's branch.
async fn commit_file(repo: &Repo, wt: &Path, name: &str, body: &str, msg: &str) -> String {
    std::fs::write(wt.join(name), body).unwrap();
    let (sha, committed) = repo.commit_all(Some(wt), msg).await.unwrap();
    assert!(committed, "expected a commit for {name}");
    sha
}

#[tokio::test]
async fn clean_merge_lands_on_integration() {
    let (_tmp, repo) = scratch().await;
    let wt = repo.add_worktree("w1", "integration").await.unwrap();
    let sha = commit_file(&repo, &wt, "a.txt", "hello\n", "add a").await;

    let res = repo.merge_only("swarm/swarm-w1", "integrate w1").await.unwrap();
    assert!(!res.noop, "branch has new work");
    assert_ne!(res.sha, res.from, "integration must advance");
    assert!(res.files.contains(&"a.txt".to_string()));

    // The file must exist on the integration worktree now.
    let iwt = repo.integration_worktree().await.unwrap();
    assert_eq!(read_normalized(&iwt.join("a.txt")), "hello\n");

    // And the merged commit must contain the worker's commit.
    let head = repo.head(Some(&iwt)).await.unwrap();
    let anc = repo
        .git(&["merge-base", "--is-ancestor", &sha, &head])
        .await;
    assert!(anc.is_ok(), "worker sha must be an ancestor of integration HEAD");
}

#[tokio::test]
async fn noop_merge_is_reported() {
    let (_tmp, repo) = scratch().await;
    repo.add_worktree("w1", "integration").await.unwrap();
    // No commits on the worker branch: nothing to integrate.
    let res = repo.merge_only("swarm/swarm-w1", "noop").await.unwrap();
    assert!(res.noop, "empty branch must be a no-op");
    assert_eq!(res.sha, res.from);
}

#[tokio::test]
async fn missing_branch_is_an_error() {
    let (_tmp, repo) = scratch().await;
    let err = repo.merge_only("swarm/nope", "x").await.unwrap_err();
    assert!(
        format!("{:?}", err).contains("does not exist"),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn gate_pass_returns_ok() {
    let (_tmp, mut repo) = scratch().await;
    // Gate always succeeds.
    repo.gate_command = if cfg!(windows) { "exit /b 0".into() } else { "true".into() };

    let wt = repo.add_worktree("w1", "integration").await.unwrap();
    commit_file(&repo, &wt, "a.txt", "x\n", "add a").await;

    let res = repo.integrate("swarm/swarm-w1", "integrate w1").await.unwrap();
    assert!(!res.noop);
}

/// The critical rollback property: a failing gate must leave `integration`
/// exactly where it was before the merge.
#[tokio::test]
async fn gate_fail_rolls_integration_back() {
    let (_tmp, mut repo) = scratch().await;
    repo.gate_command = if cfg!(windows) { "exit /b 1".into() } else { "exit 1".into() };

    let wt = repo.add_worktree("w1", "integration").await.unwrap();
    commit_file(&repo, &wt, "bad.txt", "x\n", "add bad").await;

    let before = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();

    // merge_only succeeds (merge itself is fine) …
    let res = repo.merge_only("swarm/swarm-w1", "integrate w1").await.unwrap();
    assert!(!res.noop);

    // … the gate fails, so we roll back explicitly, as the runner does.
    let gate = repo.gate().await;
    assert!(gate.is_err(), "gate must fail");
    repo.rollback_to(&res.from).await.unwrap();

    let after = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();
    assert_eq!(after, before, "integration must be back at the pre-merge SHA");

    // The merged file must be gone from the integration worktree.
    let iwt = repo.integration_worktree().await.unwrap();
    assert!(
        !iwt.join("bad.txt").exists(),
        "rolled-back file must not linger"
    );
}

#[tokio::test]
async fn integrate_runs_the_gate_and_propagates_failure() {
    let (_tmp, mut repo) = scratch().await;
    repo.gate_command = if cfg!(windows) { "exit /b 7".into() } else { "exit 7".into() };

    let wt = repo.add_worktree("w1", "integration").await.unwrap();
    commit_file(&repo, &wt, "a.txt", "x\n", "add a").await;

    let err = repo.integrate("swarm/swarm-w1", "integrate").await.unwrap_err();
    match err {
        swarm_git::RepoError::Gate(g) => {
            assert_eq!(g.command, repo.gate_command);
        }
        other => panic!("expected a gate error, got {other:?}"),
    }
}

/// Scratch artifacts are refused before any merge, so the tree stays clean.
#[tokio::test]
async fn scratch_artifact_is_refused_before_merging() {
    let (_tmp, repo) = scratch().await;
    let wt = repo.add_worktree("w1", "integration").await.unwrap();
    commit_file(&repo, &wt, "tmp_patch.mjs", "console.log(1)\n", "add scratch").await;

    let before = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();
    let err = repo.merge_only("swarm/swarm-w1", "integrate").await.unwrap_err();
    assert!(
        format!("{err:?}").contains("scratch artifacts"),
        "got: {err:?}"
    );

    let after = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();
    assert_eq!(after, before, "a refused merge must not move integration");
    let iwt = repo.integration_worktree().await.unwrap();
    assert!(
        !iwt.join("tmp_patch.mjs").exists(),
        "scratch file must never appear in the integration tree"
    );
}

#[tokio::test]
async fn two_workers_merge_sequentially() {
    let (_tmp, repo) = scratch().await;
    let w1 = repo.add_worktree("w1", "integration").await.unwrap();
    let w2 = repo.add_worktree("w2", "integration").await.unwrap();

    commit_file(&repo, &w1, "one.txt", "1\n", "w1 work").await;
    commit_file(&repo, &w2, "two.txt", "2\n", "w2 work").await;

    repo.merge_only("swarm/swarm-w1", "integrate w1").await.unwrap();
    repo.merge_only("swarm/swarm-w2", "integrate w2").await.unwrap();

    let iwt = repo.integration_worktree().await.unwrap();
    assert!(iwt.join("one.txt").exists());
    assert!(iwt.join("two.txt").exists());
}

/// A resolver turns a conflict into a merge instead of a rejection.
#[tokio::test]
async fn resolver_arbitrates_a_conflict() {
    let (_tmp, mut repo) = scratch().await;
    // Both workers touch the same file with different content.
    let w1 = repo.add_worktree("w1", "integration").await.unwrap();
    let w2 = repo.add_worktree("w2", "integration").await.unwrap();

    commit_file(&repo, &w1, "conflict.txt", "from w1\n", "w1").await;
    repo.merge_only("swarm/swarm-w1", "integrate w1").await.unwrap();

    // w2 branches from the original base, so its edit conflicts.
    commit_file(&repo, &w2, "conflict.txt", "from w2\n", "w2").await;

    // Resolver always returns a fixed merged body.
    repo.resolver = Some(std::sync::Arc::new(|_cf: &swarm_git::ConflictFile| {
        Ok("merged\n".to_string())
    }));

    let res = repo.merge_only("swarm/swarm-w2", "integrate w2").await.unwrap();
    assert!(res.arbitrated, "conflict must be resolved by the model");

    let iwt = repo.integration_worktree().await.unwrap();
    assert_eq!(read_normalized(&iwt.join("conflict.txt")), "merged\n");
}

/// Without a resolver, a conflict is a rejection and the tree is restored.
#[tokio::test]
async fn conflict_without_resolver_rolls_back() {
    let (_tmp, repo) = scratch().await;
    let w1 = repo.add_worktree("w1", "integration").await.unwrap();
    let w2 = repo.add_worktree("w2", "integration").await.unwrap();

    commit_file(&repo, &w1, "conflict.txt", "from w1\n", "w1").await;
    repo.merge_only("swarm/swarm-w1", "integrate w1").await.unwrap();
    let before = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();

    commit_file(&repo, &w2, "conflict.txt", "from w2\n", "w2").await;

    let err = repo.merge_only("swarm/swarm-w2", "integrate w2").await.unwrap_err();
    assert!(err.is_conflict(), "expected a conflict, got {err:?}");

    let after = repo.head(Some(&repo.integration_worktree().await.unwrap())).await.unwrap();
    assert_eq!(after, before, "conflict must restore integration");
    let iwt = repo.integration_worktree().await.unwrap();
    assert_eq!(
        read_normalized(&iwt.join("conflict.txt")),
        "from w1\n",
        "w1's version must survive"
    );
}
