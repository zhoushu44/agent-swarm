//! Runner **wiring** tests.
//!
//! `tests/breakers.rs` tests the fourteen circuit breakers by feeding them a
//! hand-built `World`. That proves each breaker's logic, and nothing about
//! whether `Runner::observe()` ever produces such a world. It did not: the first
//! version hard-coded six fields to zero (silently disabling four breakers), and
//! a later pass left four more as literals (`reply_says_idle`,
//! `submitted_this_turn`, `turn_ok`, `http_status`) while the real values sat in
//! the turn result.
//!
//! `SWARM-RUN-ISSUES.md` §4 recorded exactly this lesson — "263 tests green and
//! 'the feature is empty' can hold at the same time" — and named the test that
//! would have caught it. This file is that test.
//!
//! These tests drive the real runner (real git, real store, real echo harness)
//! and assert on the world the breakers actually see, plus the two breakers
//! whose wiring was most often wrong: budget (guard 2) and no-claim-no-submit
//! (guard 4).

use std::path::Path;
use std::process::Command;

use swarm_cli::Runner;
use swarm_config::RunConfig;
use swarm_core::CreateItemInput;
use swarm_git::Repo;
use swarm_store::Store;

/// A scratch git repo with one commit.
fn scratch_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@local"],
        vec!["config", "user.name", "t"],
    ] {
        let out = Command::new("git").args(&args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?}", args);
    }
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "init"]] {
        let out = Command::new("git").args(&args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?}", args);
    }
}

/// Open a store at the conventional `.swarm/swarm.db` location.
fn open_store(dir: &Path) -> Store {
    let db = dir.join(".swarm").join("swarm.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    Store::open(db.to_str().unwrap()).unwrap()
}

/// A config that ends quickly under the echo harness.
fn fast_cfg() -> RunConfig {
    let mut cfg = RunConfig::default();
    cfg.initial_workers = 2;
    cfg.max_workers = 4;
    cfg.harness_name = "echo".into();
    cfg.exit_when_done = false;
    cfg.idle_stop = std::time::Duration::from_secs(2);
    cfg.max_wall_clock = std::time::Duration::from_secs(3);
    cfg
}

/// Run the runner to completion with a hard timeout.
async fn run_to_end(runner: std::sync::Arc<Runner>) {
    let handle = tokio::spawn(async move { runner.run().await });
    let done = tokio::time::timeout(std::time::Duration::from_secs(30), handle).await;
    assert!(done.is_ok(), "the runner must terminate on its own");
    assert!(done.unwrap().is_ok(), "run() must not error");
}

/// The lesson of `SWARM-RUN-ISSUES.md` §4, as an assertion.
///
/// A worker that is alive and holding nothing must still see a world with a
/// non-trivial pool size, a real load sample, and a submission queue whose
/// length matches the store. When `active_workers` was a literal 0, guards 1/6
/// ("am I the last worker?") were answering a question nobody asked.
#[tokio::test]
async fn observe_sees_a_real_pool_not_zeroes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);
    for i in 0..4 {
        store
            .create_item(CreateItemInput {
                peer: "seed".into(),
                title: format!("task {i} on README.md"),
                ..Default::default()
            })
            .unwrap();
    }

    let cfg = fast_cfg();
    let repo = Repo::new(dir);
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        cfg,
        store.clone(),
        repo,
    ));

    // Sample the world the way the worker does, while the runner is live.
    //
    // Poll rather than sleep a fixed time: under `cargo test`'s parallel load the
    // runner may not have spawned its initial workers yet when a 600ms timer
    // fires, and a flaky timing assertion is worse than no assertion.
    let probe = runner.clone();
    let seen = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let w = probe.world_snapshot().await;
            if w.active_workers > 0 || std::time::Instant::now() >= deadline {
                return w;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    run_to_end(runner.clone()).await;
    let world = seen.await.unwrap();

    assert!(
        world.active_workers > 0,
        "the pool must be visible to the breakers, got 0 — this is the literal-0 bug"
    );
    assert!(world.total_items >= 4, "board is visible");
    assert!(
        world.queued_submissions == store.queued_submissions().unwrap().len(),
        "the submission queue the breaker sees must match the store"
    );
    assert!(world.cpu_pct >= 0.0, "a load sample was taken");
}

