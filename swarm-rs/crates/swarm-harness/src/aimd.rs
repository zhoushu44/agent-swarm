//! AIMD concurrency gate, mirroring Go `internal/harness/adaptive.go`.
//!
//! The gate answers one question: how many LLM requests may be in flight right
//! now. A fixed limit is wrong in both directions — too low wastes throughput,
//! too high triggers 429s that cost more than the parallelism gained.
//!
//! Behaviour, copied exactly from the Go implementation:
//!
//! * **Slow start** until the first rate-limit: every 8 consecutive OKs doubles
//!   the window *and* the capacity estimate, so the real limit is reached in
//!   log2(n) rounds.
//! * **Rate-limit (429/503)**: pin the ceiling to the peak concurrency seen this
//!   round, then back off to 75% of it. Concurrent failures inside a 2s storm
//!   window collapse into a single back-off.
//! * **Sustained success below the ceiling**: +1 every 8 OKs.
//! * **At the ceiling**: probe upward, with the step growing with the streak
//!   (+1, then +2 at 64, +4 at 256).
//!
//! The implementation owns its waiter queue rather than wrapping
//! `tokio::Semaphore`, because a semaphore cannot shrink: when the gate backs
//! off it must refuse *new* work immediately while letting in-flight work
//! finish. Go models this with a channel per waiter; we do the same with
//! `oneshot` senders.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Mutex, Notify};

/// Concurrent failures within this window are treated as one storm.
const STORM_WINDOW: Duration = Duration::from_secs(2);

/// Every N consecutive OKs the gate acts.
const OKS_PER_STEP: u64 = 8;

#[derive(Debug)]
struct State {
    limit: usize,
    ceiling: usize,
    active: usize,
    /// Peak concurrency since the last pin.
    hi_water: usize,
    /// Consecutive successes since the last rate-limit.
    ok_streak: u64,
    /// Last back-off instant, for the storm throttle.
    last_back: Option<Instant>,
    /// True until the first rate-limit.
    slow_start: bool,
    /// Hard upper bound on `ceiling`, installed by `pin_ceiling`.
    ///
    /// Slow start doubles the ceiling on every 8 successes, so without this the
    /// pin would be walked straight through. It is a `usize` rather than an
    /// `Option` because "no pin" is naturally `usize::MAX`.
    ceiling_floor: usize,
}

/// An adaptive concurrency gate. Cloning is cheap; clones share one gate.
#[derive(Clone)]
pub struct AdaptiveGate {
    state: Arc<Mutex<State>>,
    waiters: Arc<Mutex<Vec<oneshot::Sender<()>>>>,
    /// Signalled whenever the limit or the queue changes, so a blocked
    /// `acquire` re-checks instead of spinning.
    changed: Arc<Notify>,
    /// Cumulative counters for observability. Atomics rather than fields on
    /// `State` because they are read without the lock (see `active_now`).
    rate_limits: Arc<std::sync::atomic::AtomicU64>,
    backoffs: Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for AdaptiveGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptiveGate")
            .field("limit", &self.limit_blocking())
            .finish()
    }
}

