//! Behavioural tests for the store.
//!
//! These are the Rust equivalents of the Go `internal/store` tests, focused on
//! the semantics that are easy to get wrong when porting:
//!
//!   * `claim` accepts `in_progress` and reaps an expired lease in the same
//!     transaction (the "claim deadlock" fix),
//!   * a live lease still blocks a second claim,
//!   * `release` returns the item to `pending`,
//!   * `submit` moves the item to `submitted` and queues a submission,
//!   * `resolve_submission(ok)` → `done`, `!ok` → `pending` with backoff,
//!     and `failed` after `max_fails`,
//!   * `seed_progress` counts seed lineage through `parent_id`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use swarm_core::{status, CreateItemInput, Event, RunLogEntry};
use swarm_store::Store;

/// A store whose clock the test can advance.
fn store_with_clock() -> (Store, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let c = clock.clone();
    let st = Store::open_with_clock("", Some(Arc::new(move || c.load(Ordering::SeqCst))))
        .expect("open in-memory store");
    (st, clock)
}

/// Seed an item the way the operator does.
///
/// `peer` is `"seed"`, NOT empty: `seed_progress` counts items by
/// `created_by = 'seed'`, so changing this breaks every seed-lineage assertion.
///
/// Dedup does not apply because the seeder passes a peer that is not a worker —
/// see `create_item`, which bypasses dedup for `created_by`-style callers by
/// checking the peer against the seed marker rather than for emptiness.
fn seed(st: &Store, title: &str) -> String {
    st.create_item(CreateItemInput {
        peer: "seed".into(),
        title: title.into(),
        ..Default::default()
    })
    .unwrap()
    .id
}

#[test]
fn create_and_get_item() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "fix src/a.js");
    let it = st.get_item(&id).unwrap();
    assert_eq!(it.title, "fix src/a.js");
    assert_eq!(it.status, status::PENDING);
    assert_eq!(it.created_by, "seed");
    assert_eq!(it.kind, "task");
}

// ------------------------------------------------------------------
// Item dedup: worker-created items fold into an existing open item.
//
// Go has this (`internal/store/items.go` L358-395) and the Rust doc comment
// claimed it for as long as the port existed, but `artifact_key` was re-exported
// as `dedup_key` and called by nothing — so every worker-created item was a fresh
// row, and two workers could end up contending on one file under different ids.
// The `xtask` no-production-caller check is what found it.
// ------------------------------------------------------------------