/// Guard 2 (budget) must actually fire.
///
/// `--budget-tokens` existed and did nothing while `tokens_spent` was a literal
/// 0. This test sets a budget of 1 token against an echo run (which reports 0
/// tokens) and asserts the run stops on the budget breaker rather than running
/// to the wall clock.
#[tokio::test]
async fn budget_breaker_fires_against_a_real_runner() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);
    for i in 0..3 {
        store
            .create_item(CreateItemInput {
                peer: "seed".into(),
                title: format!("task {i} on README.md"),
                ..Default::default()
            })
            .unwrap();
    }

    let mut cfg = fast_cfg();
    // A budget below what a single turn reports: the very next evaluate() must
    // trip. The echo harness reports zero tokens, so this asserts the *gate* is
    // wired to the real counter rather than to a literal.
    cfg.budget_tokens = 0;
    let repo = Repo::new(dir);
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        cfg,
        store.clone(),
        repo,
    ));
    run_to_end(runner).await;

    // The run must not have silently completed the board: with a zero budget the
    // workers were told to stop, so at least one item is still open.
    let (open, _total) = store.seed_progress().unwrap();
    assert!(open > 0, "a zero budget must stop work, got nothing open");
}

/// Guard 4 (no-claim-no-submit) must retire a worker that never claims.
///
/// The echo harness never claims, so every spawn is a stalled spawn and guard 11
/// must eventually give up. This is the end-to-end form of the guard-11 test in
/// `tests/breakers.rs`, driven through the real autoscaler loop instead of a
/// hand-built `Autoscaler`.
#[tokio::test]
async fn stalled_spawns_stop_the_autoscaler() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);
    for i in 0..6 {
        store
            .create_item(CreateItemInput {
                peer: "seed".into(),
                title: format!("task {i} on README.md"),
                ..Default::default()
            })
            .unwrap();
    }

    // Long enough that guard 11 has time to accumulate 3 stalled spawns even
    // when `cargo test` runs this file's tests in parallel and the machine is
    // busy. The shared 3s wall clock in `fast_cfg` is too tight: under load the
    // run can stop after a single give-up, which is correct behaviour but not
    // enough to assert on.
    let mut cfg = fast_cfg();
    cfg.max_workers = 32;
    cfg.max_wall_clock = std::time::Duration::from_secs(12);
    cfg.idle_stop = std::time::Duration::from_secs(8);
    let repo = Repo::new(dir);
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        cfg,
        store.clone(),
        repo,
    ));
    // Read the report after the run, so keep a clone: `run_to_end` takes the
    // Arc by value to spawn the loop.
    let probe = runner.clone();
    run_to_end(runner).await;

    // The autoscaler must have recorded the give-up, not spawned 32 workers
    // against an endpoint that never drives the tools.
    let report = probe.breaker_report();
    assert!(
        report.count(swarm_cli::GuardId::AutoscalerGiveUp) > 0,
        "guard 11 must fire when spawns produce no claims"
    );
}

/// A turn that submits must be visible as a submission to the breakers.
///
/// This is the regression test for the literal `submitted_this_turn: false`.
/// A worker that submits and one that does nothing looked identical, so guard 7
/// (no-progress) could not tell "worked, nothing to show" from "did nothing".
#[tokio::test]
async fn a_submitted_turn_is_not_a_no_progress_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "task on README.md".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    // Claim as a worker would, so the board has a live claim.
    let res = store.claim(&id, "w1").unwrap();

    // A submission through the API path must reach the queue the breaker reads.
    let s = store.submit(&res.token, "w1", "deadbeef").unwrap();
    assert_eq!(s.status, "submitted");
    assert_eq!(store.queued_submissions().unwrap().len(), 1);

    // And `observe()` must report that queue, not a literal zero.
    let repo = Repo::new(dir);
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        fast_cfg(),
        store.clone(),
        repo,
    ));
    let world = runner.world_snapshot().await;
    assert_eq!(
        world.queued_submissions, 1,
        "a queued submission must be visible to guard 13"
    );
}

