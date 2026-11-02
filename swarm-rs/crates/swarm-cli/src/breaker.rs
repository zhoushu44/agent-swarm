//! The fourteen circuit breakers as a single pure state machine.
//!
//! Go implements these inline in a 400-line `workerLoop`, which makes them
//! nearly untestable: every one is entangled with `*store.Store`, `*gitutil.Repo`
//! and the HTTP harness. Here they are extracted into `CircuitBreaker`, a plain
//! struct with no I/O, so each one can be driven directly by a test.
//!
//! The contract with `worker.rs`:
//!
//! 1. the worker observes reality and calls the matching `note_*` / `record_*`
//!    method,
//! 2. then asks `evaluate(...)` what to do this turn,
//! 3. and obeys the returned [`Verdict`].
//!
//! Anything that touches the store, git or the network lives in the worker, not
//! here — that separation is what makes the breakers portable and reviewable.

use std::time::Duration;

use swarm_config::RunConfig;

/// Identifies one breaker, so a test (and a log line) can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardId {
    /// 1 — skip a turn when nothing is claimable and I hold nothing.
    IdleSuppression,
    /// 2 — stop the whole run when the token budget is gone.
    BudgetGate,
    /// 3 — retire after the absolute turn ceiling.
    HardTurns,
    /// 4 — retire a worker that never claims and never submits.
    NoClaimNoSubmit,
    /// 5 — worktree conflict / redo.
    WorktreeConflict,
    /// 6 — objective idle detection.
    IdleDetection,
    /// 7 — park a worker that makes no submission for N turns.
    NoProgress,
    /// 8 — park a worker whose output fingerprint never changes.
    EmptyOutput,
    /// 9 — release an item held too long / too expensively.
    ItemHold,
    /// 10 — back off on 503.
    RateLimitBackoff,
    /// 11 — autoscaler gives up after repeated stalled spawns.
    AutoscalerGiveUp,
    /// 12 — resource ceiling.
    ResourceGuard,
    /// 13 — exit when the seed work is terminal.
    ExitWhenDone,
    /// 14 — silence timeout and hard wall clock.
    RunCeiling,
}

impl GuardId {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardId::IdleSuppression => "idle-suppression",
            GuardId::BudgetGate => "budget-gate",
            GuardId::HardTurns => "hard-turns",
            GuardId::NoClaimNoSubmit => "no-claim-no-submit",
            GuardId::WorktreeConflict => "worktree-conflict",
            GuardId::IdleDetection => "idle-detection",
            GuardId::NoProgress => "no-progress",
            GuardId::EmptyOutput => "empty-output",
            GuardId::ItemHold => "item-hold",
            GuardId::RateLimitBackoff => "rate-limit-backoff",
            GuardId::AutoscalerGiveUp => "autoscaler-give-up",
            GuardId::ResourceGuard => "resource-guard",
            GuardId::ExitWhenDone => "exit-when-done",
            GuardId::RunCeiling => "run-ceiling",
        }
    }
}

/// What the worker should do this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Proceed with the turn.
    Continue,
    /// Skip this turn and sleep; the turn budget is NOT consumed.
    Skip { reason: String, sleep: Duration },
    /// The worker is finished: leave the pool.
    Retire { reason: String, guard: GuardId },
    /// The worker is broken: park it (its claims are released).
    Park { reason: String, guard: GuardId },
    /// The whole run is over.
    Stop { reason: String, guard: GuardId },
}

impl Verdict {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Verdict::Retire { .. } | Verdict::Park { .. } | Verdict::Stop { .. }
        )
    }
}