/// Two differently-worded titles naming the same file must fold.
///
/// This is level 1 of Go's dedup and the case that actually occurs: a worker says
/// "add tests for src/a.js" while the board already has "create src/a.js".
#[test]
fn worker_items_fold_on_the_file_key() {
    let (st, _) = store_with_clock();
    let first = st
        .create_item(CreateItemInput {
            peer: "w1".into(),
            title: "create src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let second = st
        .create_item(CreateItemInput {
            peer: "w2".into(),
            title: "add tests for src/a.js".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(second.id, first, "must fold, not create a twin");
    assert_eq!(st.item_count().unwrap(), 1, "no duplicate row");
}

/// Titles with no path fall back to an exact-title match.
#[test]
fn worker_items_fold_on_exact_title_without_a_path() {
    let (st, _) = store_with_clock();
    let first = st
        .create_item(CreateItemInput {
            peer: "w1".into(),
            title: "investigate the flaky test".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let second = st
        .create_item(CreateItemInput {
            peer: "w2".into(),
            title: "investigate the flaky test".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(second.id, first, "exact titles must fold");
}

/// The seeder bypasses dedup, so deliberate duplicate seeds still work.
#[test]
fn seed_items_bypass_dedup() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let b = seed(&st, "edit src/a.js");
    assert_ne!(a, b, "the operator may seed duplicates on purpose");
    assert_eq!(st.item_count().unwrap(), 2);
}

/// `auto-split` children must not fold into each other or into the parent.
///
/// presplit creates one child per resource with `peer: "auto-split"`. Treating
/// that as a worker peer folded the children together, which silently turned a
/// two-file split into a one-file split — the e2e test caught it as
/// "one child per tracked resource: left 1, right 2".
#[test]
fn auto_split_children_do_not_fold() {
    let (st, _) = store_with_clock();
    let parent = seed(&st, "refactor a.js and b.js together");
    st.set_status(&parent, status::SPLIT).unwrap();
    for res in ["a.js", "b.js"] {
        st.create_item(CreateItemInput {
            peer: "auto-split".into(),
            title: res.into(),
            parent_id: Some(parent.clone()),
            ..Default::default()
        })
        .unwrap();
    }
    assert_eq!(st.child_count(&parent).unwrap(), 2, "one child per resource");
}

/// A terminal item must not absorb new work — only OPEN items fold.
#[test]
fn terminal_items_do_not_absorb_new_work() {
    let (st, _) = store_with_clock();
    let done = st
        .create_item(CreateItemInput {
            peer: "w1".into(),
            title: "edit src/a.js".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    st.set_status(&done, status::DONE).unwrap();

    let fresh = st
        .create_item(CreateItemInput {
            peer: "w2".into(),
            title: "edit src/a.js again".into(),
            ..Default::default()
        })
        .unwrap();
    assert_ne!(fresh.id, done, "a done item must not fold new work");
    assert_eq!(fresh.status, status::PENDING);
}

/// Prose is not a resource, so two prose items never fold on the file key.
///
/// Without this, "handle the error.message case" and "handle the error.detail
/// case" would collapse into one item — the exact failure `looks_like_resource`
/// exists to prevent.
#[test]
fn prose_titles_do_not_fold() {
    let (st, _) = store_with_clock();
    let a = st
        .create_item(CreateItemInput {
            peer: "w1".into(),
            title: "handle the error.message case".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let b = st
        .create_item(CreateItemInput {
            peer: "w2".into(),
            title: "handle the error.detail case".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    assert_ne!(a, b, "prose must not be treated as a shared resource");
    assert_eq!(st.item_count().unwrap(), 2);
}

#[test]
fn create_item_requires_title() {
    let (st, _) = store_with_clock();
    let err = st
        .create_item(CreateItemInput {
            peer: "seed".into(),
            title: "   ".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, swarm_core::SwarmError::Invalid(_)));
}

#[test]
fn claim_moves_item_to_in_progress() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    let res = st.claim(&id, "w1").unwrap();
    assert!(!res.token.is_empty());
    assert_eq!(res.item.status, status::IN_PROGRESS);
    assert_eq!(st.get_item(&id).unwrap().status, status::IN_PROGRESS);
}

#[test]
fn second_claim_on_live_lease_conflicts() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    st.claim(&id, "w1").unwrap();
    let err = st.claim(&id, "w2").unwrap_err();
    assert!(matches!(err, swarm_core::SwarmError::Conflict(_)));
}

/// The load-bearing one: an EXPIRED lease must be reaped inside `claim`, and
/// the item must come back to `pending` even though it was left `in_progress`.
/// If `claim` rejected `in_progress` at the door, this item would be stuck
/// forever.
#[test]
fn claim_reaps_expired_lease_and_recovers_in_progress_item() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "t");
    st.claim(&id, "w1").unwrap();
    assert_eq!(st.get_item(&id).unwrap().status, status::IN_PROGRESS);

    // Advance past the 300s lease.
    clock.fetch_add(301_000, Ordering::SeqCst);

    let res = st.claim(&id, "w2").expect("expired lease must be re-claimable");
    assert_eq!(res.item.status, status::IN_PROGRESS);
    // The old claim is gone; only w2 holds it.
    let mine = st.active_claims_by_peer("w2").unwrap();
    assert_eq!(mine.len(), 1);
    assert!(st.active_claims_by_peer("w1").unwrap().is_empty());
}

#[test]
fn claim_unknown_item_is_not_found() {
    let (st, _) = store_with_clock();
    let err = st.claim("nope", "w1").unwrap_err();
    assert!(matches!(err, swarm_core::SwarmError::NotFound(_)));
}

#[test]
fn claim_requires_item_and_peer() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    assert!(matches!(
        st.claim("", "w1").unwrap_err(),
        swarm_core::SwarmError::Invalid(_)
    ));
    assert!(matches!(
        st.claim(&id, "").unwrap_err(),
        swarm_core::SwarmError::Invalid(_)
    ));
}

#[test]
fn claim_rejects_terminal_item() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    st.set_status(&id, status::DONE).unwrap();
    let err = st.claim(&id, "w1").unwrap_err();
    assert!(matches!(err, swarm_core::SwarmError::Conflict(_)));
}

#[test]
fn release_returns_item_to_pending() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    let res = st.claim(&id, "w1").unwrap();
    st.release(&res.token).unwrap();
    assert_eq!(st.get_item(&id).unwrap().status, status::PENDING);
    assert!(st.active_claims_by_peer("w1").unwrap().is_empty());
    // and it is claimable again
    st.claim(&id, "w2").unwrap();
}

#[test]
fn renew_extends_lease() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "t");
    let res = st.claim(&id, "w1").unwrap();
    // Move close to expiry, then renew.
    clock.fetch_add(299_000, Ordering::SeqCst);
    st.renew(&res.token).unwrap();
    // Another 299s should still be live because renew pushed it out.
    clock.fetch_add(299_000, Ordering::SeqCst);
    let claims = st.active_claims_by_peer("w1").unwrap();
    assert_eq!(claims.len(), 1, "renew must keep the claim alive");
}

#[test]
fn submit_marks_submitted_and_queues() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    let res = st.claim(&id, "w1").unwrap();
    let it = st.submit(&res.token, "w1", "abc123").unwrap();
    assert_eq!(it.status, status::SUBMITTED);

    let q = st.queued_submissions().unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].item_id, id);
    assert_eq!(q[0].commit_sha, "abc123");
    assert_eq!(q[0].state, "queued");
}

#[test]
fn resolve_ok_marks_done() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    let res = st.claim(&id, "w1").unwrap();
    st.submit(&res.token, "w1", "sha").unwrap();
    let sub = st.queued_submissions().unwrap().remove(0);

    st.resolve_submission(sub.id, true, "", 3).unwrap();
    assert_eq!(st.get_item(&id).unwrap().status, status::DONE);
    assert!(st.queued_submissions().unwrap().is_empty());
}