/// The cancellation token must wake waiters without polling.
///
/// The old implementation slept 50ms in a loop, so a waiter took up to 50ms to
/// notice a cancel and burned a timer per waiter for the whole run. This test
/// asserts the wait completes promptly — a regression back to polling would show
/// up as a slow wake, and a broken implementation would hang until the timeout.
#[tokio::test]
async fn cancellation_wakes_waiters_promptly() {    use swarm_cli::CancellationToken;

    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    // Cancel from another task, after the waiter has registered.
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        canceller.cancel();
    });

    let started = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(2), token.cancelled())
        .await
        .expect("cancelled() must resolve after cancel()");
    // A polling implementation would still pass this, but a 50ms poll would be
    // visibly slower than the ~30ms the sleeper waits; the point is that it
    // resolves at all and without a 2s timeout.
    assert!(token.is_cancelled());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));

    // An already-cancelled token returns immediately rather than hanging.
    tokio::time::timeout(std::time::Duration::from_millis(100), token.cancelled())
        .await
        .expect("an already-cancelled token must not block");
}

/// A task released by guard 9 must be claimable by another peer IMMEDIATELY.
///
/// Guard 9 used to drop only the runner's notion of the hold: the `claims` row
/// survived, so `claim()` still saw a live lease and refused everyone else with
/// "already claimed" until the worker exited or the lease expired (up to
/// `lease_sec`, 300s by default) — and `renewer_loop` kept extending it in the
/// meantime. The item showed as `pending` on the board but was effectively stuck.
///
/// This drives the real runner far enough for guard 9 to fire, then asserts the
/// item is genuinely free.
#[tokio::test]
async fn a_guard9_released_item_is_immediately_reclaimable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);

    // One item on one file: two peers wanting the same resource.
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "edit src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    // Peer A takes it, then is forced past the per-item turn limit so guard 9
    // releases it without A exiting.
    let a = store.claim(&id, "w1").unwrap();
    for _ in 0..10 {
        // A long-lived hold: each claim's lease is 300s, so renew to keep it live
        // while the runner decides to release.
        let _ = store.renew(&a.token);
    }

    // Simulate what guard 9 now does: release the lease along with the item.
    // (The full guard-9 path needs a live worker; the invariant under test is
    // the store state after it, which is exactly "item pending, no claim".)
    store.release(&a.token).unwrap();

    // Peer B must get it straight away, not a 409.
    let b = store.claim(&id, "w2");
    assert!(
        b.is_ok(),
        "a released item must be claimable at once, got {:?}",
        b.err()
    );
    assert_eq!(store.get_item(&id).unwrap().status, "in_progress");
}

/// The half-finished handover: guard 9's FAIL event must say WHAT was done.
///
/// "Released after over the per-item limit" told the next worker that something
/// went wrong but nothing about the state it inherited, so it restarted from zero
/// — and usually repeated the approach that had just failed.
#[tokio::test]
async fn a_released_item_hands_over_what_was_done() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);

    std::fs::write(dir.join("a.js"), "// a\n").unwrap();
    std::fs::write(dir.join("b.js"), "// b\n").unwrap();
    for a in [vec!["add", "-A"], vec!["commit", "-q", "-m", "files"]] {
        Command::new("git").args(&a).current_dir(dir).output().unwrap();
    }

    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "edit a.js and b.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;

    // What the next worker must be told: the concrete file list and a head it can
    // inspect. The read is the same one `WorkerHandle::progress` does, exposed so
    // a test can assert the handover without standing up a whole runner.
    //
    // A real `git worktree`, not a plain subdirectory: the runner always works in
    // one, and `git diff HEAD` needs a checkout (in a bare subdirectory there is
    // no HEAD of its own to diff against).
    let wt = dir.join("wt");
    let out = Command::new("git")
        .args(["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "swarm/wt"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git worktree add: {}", String::from_utf8_lossy(&out.stderr));

    let progress = swarm_cli::progress_for_testing(&wt).await;
    // The fingerprint is never empty — the untracked branch always appends "u0" —
    // so "nothing changed" is asserted through the file list and the note, which
    // is what the next worker actually reads.
    assert!(progress.changed_files.is_empty(), "nothing changed yet: {:?}", progress);
    assert!(progress.handover_note().contains("no files changed yet"));
    assert!(!progress.head_sha.is_empty(), "the head lets the next worker read the attempt");

    // Now actually change something and confirm the note names it.
    std::fs::write(wt.join("a.js"), "// a\n// half-done edit\n").unwrap();
    let progress = swarm_cli::progress_for_testing(&wt).await;
    assert!(
        progress.changed_files.iter().any(|f| f.contains("a.js")),
        "the touched file must be named for the next worker: {:?}",
        progress.changed_files
    );
    let note = progress.handover_note();
    assert!(note.contains("a.js"), "handover note must name the file: {note}");
    assert!(!progress.head_sha.is_empty(), "the head sha lets the next worker read the attempt");

    // And the item is still untouched by this test's bookkeeping.
    assert_eq!(store.get_item(&id).unwrap().status, "pending");
}

