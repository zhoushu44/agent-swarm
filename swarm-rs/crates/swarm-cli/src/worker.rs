//! Worker status and the per-worker ledger.
//!
//! Mirrors Go `workerState` / `itemLedger` (runner.go L167-222), with one fix
//! from `docs/OPTIMIZATION-SWARM.md` §2.2: the ledger is now removable, so a
//! long run does not accumulate one entry per item forever.

use std::collections::HashMap;
use std::time::Duration;

/// Where a worker is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    /// Working on a turn right now.
    Working,
    /// Nothing to do.
    Idle,
    /// Left the pool normally.
    Retired,
    /// Parked: broken (its claims are released).
    Crashed,
}

impl WorkerStatus {
    pub fn is_active(self) -> bool {
        !matches!(self, WorkerStatus::Retired | WorkerStatus::Crashed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WorkerStatus::Working => "working",
            WorkerStatus::Idle => "idle",
            WorkerStatus::Retired => "retired",
            WorkerStatus::Crashed => "crashed",
        }
    }
}

/// One worker's live state.
#[derive(Debug, Clone)]
pub struct WorkerState {
    pub name: String,
    pub status: WorkerStatus,
    pub turn: u32,
    pub idle_count: u32,
    /// Item currently held ("" when it holds nothing).
    pub held_item: String,
    /// Output fingerprint from the last auto-commit.
    pub last_artifact: String,
    /// Fingerprint from the turn before, for the static check.
    pub prev_artifact: String,
    /// When the fingerprint first went static.
    pub empty_since: Option<std::time::Instant>,
    /// API calls / tokens this worker has made.
    pub api_calls: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Whether this worker's loop has finished.
    pub loop_done: bool,
    /// Consecutive failed worktree removals (reaper backoff).
    pub reap_fail_count: u32,
    /// When the reaper last attempted this worker's worktree, so the backoff is
    /// measured from the attempt rather than from process start.
    pub last_reap_attempt: std::time::Instant,
}

impl WorkerState {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: WorkerStatus::Idle,
            turn: 0,
            idle_count: 0,
            held_item: String::new(),
            last_artifact: String::new(),
            prev_artifact: String::new(),
            empty_since: None,
            api_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            loop_done: false,
            reap_fail_count: 0,
            last_reap_attempt: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now),
        }
    }

    /// Whether this worker counts toward `active_workers`.
    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }
}

/// Per-item cost accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemLedger {
    pub tokens: i64,
    pub api_calls: u64,
    pub turns_held: u32,
}

/// Token spend attributed per item.
///
/// §2.2: the Go version only ever inserts. Here `remove` exists so the runner
/// can drop a ledger once its item reaches a terminal state, keeping memory
/// bounded on long runs.
#[derive(Debug, Default)]
pub struct ItemLedgers {
    inner: HashMap<String, ItemLedger>,
}

impl ItemLedgers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribute one round of spend to an item.
    pub fn add(&mut self, item_id: &str, tokens: i64, api_calls: u64) {
        let e = self.inner.entry(item_id.to_string()).or_default();
        e.tokens += tokens;
        e.api_calls += api_calls;
        e.turns_held += 1;
    }

    pub fn get(&self, item_id: &str) -> Option<&ItemLedger> {
        self.inner.get(item_id)
    }

    /// Drop an item's ledger (called when the item goes terminal).
    pub fn remove(&mut self, item_id: &str) {
        self.inner.remove(item_id);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The top-N items by token spend, for the end-of-run report.
    pub fn top_by_tokens(&self, n: usize) -> Vec<(&str, ItemLedger)> {
        let mut v: Vec<(&str, ItemLedger)> = self
            .inner
            .iter()
            .map(|(k, l)| (k.as_str(), *l))
            .collect();
        v.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens));
        v.truncate(n);
        v
    }
}

/// Reaper backoff for a worker: `10s * 2^min(fails, 5)`, capped at 320s.
pub fn reap_backoff(fail_count: u32) -> Duration {
    let exp = fail_count.min(5);
    Duration::from_secs(10 * (1u64 << exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_status_active_check() {
        assert!(WorkerStatus::Working.is_active());
        assert!(WorkerStatus::Idle.is_active());
        assert!(!WorkerStatus::Retired.is_active());
        assert!(!WorkerStatus::Crashed.is_active());
    }

    #[test]
    fn worker_starts_idle() {
        let w = WorkerState::new("w1");
        assert_eq!(w.status, WorkerStatus::Idle);
        assert!(w.held_item.is_empty());
        assert!(w.is_active());
    }

    #[test]
    fn ledger_accumulates_per_item() {
        let mut l = ItemLedgers::new();
        l.add("i1", 100, 1);
        l.add("i1", 50, 1);
        l.add("i2", 10, 1);
        assert_eq!(l.get("i1").unwrap().tokens, 150);
        assert_eq!(l.get("i1").unwrap().api_calls, 2);
        assert_eq!(l.get("i1").unwrap().turns_held, 2);
        assert_eq!(l.len(), 2);
    }

    /// §2.2: the Go ledger is never pruned.
    #[test]
    fn ledger_is_prunable() {
        let mut l = ItemLedgers::new();
        for i in 0..100 {
            l.add(&format!("i{}", i), 10, 1);
        }
        assert_eq!(l.len(), 100);
        for i in 0..90 {
            l.remove(&format!("i{}", i));
        }
        assert_eq!(l.len(), 10, "terminal items must not accumulate");
    }

    #[test]
    fn top_by_tokens_sorts_descending() {
        let mut l = ItemLedgers::new();
        l.add("cheap", 10, 1);
        l.add("expensive", 6_000_000, 200);
        l.add("medium", 1_000, 5);
        let top = l.top_by_tokens(2);
        assert_eq!(top[0].0, "expensive");
        assert_eq!(top[1].0, "medium");
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn reap_backoff_doubles_and_caps() {
        assert_eq!(reap_backoff(0), Duration::from_secs(10));
        assert_eq!(reap_backoff(1), Duration::from_secs(20));
        assert_eq!(reap_backoff(2), Duration::from_secs(40));
        assert_eq!(reap_backoff(5), Duration::from_secs(320));
        assert_eq!(reap_backoff(9), Duration::from_secs(320), "capped");
    }
}