#[test]
fn resolve_reject_requeues_then_fails_after_max() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");

    // First rejection → back to pending (with a backoff).
    let res = st.claim(&id, "w1").unwrap();
    st.submit(&res.token, "w1", "sha1").unwrap();
    let sub = st.queued_submissions().unwrap().remove(0);
    st.resolve_submission(sub.id, false, "gate failed", 2).unwrap();
    assert_eq!(st.get_item(&id).unwrap().status, status::PENDING);
    assert!(st.get_item(&id).unwrap().not_before.is_some());

    // Second rejection reaches max_fails=2 → failed.
    let res = st.claim(&id, "w1").unwrap();
    st.submit(&res.token, "w1", "sha2").unwrap();
    let sub = st.queued_submissions().unwrap().remove(0);
    st.resolve_submission(sub.id, false, "gate failed again", 2).unwrap();
    assert_eq!(st.get_item(&id).unwrap().status, status::FAILED);
}

#[test]
fn reject_backoff_is_capped_at_60s() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "t");
    // Drive several rejections so the shift would exceed the cap.
    for _ in 0..5 {
        let res = st.claim(&id, "w1").unwrap();
        st.submit(&res.token, "w1", "s").unwrap();
        let sub = st.queued_submissions().unwrap().remove(0);
        st.resolve_submission(sub.id, false, "x", 99).unwrap();
        // Skip past the backoff so the next claim succeeds.
        clock.fetch_add(120_000, Ordering::SeqCst);
    }
    let nb = st.get_item(&id).unwrap().not_before.unwrap();
    let now = clock.load(Ordering::SeqCst);
    assert!(
        nb - now <= 60_000,
        "backoff must be capped at 60s, got {}ms",
        nb - now
    );
}

#[test]
fn reap_expired_returns_items_to_pending() {
    let (st, clock) = store_with_clock();
    let a = seed(&st, "a");
    let b = seed(&st, "b");
    st.claim(&a, "w1").unwrap();
    st.claim(&b, "w2").unwrap();

    clock.fetch_add(301_000, Ordering::SeqCst);
    let n = st.reap_expired().unwrap();
    assert_eq!(n, 2);
    assert_eq!(st.get_item(&a).unwrap().status, status::PENDING);
    assert_eq!(st.get_item(&b).unwrap().status, status::PENDING);
}

#[test]
fn reap_keeps_live_leases() {
    let (st, clock) = store_with_clock();
    let a = seed(&st, "a");
    st.claim(&a, "w1").unwrap();
    clock.fetch_add(100_000, Ordering::SeqCst); // still inside the 300s lease
    assert_eq!(st.reap_expired().unwrap(), 0);
    assert_eq!(st.get_item(&a).unwrap().status, status::IN_PROGRESS);
}

#[test]
fn stats_and_counts() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "a");
    let b = seed(&st, "b");
    let _c = seed(&st, "c");

    assert_eq!(st.stats().unwrap().pending, 3);
    assert_eq!(st.open_count().unwrap(), 3);
    assert_eq!(st.claimable_count().unwrap(), 3);

    st.claim(&a, "w1").unwrap();
    let s = st.stats().unwrap();
    assert_eq!(s.pending, 2);
    assert_eq!(s.in_progress, 1);
    assert_eq!(s.total, 3);

    st.set_status(&b, status::DONE).unwrap();
    assert_eq!(st.stats().unwrap().done, 1);
    // claimable excludes the claimed item
    assert_eq!(st.claimable_count().unwrap(), 1);
}

#[test]
fn claimable_respects_not_before() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "t");
    // Simulate a requeue with a future not_before by rejecting a submission.
    let res = st.claim(&id, "w1").unwrap();
    st.submit(&res.token, "w1", "s").unwrap();
    let sub = st.queued_submissions().unwrap().remove(0);
    st.resolve_submission(sub.id, false, "x", 99).unwrap();

    assert_eq!(st.claimable_count().unwrap(), 0, "backoff must hide the item");
    clock.fetch_add(120_000, Ordering::SeqCst);
    assert_eq!(st.claimable_count().unwrap(), 1, "item returns after backoff");
}

// ------------------------------------------------------------------
// Guard 9 requeue: an item that runs over its per-item limit must be held
// back before the next worker may take it, and must eventually die.
//
// The bug this covers: `release()` put the item straight back to `pending`
// with no `not_before`, so the next worker claimed it seconds later and
// burned another full lease on the same approach. On a 20 MB bundle that was
// five consecutive 800s leases with zero progress between them — the item
// never failed and never succeeded, it just kept consuming the pool.
// ------------------------------------------------------------------