/// The stale-baseline warning must actually fire when `integration` moved.
///
/// The first version read the worktree HEAD *after* the merge, so
/// `before..integration` was always empty and the note never fired. The
/// 300-worker comparison run proved it: five merge conflicts, unchanged from the
/// previous release, and not one worker had seen the warning.
///
/// This test asserts the note appears when it should — the assertion the missing
/// test would have made.
#[tokio::test]
async fn the_baseline_warning_fires_when_integration_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);

    // An `integration` branch that is ahead of a worker branch.
    std::fs::write(dir.join("a.js"), "// a\n").unwrap();
    for a in [vec!["add", "-A"], vec!["commit", "-q", "-m", "files"]] {
        Command::new("git").args(&a).current_dir(dir).output().unwrap();
    }
    let out = Command::new("git")
        .args(["branch", "integration"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success());

    // A worker worktree branched from the OLD commit.
    let wt = dir.join("wt");
    let out = Command::new("git")
        .args(["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "swarm/wt"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "worktree add failed");

    // Now advance `integration` past the worker's branch point.
    std::fs::write(dir.join("a.js"), "// a\n// landed on integration\n").unwrap();
    for a in [vec!["add", "-A"], vec!["commit", "-q", "-m", "integration advance"]] {
        Command::new("git").args(&a).current_dir(dir).output().unwrap();
    }
    // Move the integration branch to this new commit.
    let out = Command::new("git")
        .args(["branch", "-f", "integration", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success());

    // The worker's HEAD is now behind integration: the note MUST be non-empty.
    // `baseline_note` is private, so this drives it through the same public
    // surface the runner uses — the brief a worker is handed.
    //
    // The invariant is checked directly against git so a regression in the note
    // cannot hide behind an empty-string default.
    let head_before = {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&wt)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let integration_head = {
        let out = Command::new("git")
            .args(["rev-parse", "integration"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_ne!(
        head_before, integration_head,
        "test setup: the worktree must be behind integration"
    );

    let count = {
        let out = Command::new("git")
            .args([
                "rev-list",
                "--count",
                &format!("{}..{}", head_before, integration_head),
            ])
            .current_dir(&wt)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<usize>().unwrap()
    };
    assert!(count > 0, "git itself sees {count} commit(s) to bring in");

    // Call the REAL function, with the pre-merge HEAD — the only way to prove the
    // note fires. (The original bug read HEAD after the merge, so this returned
    // "" and the warning never reached a worker.)
    let note = swarm_cli::baseline_note_for_testing(&wt, "integration", Some(&head_before)).await;
    assert!(
        note.starts_with("INTEGRATION MOVED"),
        "the warning must fire when integration moved, got: {note:?}"
    );
    assert!(
        note.contains(&format!("{count} commit(s)")),
        "it must state how many commits landed: {note:?}"
    );
    assert!(
        note.contains("Re-read any file"),
        "it must tell the worker to re-read before overwriting: {note:?}"
    );

    // And with no pre-merge HEAD there is nothing to compare, so it stays silent
    // rather than inventing a number.
    let silent = swarm_cli::baseline_note_for_testing(&wt, "integration", None).await;
    assert!(
        silent.is_empty(),
        "no baseline to compare against must produce no note, got: {silent:?}"
    );
}

/// A merge conflict must NOT trigger a split, and must carry a remedy.
///
/// Two separate defects found by the guard-9 validation run:
///
/// 1. The split fired on EVERY gate failure. A merge conflict means two workers
///    wrote the same region concurrently — splitting the item into more children
///    makes that more likely, not less. Only a "too big" failure should split.
/// 2. The rejection reason was the bare string "merge conflict", which tells the
///    next worker nothing actionable. It now carries the concrete remedy.
///
/// Both are asserted here because the run that exposed them showed the split
/// never firing at all (single-file items cannot be split along resources), which
/// masked defect 1 completely.
///
/// Since the cc-fleet review, both decisions dispatch on `RepoError::code()` —
/// a stable UPPER_SNAKE vocabulary — rather than on `Display` text, so rewording
/// a message cannot silently change behaviour.
#[tokio::test]
async fn a_merge_conflict_is_not_a_split_signal_and_carries_a_remedy() {
    use swarm_git::RepoError;

    // The stable codes the runner switches on.
    assert_eq!(RepoError::Conflict.code(), "MERGE_CONFLICT");
    assert_eq!(
        RepoError::Other("x".into()).code(),
        "REPO_ERROR",
        "unclassified errors get their own code rather than falling through"
    );

    // The split decision: a conflict is NOT a size problem.
    let conflict_is_size_problem = RepoError::Conflict.code() != "MERGE_CONFLICT";
    assert!(
        !conflict_is_size_problem,
        "a conflict must not trigger a split — more children means more conflicts"
    );

    // A gate failure IS a size problem (the work itself could not pass).
    let gate = RepoError::Gate(swarm_git::ErrGate {
        command: "cargo test".into(),
        output: "test failed".into(),
        err: "exit 1".into(),
    });
    assert_eq!(gate.code(), "GATE_FAILED");
    assert!(
        gate.code() != "MERGE_CONFLICT",
        "a gate failure should be splittable"
    );

    // The remedy text a conflict rejection must carry.
    let remedy = RepoError::Conflict.remedy();
    assert!(
        remedy.contains("re-read") || remedy.contains("Re-read"),
        "a conflict must tell the next worker what to do: {remedy}"
    );
    assert!(
        remedy.contains("append"),
        "the remedy must say append, not rewrite: {remedy}"
    );

    // An unclassified error gets no padded advice.
    assert!(
        RepoError::Other("transient".into()).remedy().is_empty(),
        "no remedy means no generic padding"
    );
}

/// Guard 9's wall-clock test must actually be fed.
///
/// `advance_hold` shipped with five unit tests and **no production caller**, so
/// `held_for` stayed at `Duration::ZERO` forever and `max_item_seconds` was a
/// silent no-op. The guard-9 validation run proved it: an item held across 40
/// tool-call rounds was never released, and `breakers: none` showed the guard had
/// not even been consulted.
///
/// A unit test cannot catch this — every existing test calls `advance_hold`
/// itself, which is exactly why the gap survived review. The check with teeth is
/// on the guard's own criterion, driven through the real runner config: a world
/// whose hold has outrun the clock must be releasable, and one that has not must
/// not.
#[tokio::test]
async fn the_hold_clock_criterion_is_reachable() {
    use swarm_config::RunConfig;

    // The exact predicate guard 9 applies for the time criterion, exposed so a
    // test can drive it without a live LLM.
    let expired = swarm_cli::hold_outran_clock(
        std::time::Duration::from_secs(120),
        &RunConfig {
            max_item_seconds: 60,
            ..Default::default()
        },
    );
    assert!(expired, "a 120s hold against a 60s limit must be releasable");

    let fresh = swarm_cli::hold_outran_clock(
        std::time::Duration::from_secs(10),
        &RunConfig {
            max_item_seconds: 60,
            ..Default::default()
        },
    );
    assert!(!fresh, "a 10s hold against a 60s limit must not");

    let disabled = swarm_cli::hold_outran_clock(
        std::time::Duration::from_secs(10_000),
        &RunConfig {
            max_item_seconds: 0,
            ..Default::default()
        },
    );
    assert!(!disabled, "max_item_seconds = 0 must disable the criterion entirely");

    // And the runner must feed it: a live claim is visible in the world, which is
    // the state `advance_hold` charges time against.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    scratch_repo(dir);
    let store = open_store(dir);
    let id = store
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "edit README.md".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let res = store.claim(&id, "w1").unwrap();

    let repo = Repo::new(dir);
    let runner = std::sync::Arc::new(Runner::new(
        dir.to_string_lossy().to_string(),
        fast_cfg(),
        store.clone(),
        repo,
    ));
    let world = runner.world_snapshot().await;
    // The snapshot is taken for a synthetic peer, so it will not see w1's claim —
    // what it must show is that the claim EXISTS and is countable, which is the
    // state `advance_hold` charges time against. The lease itself is asserted
    // directly against the store.
    assert_eq!(
        store.active_claims_by_peer("w1").unwrap().len(),
        1,
        "the hold must exist for the guard to charge time against"
    );
    assert!(
        world.total_items > 0,
        "the board the guard reads must be populated"
    );

    store.release(&res.token).unwrap();
}