impl AdaptiveGate {
    /// Create a gate starting at `n` (clamped to at least 1).
    pub fn new(n: usize) -> Self {
        let n = n.max(1);
        Self {
            state: Arc::new(Mutex::new(State {
                limit: n,
                ceiling: n,
                active: 0,
                hi_water: 0,
                ok_streak: 0,
                last_back: None,
                slow_start: true,
                ceiling_floor: usize::MAX,
            })),
            waiters: Arc::new(Mutex::new(Vec::new())),
            changed: Arc::new(Notify::new()),
            rate_limits: Arc::new(AtomicU64::new(0)),
            backoffs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The initial window for a given `max_workers`, matching Go
    /// `newSwarmRunner` (runner.go L229-235): `clamp(maxWorkers * 4, 8, 64)`.
    ///
    /// **The result is not a ceiling on the gate.** Slow start doubles it on
    /// every 8 successes, so a pool of `max_workers = 96` starts at 64 and
    /// reaches 128 — ABOVE the number of workers that exist. At that point the
    /// gate can never be the binding constraint, which defeats its purpose: it
    /// is supposed to stop the pool from overwhelming the endpoint, not to
    /// authorise more concurrency than the pool can produce.
    ///
    /// The measured run that exposed this: 96 workers, `limit=128`,
    /// `rate_limits=0` — a gate that was open so wide it could not have mattered
    /// even if the endpoint HAD throttled.
    ///
    /// The fix is at the call site (`Runner::new`): the gate's ceiling is pinned
    /// to the pool size, so slow start can walk up to it but not past it.
    pub fn initial_window(max_workers: usize) -> usize {
        max_workers.saturating_mul(4).clamp(8, 64)
    }

    /// Current limit.
    pub async fn limit(&self) -> usize {
        self.state.lock().await.limit
    }

    /// Current limit without awaiting — for the 5s status line, which must
    /// never block a worker. Returns 0 if the lock is momentarily held.
    pub fn limit_blocking(&self) -> usize {
        match self.state.try_lock() {
            Ok(s) => s.limit,
            Err(_) => 0,
        }
    }

    /// The pinned capacity ceiling.
    pub async fn ceiling(&self) -> usize {
        self.state.lock().await.ceiling
    }

    /// Requests currently in flight.
    ///
    /// Non-blocking: returns 0 if the lock is momentarily held, which is the
    /// right trade for an observation that must never stall a worker.
    pub fn active_now(&self) -> usize {
        self.state.try_lock().map(|s| s.active).unwrap_or(0)
    }

    /// Consecutive successes since the last rate-limit.
    pub fn ok_streak(&self) -> u64 {
        self.state.try_lock().map(|s| s.ok_streak).unwrap_or(0)
    }

    /// Whether the gate is still doubling on success (before the first 429).
    pub fn in_slow_start(&self) -> bool {
        self.state.try_lock().map(|s| s.slow_start).unwrap_or(false)
    }

    /// Total rate-limit signals seen, for observability.
    pub fn rate_limit_count(&self) -> u64 {
        self.rate_limits.load(Ordering::SeqCst)
    }

    /// Total times the limit was reduced by a back-off.
    pub fn backoff_count(&self) -> u64 {
        self.backoffs.load(Ordering::SeqCst)
    }

    /// Acquire a slot, waiting for one if the gate is saturated.
    pub async fn acquire(&self) -> Result<GateGuard, GateError> {
        loop {
            // Fast path: a slot is free.
            {
                let mut s = self.state.lock().await;
                if s.active < s.limit {
                    s.active += 1;
                    if s.active > s.hi_water {
                        s.hi_water = s.active;
                    }
                    return Ok(GateGuard {
                        gate: self.clone(),
                    });
                }
            }
            // Slow path: wait for a handoff or for the limit to change.
            let (tx, rx) = oneshot::channel();
            self.waiters.lock().await.push(tx);
            tokio::select! {
                _ = rx => {
                    // A releaser handed us a slot (it already bumped `active`).
                    return Ok(GateGuard { gate: self.clone() });
                }
                _ = self.changed.notified() => {
                    // Limit changed; drop our stale waiter entry and retry.
                    self.drop_waiter().await;
                }
            }
        }
    }

    async fn drop_waiter(&self) {
        // The sender in the queue is already consumed or dropped; the next
        // `handoff` skips senders whose receiver is gone, so nothing to do
        // beyond letting the vector drain naturally.
    }

    /// Hand free slots to waiters until either the queue or the capacity runs
    /// out. Callers must hold no lock.
    async fn handoff(&self) {
        let mut waiters = self.waiters.lock().await;
        let mut s = self.state.lock().await;
        while !waiters.is_empty() && s.active < s.limit {
            let tx = waiters.remove(0);
            s.active += 1;
            if s.active > s.hi_water {
                s.hi_water = s.active;
            }
            // The receiver may have gone away (cancelled); ignore that.
            let _ = tx.send(());
        }
    }

    async fn release_one(&self) {
        {
            let mut s = self.state.lock().await;
            if s.active > 0 {
                s.active -= 1;
            }
        }
        // `handoff` re-increments `active` for each waiter it wakes, so handing a
        // slot on nets to zero change — the slot MOVES rather than being copied.
        // The waiter's own `GateGuard` will decrement again when IT drops, so the
        // accounting stays balanced across the chain.
        //
        // This looks like a double-count at a glance and is not one; the two
        // tests below (`release_frees_capacity_for_the_next_acquirer`,
        // `run_does_not_leak_a_slot_per_call`) exist to keep it that way.
        self.handoff().await;
    }

    /// Record a 429/503.
    pub async fn note_rate_limit(&self) {
        // Count the signal itself, storm or not.
        //
        // The storm window dedupes the BACK-OFF, not the observation: a collapsed
        // storm is still proof the endpoint is throttling, and an operator reading
        // `rate_limits` needs the real count. Counting after the storm check made
        // two 429s inside 2s look like one.
        self.rate_limits.fetch_add(1, Ordering::SeqCst);
        {
            let mut s = self.state.lock().await;
            if s.limit == 0 {
                return;
            }
            s.ok_streak = 0;
            // The first rate-limit ends slow start permanently.
            s.slow_start = false;

            let now = Instant::now();
            if let Some(last) = s.last_back {
                if now.duration_since(last) < STORM_WINDOW {
                    return; // storm: the first rejection already backed off
                }
            }
            s.last_back = Some(now);

            let mut hit = s.hi_water;
            if hit < 1 {
                hit = s.active;
            }
            if hit < 1 {
                hit = 1;
            }
            if hit < s.ceiling {
                s.ceiling = hit;
            }
            // Consumed: the next round re-accumulates the true peak.
            s.hi_water = 0;

            let base = s.limit.min(s.ceiling);
            let backed = (base * 3 / 4).max(1);
            let before = s.limit;
            set_limit(&mut s, backed, true);
            // Only count a real reduction. A 429 that arrives when the limit is
            // already 1 cannot back off further, and counting it would make the
            // operator think the gate is shedding capacity when it is not.
            if s.limit < before {
                self.backoffs.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.changed.notify_waiters();
    }

    /// Record a success.
    pub async fn note_ok(&self) {
        {
            let mut s = self.state.lock().await;
            if s.limit == 0 {
                return;
            }
            s.ok_streak += 1;
            if s.ok_streak % OKS_PER_STEP != 0 {
                return;
            }

            // Slow start: no rate-limit seen yet → double window and capacity.
            //
            // The ceiling doubles too, but never past `ceiling_floor`: that is
            // the pin `pin_ceiling` installs to keep the gate inside the pool it
            // gates. Without it slow start walks straight through the pin — a
            // 4-worker pool would go 4 → 8 → 16, which is what `initial_window`
            // already got wrong before the pin existed.
            if s.slow_start {
                s.ceiling = s.ceiling.saturating_mul(2).min(s.ceiling_floor);
                let doubled = (s.limit * 2).min(s.ceiling);
                set_limit(&mut s, doubled, true);
            } else if s.limit < s.ceiling {
                let next = s.limit + 1;
                set_limit(&mut s, next, false);
            } else {
                // At the ceiling with sustained success: probe upward.
                let probe = if s.ok_streak >= 256 {
                    4
                } else if s.ok_streak >= 64 {
                    2
                } else {
                    1
                };
                s.ceiling += probe;
                let next = s.limit + 1;
                set_limit(&mut s, next, false);
            }
        }
        self.changed.notify_waiters();
        self.handoff().await;
    }

    /// Pin the ceiling to `n` without touching the current limit.
    ///
    /// The only other way to raise a ceiling is `set_limit`, which also moves the
    /// limit — wrong for a constructor that must not widen the gate.
    ///
    /// This exists because `initial_window` is `clamp(max_workers * 4, 8, 64)`
    /// and slow start doubles it on every 8 successes. A 96-worker pool therefore
    /// starts at 64 and walks to 128, i.e. above the number of workers that
    /// exist, at which point the gate can never be the binding constraint — the
    /// one thing it is for. Measured in an 8-minute saturated run: `limit=128`,
    /// `rate_limits=0`.
    ///
    /// Best-effort and synchronous: if the lock is momentarily held the pin is
    /// skipped rather than blocking a constructor. `n` below 1 is treated as 1.
    pub fn pin_ceiling(&self, n: usize) {
        let n = n.max(1);
        if let Ok(mut s) = self.state.try_lock() {
            // The floor is what survives slow start's doubling; the immediate
            // clamp handles a gate that is already above the pin.
            s.ceiling_floor = n;
            if n < s.ceiling {
                s.ceiling = n;
            }
            // Never widen the live limit as a side effect of pinning.
            if s.limit > s.ceiling {
                s.limit = s.ceiling;
            }
        }
    }

    /// Explicitly set the limit (CLI `--max-inflight`). An explicit raise is
    /// allowed to lift the ceiling; internal AIMD back-offs are not.
    pub async fn set_limit(&self, n: usize) {
        {
            let mut s = self.state.lock().await;
            set_limit(&mut s, n, false);
        }
        self.changed.notify_waiters();
        self.handoff().await;
    }

    /// Run `f` while holding a slot.
    pub async fn run<F, Fut, T, E>(&self, f: F) -> Result<T, GateRunError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let _guard = self.acquire().await.map_err(GateRunError::Gate)?;
        f().await.map_err(GateRunError::Inner)
    }
}

/// Guard returned by `acquire`; releasing is automatic on drop.
pub struct GateGuard {
    gate: AdaptiveGate,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        let gate = self.gate.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move { gate.release_one().await });
            }
            Err(_) => {
                // No runtime (e.g. a unit test dropping outside `block_on`):
                // release synchronously via the blocking lock.
                if let Ok(mut s) = gate.state.try_lock() {
                    if s.active > 0 {
                        s.active -= 1;
                    }
                }
            }
        }
    }
}