#[test]
fn release_with_backoff_delays_the_next_claim() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "analysis/huge.json");
    let res = st.claim(&id, "w1").unwrap();

    let delay = st.release_with_backoff(&res.token).unwrap();
    assert!(delay > 0, "a timed-out item must not be immediately claimable");
    assert_eq!(st.get_item(&id).unwrap().status, status::PENDING);
    assert_eq!(
        st.claimable_count().unwrap(),
        0,
        "not_before must hide the item from the next claimer"
    );

    clock.fetch_add(delay + 1, Ordering::SeqCst);
    assert_eq!(
        st.claimable_count().unwrap(),
        1,
        "the item returns once the backoff elapses"
    );
}

#[test]
fn release_with_backoff_grows_with_repeated_timeouts() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "analysis/huge.json");

    let mut delays = Vec::new();
    for _ in 0..3 {
        // Each attempt records the FAIL that the real runner writes before
        // releasing; the backoff is derived from those rows.
        st.add_context(Event {
            peer: "w1".into(),
            kind: "FAIL".into(),
            item_id: Some(id.clone()),
            text: "released after over the per-item limit".into(),
            tags: Some(vec!["item".into(), "timeout".into(), "item-hold".into()]),
            ..Default::default()
        })
        .unwrap();
        let res = st.claim(&id, "w1").unwrap();
        delays.push(st.release_with_backoff(&res.token).unwrap());
        clock.fetch_add(400_000, Ordering::SeqCst);
    }

    assert!(
        delays[1] > delays[0] && delays[2] > delays[1],
        "backoff must grow with each reclaimed attempt, got {:?}",
        delays
    );
}

#[test]
fn item_dies_after_max_reclaims_instead_of_looping_forever() {
    let (st, clock) = store_with_clock();
    let id = seed(&st, "analysis/impossible.json");

    // MAX_ITEM_RECLAIMS timeouts in a row. Each one records its FAIL first, the
    // way the runner does.
    for _ in 0..swarm_store::store::MAX_ITEM_RECLAIMS {
        st.add_context(Event {
            peer: "w1".into(),
            kind: "FAIL".into(),
            item_id: Some(id.clone()),
            text: "released after over the per-item limit".into(),
            tags: Some(vec!["item".into(), "timeout".into(), "item-hold".into()]),
            ..Default::default()
        })
        .unwrap();
        if st.get_item(&id).unwrap().status != status::PENDING {
            break;
        }
        let res = st.claim(&id, "w1").unwrap();
        st.release_with_backoff(&res.token).unwrap();
        clock.fetch_add(400_000, Ordering::SeqCst);
    }

    assert_eq!(
        st.get_item(&id).unwrap().status,
        status::FAILED,
        "an item that never fits in one lease must be failed, not requeued forever"
    );
    assert_eq!(st.claimable_count().unwrap(), 0);
}

#[test]
fn seed_progress_follows_parent_lineage() {
    let (st, _) = store_with_clock();
    let parent = seed(&st, "big task");
    // Split into two children created by auto-split (not 'seed').
    for t in ["src/a.js", "src/b.js"] {
        st.create_item(CreateItemInput {
            peer: "auto-split".into(),
            title: t.into(),
            parent_id: Some(parent.clone()),
            ..Default::default()
        })
        .unwrap();
    }
    let (open, total) = st.seed_progress().unwrap();
    assert_eq!(total, 3, "parent + 2 children carry seed lineage");
    assert_eq!(open, 3);

    st.set_status(&parent, status::SPLIT).unwrap();
    let (open, _) = st.seed_progress().unwrap();
    assert_eq!(open, 2, "split parent counts as terminal");
}

#[test]
fn item_depth_walks_parents() {
    let (st, _) = store_with_clock();
    let root = seed(&st, "root");
    let child = st
        .create_item(CreateItemInput {
            peer: "auto-split".into(),
            title: "c".into(),
            parent_id: Some(root.clone()),
            ..Default::default()
        })
        .unwrap()
        .id;
    let grandchild = st
        .create_item(CreateItemInput {
            peer: "auto-split".into(),
            title: "g".into(),
            parent_id: Some(child.clone()),
            ..Default::default()
        })
        .unwrap()
        .id;

    assert_eq!(st.item_depth(&root).unwrap(), 0);
    assert_eq!(st.item_depth(&child).unwrap(), 1);
    assert_eq!(st.item_depth(&grandchild).unwrap(), 2);
}

#[test]
fn context_roundtrip_and_digest() {
    let (st, _) = store_with_clock();
    seed(&st, "t");
    st.add_context(Event {
        id: 0,
        peer: "w1".into(),
        kind: "FAIL".into(),
        item_id: None,
        text: "gate failed".into(),
        tags: Some(vec!["integration".into()]),
        ts: 0,
    })
    .unwrap();

    let d = st.digest("w2", 0).unwrap();
    assert_eq!(d.new_context.len(), 1);
    assert_eq!(d.new_context[0].text, "gate failed");
    assert_eq!(
        d.new_context[0].tags.as_ref().unwrap(),
        &vec!["integration".to_string()]
    );
    assert_eq!(d.stats.pending, 1);
    assert_eq!(d.pending_preview.len(), 1);
}

