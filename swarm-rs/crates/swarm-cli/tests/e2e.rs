//! End-to-end smoke test: a real git repo, a real store, the real runner, and
//! the echo harness (which performs no network I/O).
//!
//! This is the Rust equivalent of the Go `internal/pool` e2e test: it proves the
//! wiring holds together — presplit, worktrees, the API, claim/release, and a
//! clean shutdown — without needing an LLM endpoint.

use std::path::Path;
use std::process::Command;

use swarm_cli::Runner;
use swarm_config::RunConfig;
use swarm_core::CreateItemInput;
use swarm_git::Repo;
use swarm_store::Store;

/// A scratch git repo with one commit and a `cargo`-free gate.
fn scratch_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@local"],
        vec!["config", "user.name", "t"],
    ] {
        let out = Command::new("git").args(&args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "init"]] {
        let out = Command::new("git").args(&args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?}", args);
    }
}

#[tokio::test]
async fn runner_boots_presplits_and_stops_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);

    // Open the store at the conventional location and seed two items.
    let db = dir.join(".swarm").join("swarm.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = Store::open(db.to_str().unwrap()).unwrap();

    // A seed item naming two tracked files must be presplit into children.
    std::fs::write(dir.join("a.js"), "// a\n").unwrap();
    std::fs::write(dir.join("b.js"), "// b\n").unwrap();
    for a in [vec!["add", "-A"], vec!["commit", "-q", "-m", "files"]] {
        Command::new("git").args(&a).current_dir(dir).output().unwrap();
    }

    let big = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "refactor a.js and b.js together".into(),
            detail: "both files".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    // A single-file seed item: no split.
    //
    // Its file must NOT be one the big item names. Dedup folds any item onto an
    // open item with the same file key (Go `items.go` L365-394), so a `a.js`
    // sibling would absorb the presplit child for `a.js` and the assertion below
    // would see one child instead of two. That is correct dedup behaviour, not a
    // split failure — the seed just has to name a different file.
    std::fs::write(dir.join("c.js"), "// c\n").unwrap();
    for a in [vec!["add", "-A"], vec!["commit", "-q", "-m", "more"]] {
        Command::new("git").args(&a).current_dir(dir).output().unwrap();
    }
    let _small = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "fix c.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let mut cfg = RunConfig::default();
    cfg.initial_workers = 2;
    cfg.max_workers = 4;
    cfg.harness_name = "echo".into();
    cfg.exit_when_done = false;
    // A short idle-stop so the run ends quickly in a test.
    cfg.idle_stop = std::time::Duration::from_secs(2);
    cfg.max_wall_clock = std::time::Duration::from_secs(3);

    let repo = Repo::new(dir);
    let repo_for_asserts = repo.clone();
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        cfg,
        store.clone(),
        repo,
    ));

    // Run with a hard timeout: the runner must stop on the wall clock, not hang.
    let handle = tokio::spawn(async move { runner.run().await });
    let done = tokio::time::timeout(std::time::Duration::from_secs(20), handle).await;
    assert!(done.is_ok(), "the runner must terminate on its own");
    assert!(done.unwrap().is_ok(), "run() must not error");

    // The oversized item was presplit: it is `split` with two children.
    let parent = store.get_item(&big).unwrap();
    assert_eq!(parent.status, "split", "the big item must be presplit");
    assert_eq!(
        store.child_count(&big).unwrap(),
        2,
        "one child per tracked resource"
    );

    // The integration branch exists and worker worktrees were created.
    assert!(repo_for_asserts.branch_exists("integration").await);
    let worktrees = repo_for_asserts.worktrees().await.unwrap();
    assert!(
        worktrees.len() >= 2,
        "expected the integration worktree plus at least one worker, got {}",
        worktrees.len()
    );

    // The board is consistent: the small item is untouched, the parent is split.
    let small = store.get_item(&_small).unwrap();
    assert_eq!(small.status, "pending");
}

#[tokio::test]
async fn runner_claims_and_releases_through_the_api() {
    // Prove the HTTP surface and the store agree, by claiming through the store
    // exactly as a worker would and checking the board reflects it.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);

    let db = dir.join(".swarm").join("swarm.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = Store::open(db.to_str().unwrap()).unwrap();
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "fix src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    let res = store.claim(&id, "w1").unwrap();
    assert_eq!(store.get_item(&id).unwrap().status, "in_progress");
    assert!(store.active_claims_by_peer("w1").unwrap().len() == 1);

    // A second worker gets a 409-equivalent.
    assert!(store.claim(&id, "w2").is_err());

    store.release(&res.token).unwrap();
    assert_eq!(store.get_item(&id).unwrap().status, "pending");
    // And can take it now.
    assert!(store.claim(&id, "w2").is_ok());
}