/// Everything the breakers need to know about the world right now.
///
/// The worker fills this in from the store and its own state; the breakers only
/// read it. Keeping it a plain struct means a test can describe any situation
/// without standing up a store.
#[derive(Debug, Clone, Default)]
pub struct World {
    /// Items a worker could claim right now.
    pub claimable: i64,
    /// Claims this worker holds.
    pub my_claims: usize,
    /// Non-terminal items on the board.
    pub open: i64,
    /// Total items ever created (0 = cold start).
    pub total_items: i64,
    /// Whether the board has anything at all.
    pub cold_start: bool,
    /// Tokens spent so far, run-wide.
    pub tokens_spent: i64,
    /// Tokens spent on the item currently held.
    pub held_item_tokens: i64,
    /// Active (non-retired, non-crashed) workers.
    pub active_workers: usize,
    /// Live lease still held on the item I want to release.
    pub held_item_live: bool,
    /// The reply contained "IDLE".
    pub reply_says_idle: bool,
    /// A submission happened during this turn.
    pub submitted_this_turn: bool,
    /// Whether this turn's harness call succeeded.
    pub turn_ok: bool,
    /// HTTP status of a failed harness call, if any.
    pub http_status: Option<u16>,
    /// Host CPU utilisation percent.
    pub cpu_pct: f64,
    /// Host memory utilisation percent.
    pub mem_pct: f64,
    /// Seed items still open.
    pub seed_open: i64,
    /// Seed items total.
    pub seed_total: i64,
    /// Submissions waiting for the integrator.
    pub queued_submissions: usize,
}

/// Whether a hold has outrun the configured clock (guard 9's time criterion).
///
/// Factored out of [`CircuitBreaker::should_release_held_item`] so it can be
/// tested directly. It exists as a named function because the inline version had
/// a subtle property worth making explicit: `max_item_seconds == 0` disables the
/// criterion entirely rather than meaning "zero seconds allowed".
pub fn hold_outran_clock(held_for: Duration, cfg: &RunConfig) -> bool {
    if cfg.max_item_seconds == 0 {
        return false;
    }
    held_for > Duration::from_secs(cfg.max_item_seconds as u64)
}

/// Per-worker breaker state.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    cfg: RunConfig,

    // ---- worker-scoped counters ----
    turn: u32,
    idle_count: u32,
    sync_skew: u32,
    redo_count: u32,
    no_progress_streak: u32,
    empty_artifact_streak: u32,
    rate_limited_streak: u32,
    /// Item currently held and for how many turns.
    held_item: Option<HeldItem>,
    /// Previous output fingerprint (see `note_artifact`).
    prev_artifact: String,
    reap_fail_count: u32,

    // ---- run-scoped counters ----
    /// Consecutive spawns that produced nothing (feeds guard 11).
    pub stalled_spawns: u32,
    /// Wall clock of the run, for the reaper's exponential backoff.
    pub started_at: std::time::Instant,
}

/// One item a worker is holding.
#[derive(Debug, Clone)]
pub struct HeldItem {
    pub item_id: String,
    pub turns: u32,
    pub held_for: Duration,
}

impl CircuitBreaker {
    pub fn new(cfg: RunConfig) -> Self {
        Self {
            cfg,
            turn: 0,
            idle_count: 0,
            sync_skew: 0,
            redo_count: 0,
            no_progress_streak: 0,
            empty_artifact_streak: 0,
            rate_limited_streak: 0,
            held_item: None,
            prev_artifact: String::new(),
            reap_fail_count: 0,
            stalled_spawns: 0,
            started_at: std::time::Instant::now(),
        }
    }

    /// The config these breakers were built from.
    pub fn config(&self) -> &RunConfig {
        &self.cfg
    }

    /// Current turn number.
    pub fn turn(&self) -> u32 {
        self.turn
    }

    /// How long the run has been going.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    // ------------------------------------------------------------- observers

    /// Count of consecutive turns with no submission and no idle.
    pub fn no_progress_streak(&self) -> u32 {
        self.no_progress_streak
    }

    /// Count of consecutive turns with a static output fingerprint.
    pub fn empty_artifact_streak(&self) -> u32 {
        self.empty_artifact_streak
    }

    /// Consecutive 503s, for the backoff exponent.
    pub fn rate_limited_streak(&self) -> u32 {
        self.rate_limited_streak
    }

    /// The item this worker holds, if any.
    pub fn held_item(&self) -> Option<&HeldItem> {
        self.held_item.as_ref()
    }

    /// Reaper backoff for this worker: `10s * 2^min(fails,5)`, capped at 320s.
    pub fn reap_backoff(&self) -> Duration {
        let exp = self.reap_fail_count.min(5);
        Duration::from_secs(10 * (1u64 << exp))
    }

    // ---------------------------------------------------------------- events

    /// A turn is starting.
    pub fn begin_turn(&mut self) {
        self.turn += 1;
    }