#[test]
fn digest_excludes_own_entries() {
    let (st, _) = store_with_clock();
    st.add_context(Event {
        id: 0,
        peer: "w1".into(),
        kind: "NOTE".into(),
        item_id: None,
        text: "mine".into(),
        tags: None,
        ts: 0,
    })
    .unwrap();
    let d = st.digest("w1", 0).unwrap();
    assert!(d.new_context.is_empty(), "own entries must not echo back");
}

#[test]
fn intent_marks_produce_conflicts() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    st.mark_intent("w1", &a, &["src/a.js".into()], 60_000).unwrap();

    let b = seed(&st, "also src/a.js");
    let res = st.claim(&b, "w2").unwrap();
    assert_eq!(res.conflicts.len(), 1);
    assert_eq!(res.conflicts[0].resource, "src/a.js");
    assert_eq!(res.conflicts[0].peer, "w1");
}

#[test]
fn intent_marks_ignore_own_peer() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    st.mark_intent("w1", &a, &["src/a.js".into()], 60_000).unwrap();
    let res = st.claim(&a, "w1").unwrap();
    assert!(res.conflicts.is_empty(), "own intent must not conflict");
}

#[test]
fn expired_intent_marks_are_reaped() {
    let (st, clock) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    st.mark_intent("w1", &a, &["src/a.js".into()], 1_000).unwrap();
    clock.fetch_add(2_000, Ordering::SeqCst);

    let b = seed(&st, "also src/a.js");
    let res = st.claim(&b, "w2").unwrap();
    assert!(res.conflicts.is_empty(), "expired mark must not conflict");
}

#[test]
fn release_marks_drops_peer_intents() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    st.mark_intent("w1", &a, &["src/a.js".into()], 60_000).unwrap();
    st.release_marks("w1").unwrap();

    let b = seed(&st, "also src/a.js");
    assert!(st.claim(&b, "w2").unwrap().conflicts.is_empty());
}

#[test]
fn has_queued_submission_for_peer() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "t");
    assert!(!st.has_queued_submission_for_peer("w1").unwrap());
    let res = st.claim(&id, "w1").unwrap();
    st.submit(&res.token, "w1", "s").unwrap();
    assert!(st.has_queued_submission_for_peer("w1").unwrap());
}

#[test]
fn fail_all_open_items_marks_everything_failed() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "a");
    let b = seed(&st, "b");
    st.claim(&a, "w1").unwrap();
    let n = st.fail_all_open_items("budget exhausted").unwrap();
    assert_eq!(n, 2);
    assert_eq!(st.get_item(&a).unwrap().status, status::FAILED);
    assert_eq!(st.get_item(&b).unwrap().status, status::FAILED);
    assert_eq!(st.open_count().unwrap(), 0);
}

#[test]
fn recent_split_count_counts_new_items() {
    let (st, clock) = store_with_clock();
    assert_eq!(st.recent_split_count(300_000).unwrap(), 0);
    seed(&st, "a");
    seed(&st, "b");
    assert_eq!(st.recent_split_count(300_000).unwrap(), 2);
    clock.fetch_add(400_000, Ordering::SeqCst);
    assert_eq!(st.recent_split_count(300_000).unwrap(), 0, "window must expire");
}

#[test]
fn list_items_filters_by_status() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "a");
    seed(&st, "b");
    st.set_status(&a, status::DONE).unwrap();

    let (done, total) = st.list_items(status::DONE, 0, 0).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(total, 2, "total counts the whole board, not the filter");
    let (all, _) = st.list_items("", 0, 0).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn list_items_paginates() {
    let (st, _) = store_with_clock();
    for i in 0..5 {
        seed(&st, &format!("t{}", i));
    }
    let (page, _) = st.list_items("", 2, 2).unwrap();
    assert_eq!(page.len(), 2);
}

// ------------------------------------------------------------------
// Resource-level leases.
//
// The item-level claim is not enough: two items that name the same file both
// pass it, and the collision surfaces in the merge instead of at the door.
// These tests pin the resource-lease semantics that make the merge the last
// place a conflict can appear, not the first.
//
// Evidence for the design: the Super-agent lease ablation (reproduced in
// `docs/SUPER-AGENT-RESULTS.md`) measured zero rework and +9% throughput under a
// deterministic lease, against 97.6 rework events naive and 1.2M wasted tokens
// under LLM negotiation.
// ------------------------------------------------------------------