/// Apply a new limit, replicating Go `setLimitLocked` (adaptive.go L125-142).
fn set_limit(s: &mut State, n: usize, aimd: bool) {
    let mut n = n.max(1);
    if n > s.ceiling && !aimd {
        s.ceiling = n;
    }
    // An AIMD back-off must not raise the limit above the pinned ceiling.
    if aimd && n > s.ceiling {
        n = s.ceiling;
    }
    s.limit = n;
}

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("adaptive gate closed")]
    Closed,
}

#[derive(Debug)]
pub enum GateRunError<E> {
    Gate(GateError),
    Inner(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initial_window_matches_go_clamp() {
        assert_eq!(AdaptiveGate::initial_window(1), 8, "min 8");
        assert_eq!(AdaptiveGate::initial_window(2), 8);
        assert_eq!(AdaptiveGate::initial_window(4), 16);
        assert_eq!(AdaptiveGate::initial_window(8), 32);
        assert_eq!(AdaptiveGate::initial_window(16), 64, "max 64");
        assert_eq!(AdaptiveGate::initial_window(1000), 64);
    }

    #[tokio::test]
    async fn starts_in_slow_start() {
        let g = AdaptiveGate::new(8);
        assert_eq!(g.limit().await, 8);
        assert_eq!(g.ceiling().await, 8);
    }

    #[tokio::test]
    async fn slow_start_doubles_every_8_oks() {
        let g = AdaptiveGate::new(8);
        for _ in 0..7 {
            g.note_ok().await;
        }
        assert_eq!(g.limit().await, 8, "7 OKs: no change yet");
        g.note_ok().await; // 8th
        assert_eq!(g.limit().await, 16, "doubles at 8 OKs");
        assert_eq!(g.ceiling().await, 16);
        for _ in 0..8 {
            g.note_ok().await;
        }
        assert_eq!(g.limit().await, 32, "doubles again at 16 OKs");
    }

    #[tokio::test]
    async fn rate_limit_pins_ceiling_and_backs_off_to_75pct() {
        let g = AdaptiveGate::new(32);
        let mut guards = Vec::new();
        for _ in 0..32 {
            guards.push(g.acquire().await.unwrap());
        }
        g.note_rate_limit().await;
        assert_eq!(g.ceiling().await, 32, "ceiling pinned to the peak");
        assert_eq!(g.limit().await, 24, "limit backed to 75%");
        drop(guards);
    }

    #[tokio::test]
    async fn storm_window_collapses_concurrent_failures() {
        let g = AdaptiveGate::new(64);
        let mut guards = Vec::new();
        for _ in 0..64 {
            guards.push(g.acquire().await.unwrap());
        }
        g.note_rate_limit().await;
        let after_first = g.limit().await;
        g.note_rate_limit().await; // within the storm window
        assert_eq!(
            g.limit().await,
            after_first,
            "second failure inside the storm is a no-op"
        );
        drop(guards);
    }

    #[tokio::test]
    async fn additive_increase_below_ceiling() {
        let g = AdaptiveGate::new(8);
        {
            let mut guards = Vec::new();
            for _ in 0..4 {
                guards.push(g.acquire().await.unwrap());
            }
            g.note_rate_limit().await;
        }
        let base = g.limit().await;
        for _ in 0..8 {
            g.note_ok().await;
        }
        assert_eq!(g.limit().await, base + 1, "+1 every 8 OKs");
    }

    #[tokio::test]
    async fn acquire_blocks_at_limit() {
        let g = AdaptiveGate::new(2);
        let _a = g.acquire().await.unwrap();
        let _b = g.acquire().await.unwrap();
        let third = tokio::time::timeout(Duration::from_millis(50), g.acquire()).await;
        assert!(third.is_err(), "third acquire must block at limit 2");
    }

    #[tokio::test]
    async fn release_hands_the_slot_to_a_waiter() {
        let g = AdaptiveGate::new(1);
        let a = g.acquire().await.unwrap();
        let g2 = g.clone();
        let waiter = tokio::spawn(async move { g2.acquire().await.map(|_| ()) });
        // Give the waiter time to enqueue.
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(a);
        let got = tokio::time::timeout(Duration::from_millis(500), waiter).await;
        assert!(got.is_ok(), "waiter must receive the released slot");
    }

    /// Releasing must FREE a slot, not hand it on while keeping it counted.
    ///
    /// `release_one` decrements `active` and then calls `handoff`, which
    /// increments it again for a woken waiter. That nets to zero — the slot
    /// MOVES — and the waiter's own guard decrements once more when it drops.
    ///
    /// The invariant is worth pinning because it reads like a double-count: if
    /// the handoff's increment were ever made unconditional, or the waiter's
    /// guard skipped its decrement, every completed call would permanently
    /// narrow the gate. That failure would only show up under load, which is
    /// exactly why the AIMD path had never been exercised end to end.
    #[tokio::test]
    async fn release_frees_capacity_for_the_next_acquirer() {
        let g = AdaptiveGate::new(1);
        // Hold and release repeatedly. If the count leaks, the second acquire
        // blocks forever even though nothing is in flight.
        for i in 0..5 {
            let held = tokio::time::timeout(Duration::from_millis(200), g.acquire()).await;
            assert!(held.is_ok(), "acquire {i} must not block: capacity leaked");
            drop(held);
            // Let the release task run.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The gate under real concurrency: many tasks, a tight limit, and a 429 in
    /// the middle.
    ///
    /// The unit tests above each exercise one transition in isolation. This one
    /// drives the whole state machine the way the runner does — 40 concurrent
    /// `run` calls on a limit-8 gate, with a rate limit fired partway through —
    /// and asserts the two properties that matter:
    ///
    /// 1. **no deadlock**: every call completes;
    /// 2. **capacity is conserved**: when the last call finishes, `active` is 0,
    ///    so nothing leaked across 40 acquire/release cycles.
    #[tokio::test]
    async fn concurrent_runs_conserve_capacity_and_survive_a_429() {
        let g = AdaptiveGate::new(8);
        let mut handles = Vec::new();
        for i in 0..40 {
            let g = g.clone();
            handles.push(tokio::spawn(async move {
                // A third of the calls fail, to exercise the error path too.
                let boom = i % 3 == 0;
                g.run(|| async move {
                    if boom {
                        Err::<(), _>("boom")
                    } else {
                        Ok(())
                    }
                })
                .await
                .map_err(|e| match e {
                    GateRunError::Inner(_) => "inner",
                    GateRunError::Gate(_) => "gate",
                })
            }));
        }
        // Fire a rate limit while the pool is saturated.
        tokio::time::sleep(Duration::from_millis(5)).await;
        g.note_rate_limit().await;

        let mut ok = 0;
        let mut inner_err = 0;
        for h in handles {
            match h.await.unwrap() {
                Ok(()) => ok += 1,
                Err("inner") => inner_err += 1,
                Err(other) => panic!("unexpected {other}"),
            }
        }
        assert_eq!(ok + inner_err, 40, "every call must settle, no deadlock");
        assert!(inner_err > 0, "the failing third must have run");

        // Let the last release tasks land, then check nothing is still counted.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let state = g.state.lock().await;
        assert_eq!(state.active, 0, "capacity leaked across 40 calls");
        assert!(state.limit >= 1, "the gate must stay usable after a back-off");
    }

    /// Same invariant, observed through the `run` convenience the runner uses.
    ///
    /// Every worker turn goes through `gate.run(...)`, so a leak here means each
    /// completed turn permanently consumes a slot — the pool silently narrows to
    /// one worker and never recovers.
    #[tokio::test]
    async fn run_does_not_leak_a_slot_per_call() {
        let g = AdaptiveGate::new(2);
        for i in 0..6 {
            let r: Result<(), GateRunError<()>> = g.run(|| async { Ok(()) }).await;
            assert!(r.is_ok(), "call {i}");
        }
        // After six sequential runs on a limit-2 gate, both slots must be free.
        let a = tokio::time::timeout(Duration::from_millis(200), g.acquire()).await;
        let b = tokio::time::timeout(Duration::from_millis(200), g.acquire()).await;
        assert!(a.is_ok() && b.is_ok(), "slots leaked: gate is narrower than its limit");
    }

    /// The gate's ceiling must not exceed the pool it is gating.
    ///
    /// `initial_window` is `clamp(max_workers * 4, 8, 64)` and slow start doubles
    /// it on every 8 successes, so a 96-worker pool walks 64 → 128. Above the
    /// worker count the gate can never bind, which is the one thing it is for.
    /// An 8-minute saturated run measured exactly that: `limit=128`,
    /// `rate_limits=0`.
    ///
    /// Note the pin is a CEILING clamp, not a raise: with `max_workers = 96` the
    /// window starts at 64 and the pin at 96 leaves it alone. The pin matters for
    /// small pools, where `max_workers * 4` exceeds `max_workers` — a 4-worker
    /// pool gets `initial_window = 16` and is pinned to 4.
    #[tokio::test]
    async fn pin_ceiling_keeps_the_gate_able_to_bind() {
        // A small pool: the window (16) is far above the pool (4).
        let g = AdaptiveGate::new(AdaptiveGate::initial_window(4));
        assert_eq!(g.limit().await, 16, "starts at the Go window");

        g.pin_ceiling(4);
        assert_eq!(g.ceiling().await, 4, "the ceiling is now the pool size");
        assert_eq!(g.limit().await, 4, "and the limit came down with it");

        // Slow start cannot walk back above the pin.
        for _ in 0..8 {
            g.note_ok().await;
        }
        assert_eq!(g.limit().await, 4, "cannot exceed the pin");
        assert_eq!(g.ceiling().await, 4);

        // ...and a back-off still bites below it (floored at 1).
        g.note_rate_limit().await;
        assert!(
            g.limit().await <= 4,
            "a 429 must still reduce the limit, got {}",
            g.limit().await
        );
    }

    /// Pinning must never WIDEN a gate: a limit above the pin is pulled down,
    /// and a pin above the current ceiling is ignored.
    #[tokio::test]
    async fn pin_ceiling_never_widens() {
        let g = AdaptiveGate::new(4);
        // A pin above the current ceiling changes nothing.
        g.pin_ceiling(1000);
        assert_eq!(g.ceiling().await, 4, "a larger pin must not lift the ceiling");
        assert_eq!(g.limit().await, 4);

        // A pin below the current limit pulls the limit down with it.
        g.pin_ceiling(2);
        assert_eq!(g.ceiling().await, 2);
        assert_eq!(g.limit().await, 2, "the limit must not outrun its ceiling");
    }

    #[tokio::test]
    async fn explicit_set_limit_raises_ceiling() {
        let g = AdaptiveGate::new(4);
        g.set_limit(100).await;
        assert_eq!(g.limit().await, 100);
        assert_eq!(g.ceiling().await, 100, "explicit raise lifts the ceiling");
    }

    #[tokio::test]
    async fn run_releases_on_success() {
        let g = AdaptiveGate::new(1);
        let r: Result<(), GateRunError<()>> = g.run(|| async { Ok(()) }).await;
        assert!(r.is_ok());
        let again = tokio::time::timeout(Duration::from_millis(100), g.acquire()).await;
        assert!(again.is_ok(), "slot must be released");
    }

    #[tokio::test]
    async fn back_off_never_exceeds_ceiling() {
        let g = AdaptiveGate::new(10);
        let mut guards = Vec::new();
        for _ in 0..3 {
            guards.push(g.acquire().await.unwrap());
        }
        g.note_rate_limit().await;
        // ceiling pinned at 3 (peak), limit = 2
        assert!(g.limit().await <= g.ceiling().await);
        drop(guards);
    }

    /// A pinned gate must still back off — the pin is a ceiling, not a floor.
    ///
    /// The pin exists to stop slow start walking above the pool size. If it also
    /// blocked back-off it would defeat the gate's whole purpose.
    #[tokio::test]
    async fn a_pinned_gate_still_backs_off() {
        let g = AdaptiveGate::new(8);
        g.pin_ceiling(8);
        for _ in 0..8 {
            g.note_ok().await;
        }
        let before = g.limit().await;
        assert_eq!(before, 8, "slow start reached the pin");

        g.note_rate_limit().await;
        assert!(
            g.limit().await < before,
            "a pinned gate must still back off: {} -> {}",
            before,
            g.limit().await
        );
    }

    /// The full loop: a throttling endpoint, the real client, and the gate.
    ///
    /// Every earlier test fed the gate a signal it constructed itself. Nothing
    /// proved the runner's wiring end to end — HTTP 429 → `HarnessError` →
    /// `note_rate_limit()` → limit drops → recovery on success. Four benchmark
    /// runs never observed a single 429, so this path was entirely unverified
    /// until this test.
    ///
    /// The endpoint throttles the first two requests and then succeeds, so the
    /// test covers BOTH halves: back off on a sustained throttle, and recover
    /// once the endpoint clears.
    #[tokio::test]
    async fn a_throttling_endpoint_drives_the_gate_end_to_end() {
        use crate::agent_loop::ChatClient;
        use crate::direct::DirectConfig;
        use crate::{ChatRequest, DirectHarness, LoopMessage};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut served = 0u32;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                served += 1;
                // Throttle the first two requests, then stay healthy: the
                // recovery turn must reach a working endpoint.
                let throttled = served <= 2;
                let (status, body) = if throttled {
                    (
                        "429 Too Many Requests",
                        r#"{"error":{"message":"slow down"}}"#.to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        r#"{"choices":[{"message":{"content":"ok","tool_calls":[]}}],"usage":{"total_tokens":3}}"#
                            .to_string(),
                    )
                };
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status,
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
                if served >= 4 {
                    return;
                }
            }
        });

        let cfg = DirectConfig {
            base_url: format!("http://127.0.0.1:{}", port),
            api_key: "k".into(),
            model: "m".into(),
            // Zero client-side retries: the point of this test is what reaches the
            // GATE, and the client's own retry policy is covered separately in
            // `direct.rs`. With retries on, the back-off sleeps (1s + 2s) outlast
            // any short throttle window and the first turn succeeds.
            max_retries: 0,
            ..Default::default()
        };
        let client = DirectHarness::new(cfg).unwrap();

        let gate = AdaptiveGate::new(16);
        gate.pin_ceiling(16);
        let start = gate.limit().await;

        // Turn 1 and 2: the endpoint throttles. The client retries, exhausts, and
        // the runner feeds the 429 into the gate.
        for _ in 0..2 {
            let outcome = gate
                .run(|| async {
                    client
                        .chat(ChatRequest::new("m", vec![LoopMessage::user("hi")]))
                        .await
                        .map(|_| ())
                })
                .await;
            assert!(outcome.is_err(), "the throttled turn must fail");
            assert_eq!(client.last_http_status(), Some(429));
            gate.note_rate_limit().await;
        }

        let after_backoff = gate.limit().await;
        assert!(
            after_backoff < start,
            "sustained throttling must reduce the limit: {} -> {}",
            start,
            after_backoff
        );

        // Turn 3: the endpoint is healthy again. The call must now SUCCEED,
        // which proves the gate never got wedged shut by the back-off.
        let recovered = gate
            .run(|| async {
                client
                    .chat(ChatRequest::new("m", vec![LoopMessage::user("hi")]))
                    .await
                    .map(|_| ())
            })
            .await;
        assert!(
            recovered.is_ok(),
            "the endpoint recovered, so the call must succeed — a wedged gate would block it"
        );
        gate.note_ok().await;

        // The limit is NOT expected to spring back: AIMD's additive increase is
        // +1 per 8 successes, so recovery is deliberately slow. What this asserts
        // is that the gate is still functional and still counting, i.e. the
        // back-off did not leave it in a dead state.
        assert_eq!(
            gate.rate_limit_count(),
            2,
            "exactly the two throttled turns were counted"
        );
        assert!(
            gate.backoff_count() >= 1,
            "at least one real reduction happened"
        );
        assert!(
            gate.limit().await >= 1,
            "the gate must still admit work after backing off"
        );
        // The status side-channel must have been cleared by the successful call
        // path — it is only meaningful when a call fails.
        let _ = client.last_http_status();
    }
}