    /// A turn was rolled back (rate-limit backoff consumes nothing).
    pub fn rollback_turn(&mut self) {
        self.turn = self.turn.saturating_sub(1);
    }

    /// The harness call finished.
    ///
    /// Takes only what it uses. An earlier version also accepted
    /// `reply_says_idle`, `open` and `my_claims` and then discarded all three
    /// with `let _ = (...)` — three dead parameters that made the call site look
    /// like it was feeding the guard more than it was.
    pub fn note_turn_result(&mut self, submitted: bool, idle: bool) {
        if idle {
            self.idle_count += 1;
            self.no_progress_streak = 0;
            return;
        }
        self.idle_count = 0;
        // Guard 7. Only a real (non-echo) harness with a successful turn counts:
        // a turn that ends with no submission and is not idle means the endpoint
        // is not driving the tools.
        if submitted {
            self.no_progress_streak = 0;
        } else {
            self.no_progress_streak += 1;
        }
    }

    /// Record the output fingerprint for guard 8.
    ///
    /// A static (or empty) fingerprint across turns means the model is talking,
    /// not working.
    ///
    /// The first NON-EMPTY observation only establishes the baseline, so a
    /// worker that produces `f3+10-2` on turn 1 and again on turn 2 is on its
    /// first repeat, not its second. An empty fingerprint is never a baseline:
    /// it means "nothing was produced", which is a stall from the start.
    pub fn note_artifact(&mut self, fingerprint: &str) {
        if fingerprint.is_empty() {
            self.empty_artifact_streak += 1;
            return;
        }
        if self.prev_artifact.is_empty() {
            // Baseline: remember it and stay at zero.
            self.prev_artifact = fingerprint.to_string();
            self.empty_artifact_streak = 0;
            return;
        }
        if fingerprint == self.prev_artifact {
            self.empty_artifact_streak += 1;
        } else {
            self.empty_artifact_streak = 0;
        }
        self.prev_artifact = fingerprint.to_string();
    }

    /// Note which item this worker now holds (or none).
    pub fn note_held_item(&mut self, item_id: Option<String>) {
        match (item_id, &mut self.held_item) {
            (Some(id), Some(h)) if h.item_id == id => h.turns += 1,
            (Some(id), _) => {
                self.held_item = Some(HeldItem {
                    item_id: id,
                    turns: 1,
                    held_for: Duration::ZERO,
                })
            }
            (None, _) => self.held_item = None,
        }
    }

    /// Advance the hold clock for the currently held item.
    pub fn advance_hold(&mut self, d: Duration) {
        if let Some(h) = self.held_item.as_mut() {
            h.held_for += d;
        }
    }

    /// A sync conflict happened.
    pub fn note_sync_conflict(&mut self) {
        self.sync_skew += 1;
    }

    /// A sync succeeded.
    pub fn note_sync_ok(&mut self) {
        self.sync_skew = 0;
    }

    /// A redo (worktree reset) happened.
    pub fn note_redo(&mut self) {
        self.redo_count += 1;
    }

    /// A 503 was observed (guard 10).
    pub fn note_rate_limit(&mut self) {
        self.rate_limited_streak += 1;
    }

    /// A successful turn clears the rate-limit streak.
    pub fn note_ok(&mut self) {
        self.rate_limited_streak = 0;
    }

    /// A worktree removal failed; bumps the reaper backoff.
    pub fn note_reap_failure(&mut self) {
        self.reap_fail_count += 1;
    }

    /// The worker just claimed something: the swarm IS making progress, so the
    /// give-up guard resets.
    pub fn note_claim_taken(&mut self) {
        self.stalled_spawns = 0;
    }

    /// A spawn retired without ever claiming or submitting.
    pub fn note_stalled_spawn(&mut self) {
        self.stalled_spawns += 1;
    }

    // ------------------------------------------------------------- decisions