#[test]
fn resource_lease_blocks_a_second_peer() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let b = seed(&st, "edit src/a.js too");

    // Two different items, one shared resource. The item-level claim cannot tell
    // them apart; the resource lease must.
    let first = st.claim(&a, "w1").unwrap();
    let err = st.claim(&b, "w2").unwrap_err();
    assert!(
        matches!(err, swarm_core::SwarmError::Conflict(_)),
        "a second peer must not take an item over a held resource, got {:?}",
        err
    );
    // The error must name the resource — a bare 409 leaves the operator guessing.
    let msg = err.to_string();
    assert!(msg.contains("src/a.js"), "the blocked resource must be named: {msg}");

    // And the failed claim must not have left anything behind.
    assert_eq!(st.get_item(&b).unwrap().status, status::PENDING);
    assert_eq!(st.active_claims_by_peer("w2").unwrap().len(), 0);

    // The same peer may renew its own hold.
    st.renew(&first.token).unwrap();
}

#[test]
fn release_clears_resource_leases() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let res = st.claim(&a, "w1").unwrap();
    assert!(st.claim(&a, "w2").is_err(), "resource is held");

    st.release(&res.token).unwrap();

    // Once released the resource is free, so the *other* item can proceed.
    let b = seed(&st, "edit src/a.js again");
    assert!(st.claim(&b, "w2").is_ok(), "release must unpin the resource");
}

#[test]
fn submit_clears_resource_leases() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let res = st.claim(&a, "w1").unwrap();
    st.submit(&res.token, "w1", "sha").unwrap();

    // A submitted item must not keep pinning its files forever, or nothing that
    // names them could ever be claimed again.
    let b = seed(&st, "edit src/a.js follow-up");
    assert!(st.claim(&b, "w2").is_ok(), "submit must unpin the resource");
}

#[test]
fn resource_lease_expiry_frees_the_resource() {
    let (st, clock) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let _res = st.claim(&a, "w1").unwrap();
    assert!(st.claim(&a, "w2").is_err());

    // Expire the lease: both the claim and its resource leases must go.
    clock.fetch_add(400_000, Ordering::SeqCst);
    assert_eq!(st.reap_expired().unwrap(), 1);

    let b = seed(&st, "edit src/a.js");
    assert!(st.claim(&b, "w2").is_ok(), "an expired lease must not pin anything");
}

#[test]
fn symbol_granularity_does_not_conflict() {
    // The `#symbol` suffix is load-bearing: two functions in one file are
    // different resources, so they must not serialise. Reusing
    // `resources::item_resources` for the lease keys is what guarantees this
    // (README constraint 3).
    let (st, _) = store_with_clock();
    let a = seed(&st, "fix src/video.js#pollVideo");
    let b = seed(&st, "fix src/video.js#buildVideoBody");

    let _first = st.claim(&a, "w1").unwrap();
    assert!(
        st.claim(&b, "w2").is_ok(),
        "two functions of one file are separate resources"
    );
}

#[test]
fn symbol_keys_are_distinct_by_design_a_known_gap() {
    // **Known limitation, pinned deliberately.**
    //
    // `resources.rs` treats `src/video.js` and `src/video.js#pollVideo` as two
    // different keys — that is what makes two *functions* of one file
    // parallelisable (`symbol_granularity_keeps_same_file_apart`). The cost is
    // that an item naming the whole file does NOT block an item naming a symbol
    // in it, even though the whole-file rewrite can clobber the symbol.
    //
    // Fixing it means teaching the lease about prefix containment
    // (`src/video.js` covers `src/video.js#anything`), which changes dedup and
    // conflict-scan behaviour for every caller. That is a deliberate decision
    // for the resource model's owner, not a side effect of adding leases — so
    // the current behaviour is pinned here instead of being silently changed.
    //
    // Practical mitigation until then: the three-way merge arbitration in
    // `swarm-git` is what catches this case, and the presplit axis only splits
    // along whole-file resources.
    let (st, _) = store_with_clock();
    let a = seed(&st, "rewrite src/video.js");
    let b = seed(&st, "fix src/video.js#pollVideo");

    let _first = st.claim(&a, "w1").unwrap();
    assert!(
        st.claim(&b, "w2").is_ok(),
        "whole-file and symbol keys are distinct today; the merge arbitrates"
    );
}

#[test]
fn items_without_resources_never_conflict() {
    // Prose is not a resource (`error.message` must not become one), so two
    // prose items can always run in parallel.
    let (st, _) = store_with_clock();
    let a = seed(&st, "handle the error.message case");
    let b = seed(&st, "handle the error.message case too");
    let _first = st.claim(&a, "w1").unwrap();
    assert!(st.claim(&b, "w2").is_ok());
}

#[test]
fn a_failed_claim_leaves_no_lease_behind() {
    // The claim is one transaction: if the resource check rejects it, the claim
    // row must not survive. A claim without its lease would let the next peer
    // through while this item stays `in_progress` forever.
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let b = seed(&st, "edit src/a.js");
    let _first = st.claim(&a, "w1").unwrap();

    assert!(st.claim(&b, "w2").is_err());
    assert_eq!(
        st.get_item(&b).unwrap().status,
        status::PENDING,
        "a rejected claim must not mark the item in_progress"
    );
    // And releasing the holder must make it claimable again.
    let a_res = st.active_claims_by_peer("w1").unwrap().remove(0);
    st.release(&a_res.token).unwrap();
    assert!(st.claim(&b, "w2").is_ok());
}

// ------------------------------------------------------------------
// The run log: the append-only fact layer.
//
// The report's acceptance numbers are derived from these rows, so they can be
// recomputed after the process is gone. A summary that only lives in memory
// cannot answer "was that run's work any good?" a week later.
// ------------------------------------------------------------------

#[test]
fn run_log_appends_and_reads_back() {
    let (st, _) = store_with_clock();
    assert!(st.run_log(0).unwrap().is_empty(), "starts empty");

    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "integration".into(),
        actor: "w1".into(),
        item_id: "i1".into(),
        title: "edit src/a.js".into(),
        accepted: true,
        ..Default::default()
    })
    .unwrap();

    let rows = st.run_log(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "integration");
    assert_eq!(rows[0].actor, "w1");
    assert_eq!(rows[0].item_id, "i1");
    assert!(rows[0].accepted);
    assert!(rows[0].ts > 0, "an omitted ts is stamped by the store");
}

#[test]
fn acceptance_counts_are_derived_not_stored() {
    let (st, _) = store_with_clock();
    assert_eq!(
        st.acceptance_counts().unwrap(),
        (0, 0),
        "an empty log must not report a fabricated rate"
    );

    for i in 0..7 {
        st.append_run_log(RunLogEntry {
            id: 0,
            kind: "integration".into(),
            actor: "w1".into(),
            item_id: format!("i{i}"),
            accepted: i < 5,
            ..Default::default()
        })
        .unwrap();
    }

    let (accepted, attempted) = st.acceptance_counts().unwrap();
    assert_eq!(accepted, 5);
    assert_eq!(attempted, 7);
}

#[test]
fn run_log_limit_returns_the_earliest_rows() {
    // `ORDER BY id ASC LIMIT n` keeps the head of the log, which is what an
    // auditor replaying a run from the start needs.
    let (st, _) = store_with_clock();
    for i in 0..5 {
        st.append_run_log(RunLogEntry {
            id: 0,
            kind: "integration".into(),
            item_id: format!("i{i}"),
            accepted: true,
            ..Default::default()
        })
        .unwrap();
    }
    let rows = st.run_log(3).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].item_id, "i2", "the head of the log, not the tail");
}

#[test]
fn a_rejected_integration_lands_in_the_log() {
    // Rejections must be recorded too — a log of only successes is not an audit
    // trail, it is a highlight reel.
    let (st, _) = store_with_clock();
    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "rejected".into(),
        actor: "w2".into(),
        item_id: "i9".into(),
        detail: "gate failed: test exited 1".into(),
        accepted: false,
        ..Default::default()
    })
    .unwrap();

    let (accepted, attempted) = st.acceptance_counts().unwrap();
    assert_eq!(accepted, 0);
    assert_eq!(attempted, 1, "a rejection is an attempt");
    let rows = st.run_log(0).unwrap();
    assert!(rows[0].detail.contains("gate failed"), "the reason is kept");
}

// ------------------------------------------------------------------
// `/marks` must report both coordination mechanisms.
//
// An intent mark is a worker's HINT about what it plans to edit, with its own
// TTL. A resource lease is the AUTHORITY, written in the same transaction that
// grants the claim. Reporting only the hint is worse than reporting nothing: a
// worker sees zero, claims, and gets a 409 it could not anticipate.
// ------------------------------------------------------------------

#[test]
fn marks_reports_the_lease_even_with_no_intent_mark() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "edit src/a.js");
    let _res = st.claim(&id, "w1").unwrap();

    // No INTENT mark was ever written, so the mark list is empty...
    let view = st.active_marks("src/a.js", 50).unwrap();
    assert_eq!(view.marks.len(), 0, "no hint was published");
    // ...but the resource is held, and that is what a caller must see.
    assert_eq!(view.held(), 1, "the lease is the authoritative answer");
    assert!(view.is_held());
    assert_eq!(view.leases[0].peer, "w1");
}

#[test]
fn marks_with_no_resource_reports_every_lease() {
    let (st, _) = store_with_clock();
    let a = seed(&st, "edit src/a.js");
    let b = seed(&st, "edit src/b.js");
    st.claim(&a, "w1").unwrap();
    st.claim(&b, "w2").unwrap();

    let view = st.active_marks("", 50).unwrap();
    assert_eq!(view.held(), 2, "an empty resource means all resources");
}