    /// Guard 1: should this turn be skipped entirely?
    pub fn should_suppress_idle(&self, w: &World) -> Option<Verdict> {
        if w.claimable > 0 || w.my_claims > 0 || w.cold_start {
            return None;
        }
        // Guard 6's retire condition lives here too: nothing open, idle budget
        // spent, and we are not the last worker.
        if w.open == 0 && self.idle_count >= self.cfg.idle_turns_to_exit
            && w.active_workers > self.cfg.min_workers as usize
        {
            return Some(Verdict::Retire {
                reason: format!(
                    "no claimable work for {} idle turns",
                    self.idle_count
                ),
                guard: GuardId::IdleDetection,
            });
        }
        Some(Verdict::Skip {
            reason: "nothing claimable and no active claim".to_string(),
            sleep: Duration::from_secs(1),
        })
    }

    /// Guards 2, 3, 4 — checked before a turn starts.
    pub fn evaluate_pre_turn(&self, w: &World) -> Option<Verdict> {
        // Guard 2: budget.
        if self.cfg.budget_tokens > 0 && w.tokens_spent > self.cfg.budget_tokens {
            return Some(Verdict::Stop {
                reason: format!(
                    "budget exhausted: {} > {}",
                    w.tokens_spent, self.cfg.budget_tokens
                ),
                guard: GuardId::BudgetGate,
            });
        }

        // Guard 3: absolute turn ceiling.
        if self.turn > self.cfg.hard_max_turns() {
            return Some(Verdict::Retire {
                reason: format!("turn {} over hard ceiling {}", self.turn, self.cfg.hard_max_turns()),
                guard: GuardId::HardTurns,
            });
        }

        // Retire early once there is no open work and the soft budget is spent.
        if self.turn > self.cfg.max_turns && w.open == 0 {
            return Some(Verdict::Retire {
                reason: format!("no open work at turn {}", self.turn),
                guard: GuardId::HardTurns,
            });
        }

        // Guard 4: never claimed, never submitted.
        if self.held_item.is_none() && self.turn > self.cfg.max_item_turns {
            return Some(Verdict::Retire {
                reason: format!(
                    "no claim and no submission in {} turns",
                    self.turn
                ),
                guard: GuardId::NoClaimNoSubmit,
            });
        }

        None
    }

    /// Guards 7 and 8 — checked after a turn's result is recorded.
    pub fn evaluate_post_turn(&self, is_echo_harness: bool, turn_ok: bool) -> Option<Verdict> {
        // Guard 7. Echo harnesses never submit by construction, so the guard is
        // skipped for them.
        if !is_echo_harness && turn_ok && self.no_progress_streak >= self.cfg.max_no_progress_turns {
            return Some(Verdict::Park {
                reason: format!(
                    "{} turns with no submission (endpoint likely ignores tool_choice)",
                    self.no_progress_streak
                ),
                guard: GuardId::NoProgress,
            });
        }

        // Guard 8. Turn-based, matching the original intent (see
        // docs/OPTIMIZATION-SWARM.md §1.2 for why the Go version's 18-second
        // time threshold mis-parks slow models).
        //
        // The threshold is `maxNoProgressTurns` REPEATS after the baseline, so
        // the Go message "3 turns with no new output" means: baseline turn, then
        // three unchanged turns.
        if self.empty_artifact_streak >= self.cfg.max_no_progress_turns {
            return Some(Verdict::Park {
                reason: format!(
                    "{} turns with no new output (fingerprint static)",
                    self.empty_artifact_streak
                ),
                guard: GuardId::EmptyOutput,
            });
        }

        None
    }

    /// Guard 9 — should the held item be released back to the pool?
    ///
    /// Any of three conditions fires: too many turns, too many tokens, or too
    /// long on the wall clock. The work already done is kept; the item goes back
    /// to `pending` with a FAIL note so the next worker does not repeat it.
    pub fn should_release_held_item(&self, w: &World) -> Option<String> {
        let h = self.held_item.as_ref()?;

        if h.turns > self.cfg.max_item_turns {
            return Some(format!(
                "held {} > {} turns limit",
                h.turns, self.cfg.max_item_turns
            ));
        }
        if w.held_item_tokens > self.cfg.max_item_tokens {
            return Some(format!(
                "item used {} > {} tokens",
                w.held_item_tokens, self.cfg.max_item_tokens
            ));
        }
        if hold_outran_clock(h.held_for, &self.cfg) {
            return Some(format!(
                "held {:?} > {}s limit",
                h.held_for, self.cfg.max_item_seconds
            ));
        }
        None
    }