#[test]
fn marks_does_not_deadlock_reading_both_halves() {
    // Regression: the first version of the marks/lease join called a method that
    // took the store's `Mutex` again while `active_marks` already held it. The
    // mutex is not reentrant, so the call self-deadlocked and the `/marks` test
    // hung instead of failing. This test completes only if the read is
    // single-lock.
    let (st, _) = store_with_clock();
    let id = seed(&st, "edit src/a.js");
    let _res = st.claim(&id, "w1").unwrap();
    st.mark_intent("w1", &id, &["src/a.js".into()], 60_000).unwrap();

    // Both halves populated at once: the case that used to deadlock.
    let view = st.active_marks("src/a.js", 50).unwrap();
    assert_eq!(view.marks.len(), 1);
    assert_eq!(view.held(), 1);
}

// ------------------------------------------------------------------
// Duplicate submission detection by content identity.
//
// `merge_only` reports `noop` for both "this branch added nothing" and "this
// work was already integrated". The runner used to accept both silently, so a
// duplicated submission counted as a fresh acceptance and inflated the accepted
// rate. The tree sha is what separates them: it is the CONTENT, stable across
// merges of the same work, where a commit sha is not.
// ------------------------------------------------------------------

#[test]
fn last_accepted_tree_round_trips_the_content_identity() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "edit src/a.js");

    // Never accepted: None, which the caller must read as "not a duplicate".
    assert_eq!(st.last_accepted_tree(&id).unwrap(), None);

    // An accepted integration carries its tree in `detail`.
    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "integration".into(),
        actor: "w1".into(),
        item_id: id.clone(),
        title: "abc123".into(),
        detail: "tree-sha-1".into(),
        accepted: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        st.last_accepted_tree(&id).unwrap().as_deref(),
        Some("tree-sha-1")
    );

    // A later acceptance replaces it — the LATEST tree is the comparison point.
    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "integration".into(),
        actor: "w2".into(),
        item_id: id.clone(),
        title: "def456".into(),
        detail: "tree-sha-2".into(),
        accepted: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        st.last_accepted_tree(&id).unwrap().as_deref(),
        Some("tree-sha-2"),
        "the most recent accepted tree is what a duplicate is compared against"
    );
}

/// A rejected entry must not become the duplicate-detection baseline.
#[test]
fn a_rejection_is_not_an_accepted_tree() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "edit src/a.js");
    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "rejected".into(),
        actor: "w1".into(),
        item_id: id.clone(),
        title: "abc".into(),
        detail: "tree-sha-rejected".into(),
        accepted: false,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        st.last_accepted_tree(&id).unwrap(),
        None,
        "only accepted integrations define the content identity"
    );
}

/// The log must record the state TRANSITION, not just the outcome.
///
/// agent-fleet-manager's `change_log` carries `field` / `old_value` /
/// `new_value`. Without them the log answers "was it rejected?" but not "how did
/// this item get to its current state?" — which is the question an operator
/// actually asks when a run goes wrong.
#[test]
fn run_log_records_the_state_transition() {
    let (st, _) = store_with_clock();
    let id = seed(&st, "edit src/a.js");
    st.append_run_log(RunLogEntry {
        id: 0,
        kind: "rejected".into(),
        actor: "w1".into(),
        item_id: id.clone(),
        title: "abc".into(),
        detail: "gate failed".into(),
        accepted: false,
        field: "status".into(),
        old_value: "submitted".into(),
        new_value: "pending".into(),
        ..Default::default()
    })
    .unwrap();

    let rows = st.run_log(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].field, "status");
    assert_eq!(rows[0].old_value, "submitted");
    assert_eq!(rows[0].new_value, "pending");
}

/// A database written before the provenance columns existed must still read.
///
/// The columns are additive with `NOT NULL DEFAULT ''`, and the read path
/// `COALESCE`s them, so an old file yields empty strings rather than a failed
/// query.
#[test]
fn run_log_reads_legacy_rows_without_provenance() {
    // A fresh on-disk database written by the PRE-migration schema: the base
    // `run_log` without the three provenance columns. Written with std rather
    // than a tempdir crate, which this crate does not depend on.
    let mut path = std::env::temp_dir();
    path.push(format!("swarm-legacy-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE run_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '', item_id TEXT, title TEXT, detail TEXT,
                tokens INTEGER NOT NULL DEFAULT 0, files INTEGER NOT NULL DEFAULT 0,
                seconds INTEGER NOT NULL DEFAULT 0, ts INTEGER NOT NULL DEFAULT 0,
                accepted INTEGER NOT NULL DEFAULT 0);
             INSERT INTO run_log(kind, actor, item_id, title, detail, tokens, files, seconds, ts, accepted)
             VALUES('integration','w1','i1','abc','',0,0,0,1,1);",
        )
        .unwrap();
    }

    // Opening it through the store migrates it in place.
    let st = Store::open(path.to_str().unwrap()).unwrap();
    let rows = st.run_log(0).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(rows.len(), 1, "the legacy row must survive the migration");
    assert_eq!(rows[0].kind, "integration");
    assert!(rows[0].accepted);
    assert_eq!(rows[0].field, "", "a legacy row has no provenance, not an error");
    assert_eq!(rows[0].old_value, "");
    assert_eq!(rows[0].new_value, "");
}