    /// Guard 10 — how long to back off after a 503.
    ///
    /// 5s doubling, capped at 60s. The caller must roll the turn back so the
    /// budget is not consumed.
    ///
    /// The exponent is `streak - 1` because the FIRST 503 backs off 5s, not 10s
    /// (Go L904: `5000 * (1 << (streak-1))`).
    pub fn rate_limit_backoff(&self) -> Duration {
        let exp = self.rate_limited_streak.saturating_sub(1).min(4);
        let ms = 5000u64 * (1u64 << exp);
        Duration::from_millis(ms.min(60_000))
    }

    /// Guard 5 — what to do about a worktree sync conflict.
    pub fn evaluate_sync_conflict(&self) -> Option<Verdict> {
        if self.sync_skew >= 8 {
            return Some(Verdict::Retire {
                reason: format!("sync skew {} >= 8", self.sync_skew),
                guard: GuardId::WorktreeConflict,
            });
        }
        if self.sync_skew >= 3 {
            if self.redo_count >= 2 {
                return Some(Verdict::Retire {
                    reason: format!("redo budget spent (skew {}, redo {})", self.sync_skew, self.redo_count),
                    guard: GuardId::WorktreeConflict,
                });
            }
            // The worker performs a hard reset and continues.
            return None;
        }
        None
    }

    /// Whether a worktree reset should happen this turn (guard 5's redo path).
    pub fn should_reset_worktree(&self) -> bool {
        self.sync_skew >= 3 && self.redo_count < 2
    }

    /// Guard 6 — is this turn objectively idle?
    ///
    /// Idle is an OBJECTIVE signal, not something the model has to announce:
    /// no open items AND no claim held by this worker. The string match is a
    /// second, independent trigger.
    pub fn is_idle(&self, w: &World) -> bool {
        w.reply_says_idle || (w.open == 0 && w.my_claims == 0)
    }

    /// Guard 6 — should the worker leave because there is nothing to do?
    pub fn should_retire_idle(&self, w: &World) -> bool {
        if w.open != 0 || w.my_claims > 0 {
            return false;
        }
        // The last worker leaves immediately instead of spinning out the idle
        // budget.
        if w.active_workers <= self.cfg.min_workers as usize {
            return true;
        }
        self.idle_count >= self.cfg.idle_turns_to_exit
            && w.active_workers > self.cfg.min_workers as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RunConfig {
        RunConfig::default()
    }

    fn world() -> World {
        World {
            claimable: 5,
            my_claims: 0,
            open: 5,
            total_items: 5,
            cold_start: false,
            tokens_spent: 0,
            held_item_tokens: 0,
            active_workers: 4,
            held_item_live: true,
            reply_says_idle: false,
            submitted_this_turn: false,
            turn_ok: true,
            http_status: None,
            cpu_pct: 10.0,
            mem_pct: 10.0,
            seed_open: 0,
            seed_total: 0,
            queued_submissions: 0,
        }
    }

    // ---------------------------------------------------------- guard 1

    #[test]
    fn guard_01_idle_suppression_skips_when_nothing_claimable() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.claimable = 0;
        w.my_claims = 0;
        w.cold_start = false;
        let v = b.should_suppress_idle(&w).unwrap();
        assert!(matches!(v, Verdict::Skip { .. }));
    }

    #[test]
    fn guard_01_cold_start_never_suppresses() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.claimable = 0;
        w.my_claims = 0;
        w.total_items = 0;
        w.cold_start = true;
        assert!(
            b.should_suppress_idle(&w).is_none(),
            "an empty board is not idleness"
        );
    }

    #[test]
    fn guard_01_own_claim_prevents_suppression() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.claimable = 0;
        w.my_claims = 1;
        assert!(b.should_suppress_idle(&w).is_none());
    }

    // ---------------------------------------------------------- guard 2

    #[test]
    fn guard_02_budget_stops_the_run() {
        let mut c = cfg();
        c.budget_tokens = 1000;
        let b = CircuitBreaker::new(c);
        let mut w = world();
        w.tokens_spent = 1001;
        let v = b.evaluate_pre_turn(&w).unwrap();
        assert!(matches!(v, Verdict::Stop { guard: GuardId::BudgetGate, .. }));
    }

    #[test]
    fn guard_02_zero_budget_never_fires() {
        let c = cfg();
        assert_eq!(c.budget_tokens, 0, "default is unlimited");
        let b = CircuitBreaker::new(c);
        let mut w = world();
        w.tokens_spent = 999_999_999;
        assert!(b.evaluate_pre_turn(&w).is_none());
    }

    // ---------------------------------------------------------- guard 3

    #[test]
    fn guard_03_hard_turn_ceiling_retires() {
        let mut b = CircuitBreaker::new(cfg());
        // maxTurns 10 * hardTurnFactor 6 = 60.
        for _ in 0..61 {
            b.begin_turn_dummy();
        }
        let v = b.evaluate_pre_turn(&world()).unwrap();
        assert!(matches!(v, Verdict::Retire { guard: GuardId::HardTurns, .. }));
    }

    #[test]
    fn guard_03_no_open_work_retires_after_soft_budget() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..11 {
            b.begin_turn_dummy();
        }
        let mut w = world();
        w.open = 0;
        let v = b.evaluate_pre_turn(&w).unwrap();
        assert!(matches!(v, Verdict::Retire { .. }));
    }

    // ---------------------------------------------------------- guard 4

    #[test]
    fn guard_04_no_claim_no_submit_retires() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..7 {
            b.begin_turn_dummy();
        }
        // maxItemTurns is 6, so turn 7 with no claim retires.
        let v = b.evaluate_pre_turn(&world()).unwrap();
        assert!(matches!(v, Verdict::Retire { guard: GuardId::NoClaimNoSubmit, .. }));
    }

    #[test]
    fn guard_04_holding_an_item_survives() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..20 {
            b.begin_turn_dummy();
        }
        b.note_held_item(Some("i1".into()));
        assert!(
            b.evaluate_pre_turn(&world()).is_none(),
            "a worker holding an item is not stalled"
        );
    }

    // ---------------------------------------------------------- guard 5

    #[test]
    fn guard_05_sync_skew_retires_at_eight() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..8 {
            b.note_sync_conflict();
        }
        let v = b.evaluate_sync_conflict().unwrap();
        assert!(matches!(v, Verdict::Retire { guard: GuardId::WorktreeConflict, .. }));
    }

    #[test]
    fn guard_05_redo_resets_at_three() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            b.note_sync_conflict();
        }
        assert!(b.should_reset_worktree(), "skew 3 triggers a redo");
        b.note_redo();
        b.note_redo();
        assert!(!b.should_reset_worktree(), "redo budget spent");
        let v = b.evaluate_sync_conflict().unwrap();
        assert!(matches!(v, Verdict::Retire { .. }));
    }

    #[test]
    fn guard_05_sync_ok_clears_skew() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..2 {
            b.note_sync_conflict();
        }
        b.note_sync_ok();
        assert_eq!(b.evaluate_sync_conflict(), None);
    }

    // ---------------------------------------------------------- guard 6

    #[test]
    fn guard_06_objective_idle_needs_no_announcement() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.open = 0;
        w.my_claims = 0;
        w.reply_says_idle = false;
        assert!(b.is_idle(&w), "empty board + no claim is idle, silently");
    }

    #[test]
    fn guard_06_reply_idle_alone_is_enough() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.reply_says_idle = true;
        assert!(b.is_idle(&w));
    }

    #[test]
    fn guard_06_last_worker_leaves_immediately() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.open = 0;
        w.my_claims = 0;
        w.active_workers = 1;
        assert!(b.should_retire_idle(&w));
    }

    #[test]
    fn guard_06_non_last_worker_waits_for_idle_budget() {
        let b = CircuitBreaker::new(cfg());
        let mut w = world();
        w.open = 0;
        w.my_claims = 0;
        w.active_workers = 4;
        assert!(
            !b.should_retire_idle(&w),
            "must burn the idle budget first"
        );
    }

    // ---------------------------------------------------------- guard 7

    #[test]
    fn guard_07_no_progress_parks_after_three_turns() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            b.note_turn_result(false, false);
        }
        assert_eq!(b.no_progress_streak(), 3);
        let v = b.evaluate_post_turn(false, true).unwrap();
        assert!(matches!(v, Verdict::Park { guard: GuardId::NoProgress, .. }));
    }

    #[test]
    fn guard_07_submission_resets_the_streak() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_turn_result(false, false);
        b.note_turn_result(false, false);
        b.note_turn_result(true, false);
        assert_eq!(b.no_progress_streak(), 0);
    }

    #[test]
    fn guard_07_idle_resets_the_streak() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_turn_result(false, false);
        b.note_turn_result(false, true);
        assert_eq!(b.no_progress_streak(), 0);
    }

    /// A failed turn is not evidence of "the endpoint ignores tool_choice".
    ///
    /// The runner used to pass a literal `true` for `turn_ok`, so a transport
    /// error — the endpoint unreachable, a malformed request — was counted as a
    /// no-progress turn and parked the worker, losing whatever it had done.
    /// `turn_ok: false` must exempt the guard entirely.
    #[test]
    fn guard_07_a_failed_turn_is_not_no_progress() {
        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..10 {
            b.note_turn_result(false, false);
        }
        assert_eq!(b.no_progress_streak(), 10);
        assert!(
            b.evaluate_post_turn(false, false).is_none(),
            "a failed harness call must not park the worker"
        );
        // ...but the same state with a healthy turn does fire.
        assert!(b.evaluate_post_turn(false, true).is_some());
    }

    #[test]
    fn guard_07_echo_harness_is_exempt() {        let mut b = CircuitBreaker::new(cfg());
        for _ in 0..10 {
            b.note_turn_result(false, false);
        }
        assert!(
            b.evaluate_post_turn(true, true).is_none(),
            "echo harnesses never submit by construction"
        );
    }

    // ---------------------------------------------------------- guard 8

    #[test]
    fn guard_08_static_fingerprint_parks_after_three_repeats() {
        let mut b = CircuitBreaker::new(cfg());
        // Turn 1 establishes the baseline (no stall counted).
        b.note_artifact("f3+10-2");
        assert_eq!(b.empty_artifact_streak(), 0);
        // Turns 2-4 repeat it: that is three turns with no new output.
        b.note_artifact("f3+10-2");
        b.note_artifact("f3+10-2");
        b.note_artifact("f3+10-2");
        assert_eq!(b.empty_artifact_streak(), 3);
        let v = b.evaluate_post_turn(false, true).unwrap();
        assert!(matches!(v, Verdict::Park { guard: GuardId::EmptyOutput, .. }));
    }

    #[test]
    fn guard_08_changing_fingerprint_resets() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_artifact("f1+1-0"); // baseline
        b.note_artifact("f2+3-0"); // changed
        b.note_artifact("f2+3-0"); // first repeat
        b.note_artifact("f2+3-0"); // second repeat
        assert_eq!(b.empty_artifact_streak(), 2);
        b.note_artifact("f9+9-9"); // changed again
        assert_eq!(b.empty_artifact_streak(), 0);
    }

    #[test]
    fn guard_08_empty_fingerprint_counts_as_static() {
        let mut b = CircuitBreaker::new(cfg());
        // An empty fingerprint is never a baseline: it stalls immediately.
        b.note_artifact("");
        b.note_artifact("");
        b.note_artifact("");
        assert_eq!(b.empty_artifact_streak(), 3);
    }

    /// The Go version uses an 18-SECOND threshold and mis-parks slow models
    /// (docs/OPTIMIZATION-SWARM.md §1.2). Ours is turn-based.
    #[test]
    fn guard_08_is_turn_based_not_time_based() {
        let mut b = CircuitBreaker::new(cfg());
        // Three turns with no new output, but each taking "minutes".
        b.note_artifact("same"); // baseline
        for _ in 0..3 {
            b.advance_hold(Duration::from_secs(300));
            b.note_artifact("same");
        }
        assert_eq!(b.empty_artifact_streak(), 3);

        // Two turns must NOT park, however long they took.
        let mut b2 = CircuitBreaker::new(cfg());
        b2.note_artifact("same"); // baseline
        b2.advance_hold(Duration::from_secs(600));
        b2.note_artifact("same"); // one repeat
        b2.advance_hold(Duration::from_secs(600));
        b2.note_artifact("same"); // two repeats
        assert_eq!(b2.empty_artifact_streak(), 2);
        assert!(b2.evaluate_post_turn(false, true).is_none());
    }

    // ---------------------------------------------------------- guard 9

    #[test]
    fn guard_09_releases_after_max_item_turns() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_held_item(Some("i1".into()));
        for _ in 0..7 {
            b.note_held_item(Some("i1".into()));
        }
        assert!(b.should_release_held_item(&world()).is_some());
    }

    #[test]
    fn guard_09_releases_after_token_limit() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_held_item(Some("i1".into()));
        let mut w = world();
        w.held_item_tokens = 2_000_001;
        let reason = b.should_release_held_item(&w).unwrap();
        assert!(reason.contains("tokens"), "got: {reason}");
    }

    #[test]
    fn guard_09_releases_after_wall_clock_limit() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_held_item(Some("i1".into()));
        b.advance_hold(Duration::from_secs(301));
        let reason = b.should_release_held_item(&world()).unwrap();
        assert!(reason.contains("limit"), "got: {reason}");
    }

    #[test]
    fn guard_09_zero_max_item_seconds_disables_the_clock() {
        let mut c = cfg();
        c.max_item_seconds = 0;
        let mut b = CircuitBreaker::new(c);
        b.note_held_item(Some("i1".into()));
        b.advance_hold(Duration::from_secs(100_000));
        assert!(
            b.should_release_held_item(&world()).is_none(),
            "0 must disable the wall-clock breaker"
        );
    }

    // --------------------------------------------------------- guard 10

    #[test]
    fn guard_10_backoff_doubles_and_caps() {
        let mut b = CircuitBreaker::new(cfg());
        // streak 0 (nothing seen yet) → the first backoff is 5s.
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(5));
        b.note_rate_limit(); // streak 1 → 5s
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(5));
        b.note_rate_limit(); // streak 2 → 10s
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(10));
        b.note_rate_limit(); // streak 3 → 20s
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(20));
        b.note_rate_limit(); // streak 4 → 40s
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(40));
        for _ in 0..10 {
            b.note_rate_limit();
        }
        assert_eq!(b.rate_limit_backoff(), Duration::from_secs(60), "capped");
    }

    #[test]
    fn guard_10_rollback_does_not_consume_turn_budget() {
        let mut b = CircuitBreaker::new(cfg());
        b.begin_turn_dummy();
        assert_eq!(b.turn(), 1);
        b.rollback_turn();
        assert_eq!(b.turn(), 0);
    }

    // --------------------------------------------------------- guard 11

    #[test]
    fn guard_11_gives_up_after_three_stalled_spawns() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_stalled_spawn();
        b.note_stalled_spawn();
        b.note_stalled_spawn();
        assert_eq!(b.stalled_spawns, 3);
    }

    #[test]
    fn guard_11_claim_taken_resets_the_give_up_guard() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_stalled_spawn();
        b.note_stalled_spawn();
        b.note_claim_taken();
        assert_eq!(b.stalled_spawns, 0);
    }

    // --------------------------------------------------------- misc

    #[test]
    fn reaper_backoff_doubles_and_caps_at_320s() {
        let mut b = CircuitBreaker::new(cfg());
        assert_eq!(b.reap_backoff(), Duration::from_secs(10));
        b.note_reap_failure();
        assert_eq!(b.reap_backoff(), Duration::from_secs(20));
        for _ in 0..10 {
            b.note_reap_failure();
        }
        assert_eq!(b.reap_backoff(), Duration::from_secs(320));
    }

    #[test]
    fn held_item_turns_increment_only_for_the_same_item() {
        let mut b = CircuitBreaker::new(cfg());
        b.note_held_item(Some("i1".into()));
        b.note_held_item(Some("i1".into()));
        assert_eq!(b.held_item().unwrap().turns, 2);
        b.note_held_item(Some("i2".into()));
        assert_eq!(b.held_item().unwrap().turns, 1, "a new item restarts at 1");
        b.note_held_item(None);
        assert!(b.held_item().is_none());
    }

    #[test]
    fn guard_ids_have_stable_names() {
        assert_eq!(GuardId::NoProgress.as_str(), "no-progress");
        assert_eq!(GuardId::EmptyOutput.as_str(), "empty-output");
        assert_eq!(GuardId::ItemHold.as_str(), "item-hold");
    }

    impl CircuitBreaker {
        fn begin_turn_dummy(&mut self) {
            self.turn += 1;
        }
    }
}

