//! The runner: wires the store, the git repo, the harness and the API together
//! and drives the worker pool.
//!
//! This is where the Go `runner.go` orchestration lives: spawn workers, run the
//! autoscaler / integrator / renewer / reaper loops, and stop on any of the four
//! ceiling conditions. The *decisions* are all in [`crate::breaker`] and
//! [`crate::autoscaler`]; this module only observes reality and obeys them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use swarm_config::RunConfig;
use swarm_git::Repo;
use swarm_harness::{AdaptiveGate, AgentLoopConfig};
use swarm_store::Store;
use tokio::sync::Mutex as AsyncMutex;

use crate::autoscaler::{Autoscaler, ScaleDecision};
use crate::breaker::{CircuitBreaker, GuardId, Verdict, World};
use crate::breaker_report::BreakerReport;
use crate::worker::{ItemLedgers, WorkerState, WorkerStatus};

/// A minimal cancellation token.
///
/// `tokio_util::sync::CancellationToken` would do, but a short version keeps the
/// dependency list small and the semantics we need (cancel + wait) obvious.
///
/// Waiting is event-driven, not polled. The previous version slept 50ms in a
/// loop, which meant one live timer per waiter — at `max_workers = 1000` that is
/// a thousand timers doing nothing but waking up to check a bool
/// (`docs/OPTIMIZATION-SWARM-RS.md` §3.1). `Notify` gives the same semantics with
/// zero timers: `cancel` stores the flag *then* wakes every waiter, so a waiter
/// that starts after the cancel still sees the flag and returns immediately.
#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Woken on cancel. Stored beside the flag so `Default` gives a ready token.
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        // Flag first: a waiter woken by `notify_waiters` must observe it, and a
        // waiter arriving later must observe it without being woken.
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    /// Resolves once cancelled. Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // Register the waiter before re-checking, so a cancel that lands between
        // the check and the registration cannot be missed.
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// What the turn that just finished actually did.
///
/// These four values are the only ones `observe()` cannot read off the store or
/// the host, because they describe an in-flight LLM call. Passing them in beats
/// inventing them: a hard-coded `submitted_this_turn: false` makes a worker that
/// just submitted indistinguishable from one that did nothing.
#[derive(Debug, Clone, Copy, Default)]
struct TurnOutcome {
    /// The reply contained the model's "nothing left to do" marker.
    reply_says_idle: bool,
    /// A submission happened during this turn.
    submitted: bool,
    /// The harness call itself succeeded (transport + a non-empty completion).
    ok: bool,
    /// HTTP status of a failed harness call, if any.
    http_status: Option<u16>,
}

/// Run-wide token totals, including retired workers' spend.
#[derive(Default)]
struct Totals {
    input_tokens: i64,
    output_tokens: i64,
    api_calls: u64,
    merged: usize,
    rejected: usize,
}

/// What the AIMD gate is doing, sampled once per turn.
///
/// The gate's whole job is to react to throttling, and for four benchmark runs
/// its state was invisible: `aimd.rs` had fifteen unit tests and no way to
/// observe the live limit, so nobody could tell a correct back-off from a gate
/// that never fired. This is that observation.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateState {
    /// Current concurrency limit.
    pub limit: usize,
    /// Pinned ceiling after a back-off.
    pub ceiling: usize,
    /// Requests in flight right now.
    pub active: usize,
    /// Consecutive successes since the last rate-limit.
    pub ok_streak: u64,
    /// Whether the gate is still in its doubling slow-start phase.
    pub slow_start: bool,
    /// Total 429/503 signals observed.
    pub rate_limits: u64,
    /// How many times the limit was reduced.
    pub backoffs: u64,
}

/// The swarm runner.
pub struct Runner {
    cfg: RunConfig,
    store: Store,
    repo: Repo,

    workers: AsyncMutex<Vec<WorkerState>>,
    ledgers: Arc<Mutex<ItemLedgers>>,
    breakers: Arc<Mutex<BreakerReport>>,
    /// In-flight gates. Atomic so the integrator can never read a stale zero
    /// (docs/OPTIMIZATION-SWARM.md §2.1).
    inflight: Arc<AtomicUsize>,
    totals: Arc<Mutex<Totals>>,
    /// Live worker count, shared with every worker so the breakers that ask
    /// "am I the last one?" get a real answer.
    active_workers: Arc<AtomicUsize>,
    /// Consecutive spawns that produced no claim or submission. Shared so a
    /// worker can report its own outcome and the autoscaler can read the total
    /// (guard 11; docs/OPTIMIZATION-SWARM-RS.md §1.5).
    stalled_spawns: Arc<AtomicUsize>,
    /// AIMD concurrency gate over every worker's LLM call.
    ///
    /// One gate for the whole pool: what needs governing is the *aggregate* rate
    /// at which the swarm hits its endpoint. `aimd.rs` shipped complete with 15
    /// tests and was never constructed anywhere, so the first 429 got no backoff
    /// at all (docs/OPTIMIZATION-SWARM-RS.md §2.1). Initial window follows Go:
    /// `clamp(maxWorkers * 4, 8, 64)`.
    gate: AdaptiveGate,
    /// Latest observed gate state, for the report and for tests.
    gate_state: Arc<Mutex<GateState>>,
    started_at: std::time::Instant,
}

impl Runner {
    pub fn new(dir: String, cfg: RunConfig, store: Store, repo: Repo) -> Self {
        let _ = dir;
        // Read before the move: the struct literal takes `cfg` by value.
        let gate = AdaptiveGate::new(AdaptiveGate::initial_window(cfg.max_workers));
        // Pin the ceiling to the pool size.
        //
        // `initial_window` is `clamp(max_workers * 4, 8, 64)` and slow start
        // doubles it on every 8 successes, so a 96-worker pool starts at 64 and
        // walks to 128 — above the number of workers that exist. The gate then
        // can never be the binding constraint, which is the one thing it is for.
        // Measured in the 8-minute saturated run: `limit=128`, `rate_limits=0`.
        //
        // `pin_ceiling` only lowers a ceiling and never widens the live limit,
        // so a later AIMD back-off still works exactly as before.
        gate.pin_ceiling(cfg.max_workers);
        let gate_state = Arc::new(Mutex::new(GateState::default()));
        Self {
            cfg,
            store,
            repo,
            workers: AsyncMutex::new(Vec::new()),
            ledgers: Arc::new(Mutex::new(ItemLedgers::new())),
            breakers: Arc::new(Mutex::new(BreakerReport::new())),
            inflight: Arc::new(AtomicUsize::new(0)),
            totals: Arc::new(Mutex::new(Totals::default())),
            active_workers: Arc::new(AtomicUsize::new(0)),
            stalled_spawns: Arc::new(AtomicUsize::new(0)),
            gate,
            gate_state,
            started_at: std::time::Instant::now(),
        }
    }

    /// Run until a stop condition fires, then drain and report.
    ///
    /// Takes `Arc<Self>` because the four background loops each need a
    /// `'static` handle; the caller keeps its own reference for the report.
    pub async fn run(self: &Arc<Self>) -> Result<(), String> {
        // Recover orphan claims, ensure the integration branch, presplit.
        let _ = self.store.reap_expired();
        self.repo
            .ensure_branch(&self.repo.integration(), "HEAD")
            .await
            .map_err(|e| e.to_string())?;

        let presplit = self.presplit_oversized_items().await;
        if presplit > 0 {
            println!(
                "[swarm] presplit {presplit} oversized seed item(s) into per-file sub-items"
            );
        }

        // Initial workers.
        for i in 0..self.cfg.initial_workers {
            if self.spawn_worker(i).await.is_err() {
                break;
            }
        }

        // The four background loops, each with its own clone of the token.
        let cancel = CancellationToken::new();
        let loops = [
            tokio::spawn(self.clone().autoscaler_loop(cancel.clone())),
            tokio::spawn(self.clone().integrator_loop(cancel.clone())),
            tokio::spawn(self.clone().renewer_loop(cancel.clone())),
            tokio::spawn(self.clone().reaper_loop(cancel.clone())),
        ];

        let stop = self.wait_for_stop(&cancel).await;
        println!("[swarm] stopping: {stop}");

        cancel.cancel();
        for h in loops {
            let _ = h.await;
        }

        self.report(&stop);
        Ok(())
    }

    /// The world as the breakers currently see it.
    ///
    /// Exists so a test can assert that `observe()` fills its fields from real
    /// sources instead of literals. `SWARM-RUN-ISSUES.md` §4 named this exact
    /// test as the one that would have caught the six-zero bug; the per-turn
    /// fields are `Default` here because there is no turn in flight.
    pub async fn world_snapshot(&self) -> World {
        let cb = CircuitBreaker::new(self.cfg.clone());
        let handle = WorkerHandle {
            cfg: self.cfg.clone(),
            store: self.store.clone(),
            repo: self.repo.clone(),
            worktree: self.repo.integration_path(),
            breakers: self.breakers.clone(),
            totals: self.totals.clone(),
            ledgers: self.ledgers.clone(),
            active_workers: self.active_workers.clone(),
            stalled_spawns: self.stalled_spawns.clone(),
            gate: self.gate.clone(),
            gate_state: self.gate_state.clone(),
        };
        handle.observe(&cb, TurnOutcome::default()).await
    }

    /// The breaker statistics gathered so far.
    pub fn breaker_report(&self) -> BreakerReport {
        self.breakers.lock().map(|b| b.clone()).unwrap_or_default()
    }

    /// The AIMD gate's latest observed state.
    pub fn gate_state(&self) -> GateState {
        self.gate_state.lock().map(|g| *g).unwrap_or_default()
    }

    /// Wait for any of the four ceilings.
    async fn wait_for_stop(&self, cancel: &CancellationToken) -> String {
        let idle_stop = self.cfg.effective_idle_stop();
        let wall = if self.cfg.max_wall_clock.is_zero() {
            None
        } else {
            Some(tokio::time::Instant::now() + self.cfg.max_wall_clock)
        };
        let mut done_tick = tokio::time::interval(Duration::from_secs(3));
        done_tick.tick().await;
        let mut idle_tick = tokio::time::interval(idle_stop);
        idle_tick.tick().await;
        let mut last_progress = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return "signal".into(),
                _ = idle_tick.tick() => {
                    // Any progress resets the silence timer.
                    if last_progress.elapsed() < idle_stop {
                        last_progress = std::time::Instant::now();
                        continue;
                    }
                    return "silence-timeout".into();
                }
                _ = done_tick.tick(), if self.cfg.exit_when_done => {
                    if self.seed_work_done().await {
                        return "seed-done".into();
                    }
                    last_progress = std::time::Instant::now();
                }
                _ = async {
                    match wall {
                        Some(w) => tokio::time::sleep_until(w).await,
                        None => std::future::pending::<()>().await,
                    }
                } => return "wall-clock".into(),
            }
        }
    }

    /// Whether every seed item is terminal and nothing is queued or running.
    async fn seed_work_done(&self) -> bool {
        let Ok((open, total)) = self.store.seed_progress() else {
            return false;
        };
        if open > 0 || total == 0 {
            return false;
        }
        let queued = self.store.queued_submissions().unwrap_or_default().len();
        if queued > 0 {
            return false;
        }
        let active = self
            .workers
            .lock()
            .await
            .iter()
            .filter(|w| w.is_active())
            .count();
        active == 0
    }

    /// Split every seed item that names more files than one worker should
    /// serialise, BEFORE any worker starts.
    async fn presplit_oversized_items(&self) -> usize {
        let Ok((items, _total)) = self.store.list_items("pending", 0, 0) else {
            return 0;
        };
        let mut split = 0;
        for it in items {
            if it.created_by.trim() != "seed" {
                continue; // only the operator's items
            }
            if self.split_item(&it.id).await {
                split += 1;
            }
        }
        split
    }

    /// Decompose an item into one child per real resource.
    ///
    /// Called both by the pre-run presplit (seed items only) and by the
    /// integrator when an item keeps failing the gate. The second caller is the
    /// point: a task that cannot pass on its own is a task that is too big, and
    /// handing it to yet another worker unchanged just burns `max_item_fails`
    /// attempts on the same approach.
    ///
    /// Safe to call repeatedly — it refuses once the item already has children,
    /// so a run of rejections cannot fork-bomb the board.
    async fn split_item(&self, item_id: &str) -> bool {
        let Ok(item) = self.store.get_item(item_id) else {
            return false;
        };
        let Ok(total) = self.store.item_count() else {
            return false;
        };
        let Ok(depth) = self.store.item_depth(item_id) else {
            return false;
        };
        if total >= self.cfg.max_items || depth >= 2 {
            return false;
        }

        let resources = swarm_store::resources::item_resources(&item.title, &item.detail);
        // Only split along resources git actually tracks, so a child is never a
        // phantom path. Without the index we cannot tell real from phantom, so
        // keep the candidates rather than silently refusing to split.
        let tracked: Option<Vec<String>> = self
            .repo
            .git(&["ls-files"])
            .await
            .ok()
            .map(|out| {
                out.lines()
                    .map(|l| l.trim().replace('\\', "/").to_lowercase())
                    .filter(|l| !l.is_empty())
                    .collect()
            });
        let real: Vec<String> = match tracked {
            Some(tracked) => resources
                .into_iter()
                .filter(|r| {
                    let file = r.split('#').next().unwrap_or(r).to_lowercase();
                    tracked.iter().any(|t| t == &file)
                })
                .collect(),
            None => resources,
        };

        // Two or more files means the work is parallelisable.
        //
        // A single-file item CANNOT be split along resources, and that is the
        // common case for a "create this module" task. Refusing here used to be
        // silent, so the caller believed it had split when it had not — the
        // failure-triggered split then looked broken while actually having never
        // applied. The caller distinguishes the two outcomes.
        if real.len() < 2 {
            return false;
        }
        if self.store.set_status(item_id, "split").is_err() {
            return false;
        }
        // One child per resource. `create_item` may FOLD a child into an existing
        // open item with the same file key — that is the dedup Go has at
        // `items.go` L365-394, and it is correct behaviour: two items naming one
        // file should be one item. What must not happen is a silent partial
        // split, so the outcome is verified against the child count rather than
        // assumed from the loop's trip count.
        for res in &real {
            let _ = self.store.create_item(swarm_core::CreateItemInput {
                peer: "auto-split".into(),
                title: res.clone(),
                detail: format!(
                    "Split from {:?} — deliver {} only. {}",
                    item.title, res, item.detail
                ),
                parent_id: Some(item_id.to_string()),
                kind: "task".into(),
                priority: 0,
            });
        }
        self.store.child_count(item_id).unwrap_or(0) > 0
    }

    /// Spawn one worker with a fresh name and worktree.
    async fn spawn_worker(&self, hint: usize) -> Result<(), String> {
        let name = {
            let workers = self.workers.lock().await;
            let mut n = workers.len() + 1 + hint;
            loop {
                let candidate = format!("w{}", n);
                if !workers.iter().any(|w| w.name == candidate) {
                    break candidate;
                }
                n += 1;
            }
        };

        let wt = self
            .repo
            .add_worktree(&name, &self.repo.integration())
            .await
            .map_err(|e| e.to_string())?;

        {
            let mut workers = self.workers.lock().await;
            workers.push(WorkerState::new(name));
        }
        self.active_workers.fetch_add(1, Ordering::SeqCst);

        let handle = WorkerHandle {
            cfg: self.cfg.clone(),
            store: self.store.clone(),
            repo: self.repo.clone(),
            worktree: wt,
            breakers: self.breakers.clone(),
            totals: self.totals.clone(),
            ledgers: self.ledgers.clone(),
            active_workers: self.active_workers.clone(),
            stalled_spawns: self.stalled_spawns.clone(),
            gate: self.gate.clone(),
            gate_state: self.gate_state.clone(),
        };
        tokio::spawn(async move {
            let mut cb = CircuitBreaker::new(handle.cfg.clone());
            handle.worker_loop(&mut cb).await;
        });
        Ok(())
    }

    /// The autoscaler loop (1s tick).
    async fn autoscaler_loop(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut autoscaler = Autoscaler::new(self.cfg.clone());
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }

            let open = self.store.open_count().unwrap_or(0);
            let recent = self.store.recent_split_count(300_000).unwrap_or(0);
            let (active, idle) = {
                let workers = self.workers.lock().await;
                (
                    workers.iter().filter(|w| w.is_active()).count(),
                    workers
                        .iter()
                        .filter(|w| w.status == WorkerStatus::Idle)
                        .count(),
                )
            };

            // Decay the give-up counter when the pool is empty (§2.3), and
            // absorb whatever the workers reported since the last tick.
            autoscaler.decay(Duration::from_secs(300));
            let reported = self.stalled_spawns.swap(0, Ordering::SeqCst);
            for _ in 0..reported.min(100) {
                autoscaler.note_stalled_spawn();
            }
            // Real host load now that sysload exists (§1.3).
            //
            // `sample()` blocks for 250ms to get a CPU delta, and this is the
            // autoscaler's async loop — blocking it stalls every `select!` arm
            // for that long, including the cancellation check. At 1000 workers
            // the loop is the only thing that can shed them, so it must stay
            // responsive. The sample is cheap enough to move off-thread, and the
            // tick is 1s so a late sample costs nothing.
            let load = tokio::task::spawn_blocking(crate::sysload::sample)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();

            match autoscaler.decide(open, recent, active, idle, load) {
                ScaleDecision::Spawn(n) => {
                    for i in 0..n {
                        if self.spawn_worker(i).await.is_err() {
                            break;
                        }
                    }
                }
                ScaleDecision::Shrink(n) => {
                    let mut workers = self.workers.lock().await;
                    let mut left = n;
                    for w in workers.iter_mut() {
                        if left == 0 {
                            break;
                        }
                        if w.status == WorkerStatus::Idle {
                            w.status = WorkerStatus::Retired;
                            left -= 1;
                        }
                    }
                }
                ScaleDecision::GiveUp => {
                    eprintln!(
                        "[swarm] autoscaler: giving up after {} stalled spawns (harness is not driving tools)",
                        autoscaler.stalled_spawns()
                    );
                }
                ScaleDecision::Hold => {}
            }
        }
    }

    /// The integrator loop: merge serially, gate concurrently.
    async fn integrator_loop(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }

            let subs = match self.store.queued_submissions() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if subs.is_empty() {
                if self.inflight.load(Ordering::SeqCst) == 0 {
                    continue;
                }
                // Gates still running: wait for either shutdown or a tick.
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
                continue;
            }

            for s in subs {
                if cancel.is_cancelled() {
                    return;
                }

                // Stale check: the submit sha must still be on the branch.
                if !s.commit_sha.is_empty() {
                    let branch = self.repo.worker_branch(&s.peer);
                    let ok = self
                        .repo
                        .git(&["merge-base", "--is-ancestor", &s.commit_sha, &branch])
                        .await
                        .is_ok();
                    if !ok {
                        self.reject_submission(&s, "stale submission", "stale").await;
                        continue;
                    }
                }

                let branch = self.repo.worker_branch(&s.peer);
                let res = match self
                    .repo
                    .merge_only(&branch, &format!("integrate {} ({})", s.peer, s.item_id))
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        self.reject_submission(&s, &e.to_string(), "failed").await;
                        continue;
                    }
                };
                if res.noop {
                    // "Nothing new" and "already integrated" are different
                    // outcomes, and the old code accepted both silently — so a
                    // duplicated submission inflated the accepted count.
                    //
                    // The tree sha is the content identity: if it matches the
                    // last accepted tree for this item, the work was already
                    // folded in and this is a duplicate; otherwise the branch
                    // genuinely carried nothing.
                    let duplicate = !res.tree_sha.is_empty()
                        && self
                            .store
                            .last_accepted_tree(&s.item_id)
                            .ok()
                            .flatten()
                            .as_deref()
                            == Some(res.tree_sha.as_str());
                    let _ = self.store.resolve_submission(s.id, true, "", 3);
                    if duplicate {
                        // Record it as what it is, not as a fresh acceptance.
                        let _ = self.store.append_run_log(swarm_core::RunLogEntry {
                            id: 0,
                            kind: "duplicate".into(),
                            actor: s.peer.clone(),
                            item_id: s.item_id.clone(),
                            title: res.sha.clone(),
                            detail: format!(
                                "tree {} already integrated; submission was a duplicate",
                                &res.tree_sha[..res.tree_sha.len().min(12)]
                            ),
                            tokens: 0,
                            files: 0,
                            seconds: 0,
                            ts: 0,
                            accepted: false,
                            // Same transition as an acceptance — the item really
                            // does reach `done` — but recorded as a duplicate so
                            // the acceptance rate is not inflated.
                            field: "status".into(),
                            old_value: "submitted".into(),
                            new_value: "done".into(),
                        });
                        continue;
                    }
                    continue;
                }

                // ---- concurrent phase: gate off the serial path ----
                self.inflight.fetch_add(1, Ordering::SeqCst);
                let inflight = self.inflight.clone();
                let repo = self.repo.clone();
                let store = self.store.clone();
                let totals = self.totals.clone();
                let from = res.from.clone();
                let peer = s.peer.clone();
                let item_id = s.item_id.clone();
                // `split_failed_item` needs the runner (it touches the repo), so
                // the spawned task carries an `Arc` clone rather than borrowing.
                let splitter = self.clone();
                tokio::spawn(async move {
                    match repo.gate().await {
                        Ok(()) => {
                            let _ = store.resolve_submission(s.id, true, "", 3);
                            // The fact layer: an accepted integration is recorded
                            // with its item and actor, so the acceptance rate is
                            // derived from durable rows rather than from a
                            // counter that dies with the process.
                            let _ = store.append_run_log(swarm_core::RunLogEntry {
                                id: 0,
                                kind: "integration".into(),
                                actor: peer.clone(),
                                item_id: item_id.clone(),
                                title: from.clone(),
                                // The tree sha is the content identity, kept here
                                // so a later duplicate submission can be told
                                // apart from genuinely new work. `title` holds the
                                // pre-merge commit, which is NOT stable across
                                // merges of the same content.
                                detail: res.tree_sha.clone(),
                                tokens: 0,
                                files: 0,
                                seconds: 0,
                                ts: 0,
                                accepted: true,
                                field: "status".into(),
                                old_value: "submitted".into(),
                                new_value: "done".into(),
                            });
                            if let Ok(mut t) = totals.lock() {
                                t.merged += 1;
                            }
                        }
                        Err(e) => {
                            // Roll back first, then record WHY. The gate error
                            // carries the full test output, which is the only
                            // thing that makes a rejection diagnosable — losing
                            // it was docs/OPTIMIZATION-SWARM-RS.md §1.2.
                            let _ = repo.rollback_to(&from).await;
                            let mut reason: String =
                                e.to_string().chars().take(400).collect();
                            // Dispatch on the stable code, never on the message
                            // text. Searching `reason.contains("conflict")` was the
                            // pattern cc-fleet's CLAUDE.md forbids: reword the
                            // message and the behaviour silently changes.
                            //
                            // The remedy is what turns a rejection into progress —
                            // in the 300-worker run the workers derived exactly
                            // this fix themselves, the hard way, across several
                            // failed items.
                            let remedy = e.remedy();
                            if !remedy.is_empty() {
                                reason.push_str(" — ");
                                reason.push_str(remedy);
                            }
                            let _ = store.resolve_submission(s.id, false, &reason, 3);
                            let _ = store.append_run_log(swarm_core::RunLogEntry {
                                id: 0,
                                kind: "rejected".into(),
                                actor: peer.clone(),
                                item_id: item_id.clone(),
                                title: from.clone(),
                                detail: reason.clone(),
                                tokens: 0,
                                files: 0,
                                seconds: 0,
                                ts: 0,
                                accepted: false,
                                // State provenance: the answer to "how did this
                                // item get here?" is `submitted → pending`, not
                                // just "it was rejected". Without old/new the log
                                // records outcomes but not transitions.
                                field: "status".into(),
                                old_value: "submitted".into(),
                                new_value: "pending".into(),
                            });
                            let _ = store.add_context(swarm_core::Event {
                                id: 0,
                                peer,
                                kind: "FAIL".into(),
                                item_id: Some(item_id),
                                text: format!(
                                    "integration rejected for item {} (stage=gate): {}",
                                    s.item_id, reason
                                ),
                                tags: Some(vec!["integration".into(), "gate".into()]),
                                ts: 0,
                            });
                            if let Ok(mut t) = totals.lock() {
                                t.rejected += 1;
                            }
                            // A task that keeps failing the gate is often a task
                            // that is too big. Splitting it is the structural
                            // answer; without it the same item is handed to the
                            // next worker, which repeats the approach that just
                            // failed until `max_item_fails` marks it dead.
                            //
                            // NOT every rejection is a size problem, though. A
                            // merge conflict means two workers wrote the same
                            // region concurrently — splitting the item into MORE
                            // children makes that more likely, not less. So the
                            // split is attempted only when the code says the work
                            // itself failed; a conflict is left to the lease plus
                            // the baseline warning, which is what actually
                            // prevents it.
                            //
                            // `split_item` is idempotent (it refuses once the item
                            // has children) and depth-capped, so repeated
                            // rejections cannot fork-bomb the board.
                            let size_problem = e.code() != "MERGE_CONFLICT";
                            if size_problem && splitter.split_failed_item(&s.item_id).await {
                                let _ = store.add_context(swarm_core::Event {
                                    id: 0,
                                    peer: "auto-split".into(),
                                    kind: "FACT".into(),
                                    item_id: Some(s.item_id.clone()),
                                    text: "repeated gate failures: split into per-resource children so each can pass independently".into(),
                                    tags: Some(vec!["split".into(), "retry".into()]),
                                    ts: 0,
                                });
                            }
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
    }

    async fn reject_submission(&self, s: &swarm_core::Submission, reason: &str, stage: &str) {
        let reason: String = reason.chars().take(400).collect();
        let _ = self.store.resolve_submission(s.id, false, &reason, 3);
        let _ = self.store.add_context(swarm_core::Event {
            id: 0,
            peer: s.peer.clone(),
            kind: "FAIL".into(),
            item_id: Some(s.item_id.clone()),
            text: format!(
                "integration rejected for item {} (stage={}): {}",
                s.item_id, stage, reason
            ),
            tags: Some(vec!["integration".into(), stage.into()]),
            ts: 0,
        });
        if let Ok(mut t) = self.totals.lock() {
            t.rejected += 1;
        }
    }

    /// Split an item that has failed repeatedly, so its parts can each pass the
    /// gate on their own.
    ///
    /// Returns whether a split actually happened. The integrator calls this from
    /// a spawned task, hence the `Arc<Self>`.
    pub async fn split_failed_item(self: &Arc<Self>, item_id: &str) -> bool {
        self.split_item(item_id).await
    }

    /// Renew live leases every `leaseSec / 3`.
    async fn renewer_loop(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs((self.cfg.lease_sec / 3).max(1)));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let names: Vec<String> = {
                let workers = self.workers.lock().await;
                workers
                    .iter()
                    .filter(|w| {
                        w.status != WorkerStatus::Retired && w.status != WorkerStatus::Crashed
                    })
                    .map(|w| w.name.clone())
                    .collect()
            };
            for n in names {
                if let Ok(claims) = self.store.active_claims_by_peer(&n) {
                    for c in claims {
                        let _ = self.store.renew(&c.token);
                    }
                }
            }
        }
    }

    /// Reap expired leases and remove finished workers' worktrees.
    async fn reaper_loop(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let _ = self.store.reap_expired();

            // Which finished workers' worktrees are due for a removal attempt.
            //
            // `reap_fail_count` was incremented on failure and read by nothing, so
            // the backoff was decorative. A worker whose worktree cannot be removed
            // (a held file handle on Windows, a busy index) was retried every 10
            // seconds forever. It now follows the exponential schedule
            // `reap_backoff` always described.
            let now = std::time::Instant::now();
            let to_remove: Vec<String> = {
                let workers = self.workers.lock().await;
                workers
                    .iter()
                    .filter(|w| {
                        w.loop_done
                            && (w.status == WorkerStatus::Retired
                                || w.status == WorkerStatus::Crashed)
                    })
                    .filter(|w| {
                        // Not yet failed: always due.
                        w.reap_fail_count == 0
                            || now.duration_since(w.last_reap_attempt)
                                >= crate::worker::reap_backoff(w.reap_fail_count)
                    })
                    .filter(|w| {
                        !self
                            .store
                            .has_queued_submission_for_peer(&w.name)
                            .unwrap_or(false)
                    })
                    .map(|w| w.name.clone())
                    .collect()
            };
            {
                // Stamp the attempt for everyone we are about to try, so the
                // backoff is measured from THIS attempt rather than from the
                // last failure.
                let mut workers = self.workers.lock().await;
                for w in workers.iter_mut() {
                    if to_remove.contains(&w.name) {
                        w.last_reap_attempt = now;
                    }
                }
            }
            for name in to_remove {
                if self.repo.remove_worktree(&name).await.is_err() {
                    let mut workers = self.workers.lock().await;
                    if let Some(w) = workers.iter_mut().find(|w| w.name == name) {
                        w.reap_fail_count += 1;
                    }
                    continue;
                }
                let mut workers = self.workers.lock().await;
                workers.retain(|w| w.name != name);
            }
        }
    }

    /// The end-of-run report.
    fn report(&self, stop_reason: &str) {
        let t = self.totals.lock().unwrap_or_else(|e| e.into_inner());
        let ledgers = self.ledgers.lock().unwrap_or_else(|e| e.into_inner());
        let breakers = self.breakers.lock().unwrap_or_else(|e| e.into_inner());

        let wall = self.started_at.elapsed();
        let stats = self.store.stats().unwrap_or_default();
        let (seed_open, seed_total) = self.store.seed_progress().unwrap_or((0, 0));

        println!();
        println!("===== swarm run report ({stop_reason}) =====");
        println!(
            "items: {} total | {} done | {} failed | {} still open",
            stats.total,
            stats.done,
            stats.failed,
            stats.pending + stats.in_progress + stats.submitted
        );
        if seed_total > 0 {
            println!(
                "seed work: {}/{} terminal ({} still open)",
                seed_total - seed_open,
                seed_total,
                seed_open
            );
        }
        println!(
            "tokens: {} in / {} out / {} api calls",
            t.input_tokens, t.output_tokens, t.api_calls
        );
        println!("integrations: {} merged, {} rejected", t.merged, t.rejected);

        // Acceptance, not just throughput.
        //
        // The report used to answer "how much did it do" and never "was any of
        // it accepted". The number is derived from the run log — the durable
        // fact layer — so it survives the process and can be recomputed later,
        // rather than living only in an in-memory counter.
        let (logged_accepted, logged_attempted) =
            self.store.acceptance_counts().unwrap_or((0, 0));
        // Take the larger of the in-memory counters and the durable log: the log
        // is authoritative, but a run that logged nothing (an older database, or
        // a run stopped before any integration) still has the counters.
        let accepted = (t.merged as i64).max(logged_accepted);
        let attempted = logged_attempted.max(accepted + t.rejected as i64);
        println!(
            "accepted: {}/{} items passed the gate ({} rejected)",
            accepted, attempted, t.rejected
        );
        if attempted > 0 {
            let rate = (accepted as f64 / attempted as f64) * 100.0;
            println!("first-pass acceptance rate: {:.0}%", rate);
        }

        // The AIMD gate's state, so a reader can tell whether the pool was
        // throttled and whether the gate reacted. Before this line the only
        // evidence was the absence of evidence: no 429s observed meant either a
        // healthy endpoint or a gate that never fired, and nothing distinguished
        // the two.
        let gs = self.gate_state();
        println!(
            "llm gate: limit={} ceiling={} slow_start={} rate_limits={} backoffs={}",
            gs.limit, gs.ceiling, gs.slow_start, gs.rate_limits, gs.backoffs
        );
        if gs.rate_limits > 0 {
            println!(
                "  endpoint throttled {} time(s); gate reduced its limit {} time(s)",
                gs.rate_limits, gs.backoffs
            );
        }

        println!("wall: {:?}", wall);

        let top = ledgers.top_by_tokens(10);
        if !top.is_empty() {
            println!("top token spend:");
            for (id, l) in top {
                println!(
                    "  {:<24} {:>10} tok  {:>5} calls  {:>3} turns",
                    id, l.tokens, l.api_calls, l.turns_held
                );
            }
        }
        println!("{}", breakers.render());
        if seed_total > 0 && stats.done > 0 {
            let per_item = (t.input_tokens + t.output_tokens) / stats.done.max(1);
            println!("tokens per done item: {}", per_item);
        }
    }
}


/// A cloneable handle so a spawned worker can reach the shared pieces.
#[derive(Clone)]
struct WorkerHandle {
    cfg: RunConfig,
    store: Store,
    repo: Repo,
    /// The worker's worktree path.
    worktree: std::path::PathBuf,
    breakers: Arc<Mutex<BreakerReport>>,
    totals: Arc<Mutex<Totals>>,
    /// Per-item token ledgers, so guard 9's token test has real data.
    ledgers: Arc<Mutex<ItemLedgers>>,
    /// Live worker count, so guards 1/6/12/13 can see the whole pool.
    active_workers: Arc<AtomicUsize>,
    /// Shared stalled-spawn counter for guard 11.
    stalled_spawns: Arc<AtomicUsize>,
    /// The pool-wide AIMD gate (see `Runner::gate`).
    gate: AdaptiveGate,
    /// Shared with the runner so the report can print the gate's state.
    gate_state: Arc<Mutex<GateState>>,
}

impl WorkerHandle {
    /// Sample the pool-wide AIMD gate and remember what it is doing.
    ///
    /// Called once per turn, AFTER the turn's signal has been fed in, so the
    /// recorded state reflects the gate's reaction rather than its input.
    ///
    /// This exists because the gate's behaviour was unverifiable: `aimd.rs` had
    /// fifteen unit tests and no way to observe the live limit, so four
    /// benchmark runs could not tell a correct back-off from a gate that never
    /// fired. Now the state is on the record and the report can print it.
    async fn record_gate_state(&self) {
        let state = GateState {
            limit: self.gate.limit().await,
            ceiling: self.gate.ceiling().await,
            active: self.gate.active_now(),
            ok_streak: self.gate.ok_streak(),
            slow_start: self.gate.in_slow_start(),
            // Straight from the gate's own atomics, so they already span the
            // whole run and need no carry-forward between samples.
            rate_limits: self.gate.rate_limit_count(),
            backoffs: self.gate.backoff_count(),
        };
        if let Ok(mut g) = self.gate_state.lock() {
            *g = state;
        }
    }

    /// The peer name is the last component of the worktree path.
    fn peer(&self) -> String {
        self.worktree
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.worktree.to_string_lossy().to_string())
    }

    async fn worker_loop(&self, cb: &mut CircuitBreaker) {
        let is_echo = self.cfg.harness_name == "echo";
        loop {
            // ---- Guard 1: idle suppression ----
            // No turn has run yet this iteration, so the only honest answer for
            // the per-turn fields is "nothing happened".
            let world = self
                .observe(cb, TurnOutcome { reply_says_idle: false, ..Default::default() })
                .await;
            if let Some(Verdict::Skip { sleep, .. }) = cb.should_suppress_idle(&world) {
                tokio::time::sleep(sleep).await;
                continue;
            }
            if let Some(v) = cb.should_suppress_idle(&world) {
                self.record(&v);
                break;
            }

            // ---- Guards 2, 3, 4 ----
            if let Some(v) = cb.evaluate_pre_turn(&world) {
                self.record(&v);
                break;
            }

            cb.begin_turn();

            // When this turn started, so guard 9's wall-clock test has something
            // real to measure.
            //
            // `advance_hold` existed with tests and no production caller, so
            // `held_for` stayed at zero forever and the "held too long" criterion
            // of guard 9 could never fire. The guard-9 validation run proved it:
            // an item held for 40 tool-call rounds (the turn cap) was never
            // released, and `breakers: none` showed the guard had not even been
            // consulted. `max_item_seconds` was silently a no-op.
            let turn_started = std::time::Instant::now();

            // Per-turn harness facts, filled in by the turn below. They cannot be
            // read back from the store, so the loop carries them forward into
            // `observe()` instead of letting it invent a value.
            let mut harness_error: Option<String> = None;
            let mut last_http_status: Option<u16> = None;

            // ---- Guard 5: worktree sync ----
            let integration = self.repo.integration();
            // Where this worktree sits BEFORE the merge. Captured here, not after:
            // the merge moves HEAD onto `integration`, so a post-merge read makes
            // "what changed" always zero and the baseline note never fires. The
            // first version of this had exactly that bug and the 300-worker
            // comparison run showed it — five merge conflicts, unchanged, because
            // the warning never reached a single worker.
            let head_before_merge = self.repo.head(Some(&self.worktree)).await.ok();
            if self
                .repo
                .git_in(&self.worktree, &["merge", "--no-edit", &integration])
                .await
                .is_err()
            {
                cb.note_sync_conflict();
                if let Some(v) = cb.evaluate_sync_conflict() {
                    self.record(&v);
                    break;
                }
                if cb.should_reset_worktree() {
                    let _ = self
                        .repo
                        .git_in(&self.worktree, &["reset", "--hard", &integration])
                        .await;
                    cb.note_redo();
                    cb.note_sync_ok();
                }
            } else {
                cb.note_sync_ok();
            }

            // How far behind `integration` this worktree was BEFORE the merge
            // above, and what landed since.
            //
            // The merge makes the worktree current, but it does not tell the
            // model that anything changed — and the model's whole failure mode is
            // reading a file, forming a plan, then writing a full-file version
            // based on what it read. If a peer merged into the same file in the
            // meantime, that write silently reverts their work, which is exactly
            // the merge conflict the integrator then reports.
            //
            // Stating the baseline in the brief is the cheap structural fix: it
            // costs one `rev-list --count` and turns "your file may be stale" into
            // "N commits landed while you were away, re-read before overwriting".
            let baseline_note = self
                .baseline_note(&integration, head_before_merge.as_deref())
                .await;

            // ---- The turn itself ----
            let brief = self.build_brief(&baseline_note).await;
            // The item's Definition of Done, if its detail carries one. This is
            // the "contract-first" idea from the Super-agent design exploration:
            // the worker is told what acceptance means before it starts, rather
            // than inferring it from the title.
            let dod = self.held_dod(cb).await;
            let prompt = swarm_prompts::build_worker_prompt(&swarm_prompts::Options {
                peer_name: self.peer(),
                server_port: self.cfg.server_port,
                mission: self.cfg.mission.clone(),
                conventions: self.cfg.conventions.clone(),
                turn: cb.turn(),
                brief,
                harness: self.cfg.harness_name.clone(),
                dod,
                ..Default::default()
            });

            let loop_cfg = AgentLoopConfig {
                work_dir: self.worktree.to_string_lossy().to_string(),
                server_port: self.cfg.server_port,
                ..Default::default()
            };

            let result = if is_echo {
                // The echo harness performs no I/O: it exists so the offline
                // tests can drive the whole loop. It never touches the gate,
                // because it also never touches the endpoint the gate governs.
                swarm_harness::AgentLoopResult {
                    reply: "echo".into(),
                    usage: Default::default(),
                    steps: 1,
                    submitted: false,
                }
            } else {
                match swarm_harness::DirectHarness::new(swarm_harness::DirectConfig::new(
                    &self.cfg.api_base,
                    &self.cfg.api_key,
                    &self.cfg.model,
                )) {
                    Ok(c) => {
                        // The gate wraps the whole loop, not one request: the
                        // loop makes several round-trips per turn, and it is the
                        // aggregate concurrency that triggers the 429.
                        //
                        // The status is read from the client because the loop's
                        // return type is shared with the echo harness, which has
                        // no status to report.
                        let gate = self.gate.clone();
                        let model = self.cfg.model.clone();
                        let prompt = prompt.clone();
                        let loop_cfg = loop_cfg.clone();
                        let client = c;
                        // `gate.run` needs a `Result`, so the status travels in
                        // the Ok arm and the error travels in the Err arm.
                        match gate
                            .run(|| async move {
                                let status = client.last_http_status();
                                swarm_harness::run_agent_loop(
                                    &client, &model, &prompt, &loop_cfg,
                                )
                                .await
                                .map(|r| (status, r))
                                .map_err(|e| (status, e))
                            })
                            .await
                        {
                            Ok((status, r)) => {
                                last_http_status = status;
                                r
                            }
                            Err(swarm_harness::GateRunError::Inner((status, e))) => {
                                last_http_status = status;
                                harness_error = Some(e.to_string());
                                Default::default()
                            }
                            Err(swarm_harness::GateRunError::Gate(_)) => Default::default(),
                        }
                    }
                    Err(_) => break,
                }
            };

            // What this turn actually did. The harness reports the HTTP status
            // through a side channel because the loop's return type is shared
            // with the echo harness, which never touches the network.
            let outcome = TurnOutcome {
                reply_says_idle: reply_says_idle(&result.reply),
                submitted: result.submitted,
                ok: harness_error.is_none() && result.steps > 0,
                http_status: last_http_status,
            };
            if harness_error.is_none() && result.steps == 0 {
                harness_error = Some("empty completion (no choices)".into());
            }

            // Token accounting: run-wide totals plus the per-item ledger, so
            // guard 9's token test has real data and the report's top-spend
            // table is not empty (docs/OPTIMIZATION-SWARM-RS.md §1.4).
            let round_tokens = result.usage.total_tokens;
            {
                if let Ok(mut t) = self.totals.lock() {
                    t.input_tokens += result.usage.prompt_tokens;
                    t.output_tokens += result.usage.completion_tokens;
                    t.api_calls += result.steps as u64;
                }
                if let Some(h) = cb.held_item() {
                    let id = h.item_id.clone();
                    if let Ok(mut l) = self.ledgers.lock() {
                        l.add(&id, round_tokens, result.steps as u64);
                    }
                }
            }

            // Track what this worker holds. The runner cannot see the claims a
            // worker takes through the HTTP API, so it asks the store after
            // every turn — this is what makes guards 4 and 9 meaningful
            // (docs/OPTIMIZATION-SWARM-RS.md §1.6).
            let my_claims = self.store.active_claims_by_peer(&self.peer()).unwrap_or_default();
            match my_claims.first() {
                Some(c) => {
                    if cb.held_item().map(|h| h.item_id.as_str()) != Some(c.item_id.as_str()) {
                        // A fresh claim: the swarm IS making progress.
                        cb.note_held_item(Some(c.item_id.clone()));
                    } else {
                        cb.note_held_item(Some(c.item_id.clone()));
                    }
                }
                None => cb.note_held_item(None),
            }

            // ---- Guards 7 and 8 ----
            // One read serves both: the fingerprint feeds guard 8, and the same
            // read (files + head) becomes the handover note if guard 9 fires.
            // Reading it twice would double three git invocations per turn.
            let progress = self.progress().await;
            cb.note_artifact(&progress.fingerprint);
            // Charge this turn's wall-clock to the item being held, so guard 9's
            // time criterion measures reality rather than a permanent zero.
            cb.advance_hold(turn_started.elapsed());
            // `turn_ok` is the real outcome, not a literal. Passing `true` here
            // makes guard 7 fire on turns whose harness call actually failed —
            // a transport error is not evidence that "the endpoint ignores
            // tool_choice", and parking a worker for it loses the work.
            if let Some(v) = cb.evaluate_post_turn(is_echo, outcome.ok) {
                self.record(&v);
                break;
            }

            // ---- Guard 9: release an item held too long ----
            if let Some(reason) = cb.should_release_held_item(&world) {
                if let Some(h) = cb.held_item() {
                    let item_id = h.item_id.clone();
                    cb.note_held_item(None);
                    // Drop the lease too, not just the runner's notion of the
                    // hold. Releasing the item back to `pending` while the claim
                    // row survives means `claim()` still sees a live lease and
                    // refuses every other peer with "already claimed" until this
                    // worker exits or the lease expires (up to `lease_sec`,
                    // default 300s) — and `renewer_loop` keeps extending it in
                    // the meantime, so the item is effectively stuck.
                    //
                    // The git work is NOT touched: commits live on this worker's
                    // branch and uncommitted edits stay on disk, so the next
                    // worker inherits the half-finished state either way.
                    for c in self.store.active_claims_by_peer(&self.peer()).unwrap_or_default() {
                        if c.item_id == item_id {
                            let _ = self.store.release(&c.token);
                        }
                    }
                    let _ = self.store.add_context(swarm_core::Event {
                        id: 0,
                        peer: self.peer(),
                        kind: "FAIL".into(),
                        item_id: Some(item_id),
                        text: format!(
                            "released after over the per-item limit — do not repeat this approach; try a different one ({reason}). Partial work to build on: {}",
                            progress.handover_note()
                        ),
                        tags: Some(vec!["item".into(), "timeout".into()]),
                        ts: 0,
                    });
                    self.record_raw(GuardId::ItemHold, reason);
                }
            }

            // ---- Guard 10: rate-limit backoff ----
            // `steps == 0` means the harness produced no usable completion.
            // A 429/503 is the only case that benefits from waiting, so the
            // status is recorded on the board — otherwise the operator sees a
            // breaker fire with no idea whether the endpoint was throttling or
            // the request was malformed.
            //
            // This is also where the AIMD gate learns: a 429/503 tells it to
            // back off, anything else tells it the capacity is fine. Feeding the
            // gate here rather than inside the loop keeps `aimd.rs` free of
            // HTTP concerns (it is a pure concurrency gate).
            let throttled = matches!(outcome.http_status, Some(429 | 503));
            if result.steps == 0 {
                if let Some(ref why) = harness_error {
                    let _ = self.store.add_context(swarm_core::Event {
                        id: 0,
                        peer: self.peer(),
                        kind: "OBSERVED".into(),
                        item_id: None,
                        text: format!(
                            "harness produced no completion (http={:?}): {}",
                            outcome.http_status, why
                        ),
                        tags: Some(vec!["harness".into(), "no-completion".into()]),
                        ts: 0,
                    });
                }
                if throttled {
                    self.gate.note_rate_limit().await;
                }
                cb.note_rate_limit();
                tokio::time::sleep(cb.rate_limit_backoff()).await;
                cb.rollback_turn();
                continue;
            }
            if throttled {
                self.gate.note_rate_limit().await;
            } else {
                self.gate.note_ok().await;
            }
            cb.note_ok();
            // Record where the gate ended up after this turn's signal. Without
            // this the AIMD state is invisible: the gate could be backing off
            // correctly and nobody could tell from the outside, which is exactly
            // why its behaviour went unverified across four benchmark runs.
            self.record_gate_state().await;

            // ---- Guard 6: idle ----
            let world = self.observe(cb, outcome).await;
            let idle = cb.is_idle(&world);
            cb.note_turn_result(result.submitted, idle);
            if cb.should_retire_idle(&world) {
                break;
            }
            if idle {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }

        // The worker is leaving. Release its claims back to the board and tell
        // the autoscaler whether this spawn produced anything — that is what
        // drives guard 11 (docs/OPTIMIZATION-SWARM-RS.md §1.5).
        for c in self.store.active_claims_by_peer(&self.peer()).unwrap_or_default() {
            let _ = self.store.release(&c.token);
        }
        let stalled = cb.held_item().is_none() && cb.turn() <= self.cfg.max_item_turns;
        if stalled {
            // Retired without ever claiming anything: a stalled spawn. This is
            // the only thing that can stop the autoscaler from spawning 1000
            // workers against an endpoint that never drives the tools.
            self.stalled_spawns.fetch_add(1, Ordering::SeqCst);
            self.record_raw(GuardId::AutoscalerGiveUp, "spawn produced no claim".into());
        }
        self.active_workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Snapshot the world for the breakers.
    ///
    /// Every field is filled from a real source. The first version of this
    /// method hard-coded six of them to zero, which silently disabled four
    /// breakers (budget, item-token, resource guard, exit-when-done) — see
    /// `docs/OPTIMIZATION-SWARM-RS.md` §1.1.
    ///
    /// A second pass left four *literal* fields behind — `reply_says_idle`,
    /// `submitted_this_turn`, `turn_ok`, `http_status` — even though the values
    /// were sitting right there in the turn result. Those four feed the
    /// no-progress and idle breakers, so a literal `false` there makes a worker
    /// that just submitted look identical to one that did nothing. The values
    /// are now passed in per turn instead of being invented here.
    async fn observe(&self, cb: &CircuitBreaker, turn: TurnOutcome) -> World {
        // The store is synchronous (one `Mutex<Connection>`, Go's
        // `SetMaxOpenConns(1)` equivalent), and `observe()` fires five queries
        // on every turn of every worker. Running those inline on the async
        // worker means a slow query blocks a tokio worker thread — and with
        // `max_workers` in the hundreds there are only a handful of those.
        // `spawn_blocking` moves the whole batch onto the blocking pool.
        let store = self.store.clone();
        let peer = self.peer();
        let snapshot = tokio::task::spawn_blocking(move || {
            (
                store.claimable_count().unwrap_or(0),
                store.open_count().unwrap_or(0),
                store.stats().unwrap_or_default(),
                store.active_claims_by_peer(&peer).unwrap_or_default().len(),
                store.seed_progress().unwrap_or((0, 0)),
                store.queued_submissions().unwrap_or_default().len(),
            )
        })
        .await;

        let (
            claimable,
            open,
            stats,
            my_claims,
            (seed_open, seed_total),
            queued_submissions,
        ) = snapshot.unwrap_or((0, 0, Default::default(), 0, (0, 0), 0));

        // Run-wide totals, so guard 2 can actually fire.
        let tokens_spent = self
            .totals
            .lock()
            .map(|t| t.input_tokens + t.output_tokens)
            .unwrap_or(0);

        // The item I hold: its id drives the ledger lookup for guard 9.
        let held = cb.held_item();
        let held_item_tokens = held
            .map(|h| {
                self.ledgers
                    .lock()
                    .ok()
                    .and_then(|l| l.get(&h.item_id).map(|e| e.tokens))
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        // Host load. `LoadSample::sample()` returns None on an unsupported
        // platform, which degrades to "no pressure" exactly like the Go guard.
        let load = crate::sysload::sample().unwrap_or_default();

        World {
            claimable,
            my_claims,
            open,
            total_items: stats.total,
            cold_start: stats.total == 0,
            tokens_spent,
            held_item_tokens,
            // The worker cannot see the whole pool; the runner passes it in via
            // the shared counter that `spawn_worker` maintains.
            active_workers: self.active_workers.load(Ordering::SeqCst),
            held_item_live: held.is_some(),
            reply_says_idle: turn.reply_says_idle,
            submitted_this_turn: turn.submitted,
            turn_ok: turn.ok,
            http_status: turn.http_status,
            cpu_pct: load.cpu_pct,
            mem_pct: load.mem_pct,
            seed_open,
            seed_total,
            queued_submissions,
        }
    }

    /// The Definition of Done for the item this worker currently holds.
    ///
    /// Reads it off the held item's `detail`, so an operator can state
    /// acceptance criteria in the seed file without any schema change. Empty
    /// when nothing is held or the item has no `DoD:` paragraph.
    async fn held_dod(&self, cb: &CircuitBreaker) -> String {
        let Some(h) = cb.held_item() else {
            return String::new();
        };
        let Ok(item) = self.store.get_item(&h.item_id) else {
            return String::new();
        };
        swarm_prompts::extract_dod(&item.detail)
    }

    /// Whether `integration` moved since this worktree last synced, as a note
    /// for the brief.
    ///
    /// Counts commits reachable from `integration` but not from the worktree's
    /// *previous* HEAD — i.e. exactly what the merge above just brought in. Zero
    /// (or an unreadable repo) produces no note, so a quiet swarm stays quiet.
    async fn baseline_note(&self, integration: &str, head_before: Option<&str>) -> String {
        // `head_before` is the worktree HEAD from BEFORE this turn's merge. Without
        // it there is nothing to compare against, so no note.
        let Some(before) = head_before else {
            return String::new();
        };
        let out = match self
            .repo
            .git_in(
                &self.worktree,
                &["rev-list", "--count", &format!("{}..{}", before, integration)],
            )
            .await
        {
            Ok(o) => o,
            Err(_) => return String::new(),
        };
        let n: usize = out.trim().parse().unwrap_or(0);
        if n == 0 {
            return String::new();
        }
        format!(
            "INTEGRATION MOVED: {n} commit(s) landed since your last sync. \
             Re-read any file you are about to edit — another worker may have \
             changed it. Prefer appending over rewriting whole files."
        )
    }

    /// The per-turn TURN BRIEF block.
    async fn build_brief(&self, baseline_note: &str) -> String {
        // `digest` is a multi-table read (claims, events, items) — the heaviest
        // single query in the turn. Off the async worker for the same reason as
        // `observe()`.
        let store = self.store.clone();
        let peer = self.peer();
        let Ok(d) = tokio::task::spawn_blocking(move || store.digest(&peer, 0))
            .await
            .unwrap_or_else(|_| Err(swarm_core::SwarmError::Store("digest panicked".into())))
        else {
            return String::new();
        };
        let mut lines = Vec::new();
        if !d.claims.is_empty() {
            let c: Vec<String> = d
                .claims
                .iter()
                .map(|x| format!("{} ({})", x.item_id, x.item_id))
                .collect();
            lines.push(format!("YOUR ACTIVE CLAIMS: {}", c.join("; ")));
        }
        lines.push(format!(
            "BOARD SNAPSHOT: {} pending / {} in_progress / {} done",
            d.stats.pending, d.stats.in_progress, d.stats.done
        ));
        // The stale-baseline warning goes ABOVE the board preview: it is a
        // precondition for acting on anything below it.
        if !baseline_note.is_empty() {
            lines.push(baseline_note.to_string());
        }
        for it in d.pending_preview.iter().take(20) {
            lines.push(format!("- {}: {} [p{}]", it.id, it.title, it.priority));
        }
        lines.join("\n")
    }

    /// What this worker has actually produced, for the next one to inherit.
    ///
    /// The fingerprint alone says "3 files changed, +142 lines", which tells the
    /// next worker that something happened but not *what*. A half-finished task
    /// used to hand over nothing but "released after over the per-item limit",
    /// so the next worker started from scratch — and often repeated the exact
    /// approach that had just failed.
    ///
    /// This is the raw material for that handover: which files, and the commit
    /// this worktree currently sits on (so the next worker can read or cherry-pick
    /// the previous attempt instead of rediscovering it).
    async fn progress(&self) -> WorkProgress {
        let mut p = WorkProgress::default();

        // Changed tracked files (added / modified).
        if let Ok(diff) = self
            .repo
            .git_in(&self.worktree, &["diff", "--numstat", "HEAD"])
            .await
        {
            let (files, add, del) = count_numstat(&diff);
            p.fingerprint.push_str(&format!("f{}+{}-{}", files, add, del));
            for line in diff.lines() {
                // "<added>\t<deleted>\t<path>"
                if let Some(path) = line.split('\t').nth(2) {
                    let path = path.trim();
                    if !path.is_empty() {
                        p.changed_files.push(path.to_string());
                    }
                }
            }
        }
        // Untracked files: a worker that only creates new files has an empty
        // `git diff HEAD` and would otherwise look idle.
        if let Ok(untracked) = self
            .repo
            .git_in(&self.worktree, &["ls-files", "--others", "--exclude-standard"])
            .await
        {
            let mut n = 0;
            for line in untracked.lines() {
                let path = line.trim();
                if path.is_empty() {
                    continue;
                }
                n += 1;
                p.changed_files.push(path.to_string());
            }
            p.fingerprint.push_str(&format!("u{}", n));
        }
        // Staged-but-uncommitted work.
        if let Ok(staged) = self
            .repo
            .git_in(&self.worktree, &["diff", "--cached", "--numstat"])
            .await
        {
            let mut n = 0;
            for line in staged.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                n += 1;
                if let Some(path) = line.split('\t').nth(2) {
                    let path = path.trim();
                    if !path.is_empty() {
                        p.changed_files.push(path.to_string());
                    }
                }
            }
            if n > 0 {
                p.fingerprint.push_str(&format!("s{}", n));
            }
        }

        // Where this worktree currently points, so the next worker can find the
        // commits the previous one already made.
        if let Ok(sha) = self.repo.head(Some(&self.worktree)).await {
            p.head_sha = sha;
        }
        p
    }

    fn record(&self, v: &Verdict) {
        if let Some(reason) = retire_reason(v) {
            self.record_raw(guard_of(v), reason);
        }
    }

    fn record_raw(&self, guard: GuardId, reason: String) {
        if let Ok(mut b) = self.breakers.lock() {
            b.record(guard, reason);
        }
    }
}

/// Whether the model's reply is the explicit "nothing left for me" marker.
///
/// The worker prompt (`swarm-prompts` L78, L89) instructs the model to reply
/// *exactly* `IDLE` when the board has no suitable work, and guard 6 reads that
/// back as a second, independent idle trigger. Matching the whole reply — rather
/// than searching for the substring — keeps a worker that merely mentions
/// "idle" in a summary from being counted as idle.
fn reply_says_idle(reply: &str) -> bool {
    reply.trim().eq_ignore_ascii_case("IDLE")
}

/// What a worker has produced in `dir`, for the next one to inherit.
///
/// Exposed for the handover tests: the same read `WorkerHandle::progress` does,
/// against an arbitrary worktree, so a test can assert the next worker is told
/// which files changed without standing up a whole runner.
pub async fn progress_for_testing(dir: &std::path::Path) -> WorkProgress {    let repo = Repo::new(dir.to_string_lossy().as_ref());
    let handle = WorkerHandle {
        cfg: RunConfig::default(),
        store: Store::open("").unwrap_or_else(|_| Store::open(":memory:").expect("memory store")),
        repo,
        worktree: dir.to_path_buf(),
        breakers: Arc::new(Mutex::new(BreakerReport::new())),
        totals: Arc::new(Mutex::new(Totals::default())),
        ledgers: Arc::new(Mutex::new(ItemLedgers::new())),
        active_workers: Arc::new(AtomicUsize::new(0)),
        stalled_spawns: Arc::new(AtomicUsize::new(0)),
        gate: AdaptiveGate::new(8),
        gate_state: Arc::new(Mutex::new(GateState::default())),
    };
    handle.progress().await
}

/// The stale-baseline warning for a worktree, exposed for tests.
///
/// `head_before` is the worktree HEAD from BEFORE this turn's merge — the whole
/// point of the parameter is that it must be captured pre-merge, and a test that
/// could only reach this through a live runner could not check that.
pub async fn baseline_note_for_testing(
    worktree: &std::path::Path,
    integration: &str,
    head_before: Option<&str>,
) -> String {
    let handle = WorkerHandle {
        cfg: RunConfig::default(),
        store: Store::open("").unwrap_or_else(|_| Store::open(":memory:").expect("memory store")),
        repo: Repo::new(worktree.to_string_lossy().as_ref()),
        worktree: worktree.to_path_buf(),
        breakers: Arc::new(Mutex::new(BreakerReport::new())),
        totals: Arc::new(Mutex::new(Totals::default())),
        ledgers: Arc::new(Mutex::new(ItemLedgers::new())),
        active_workers: Arc::new(AtomicUsize::new(0)),
        stalled_spawns: Arc::new(AtomicUsize::new(0)),
        gate: AdaptiveGate::new(8),
        gate_state: Arc::new(Mutex::new(GateState::default())),
    };
    handle.baseline_note(integration, head_before).await
}

/// What a worker has produced, for the next one to inherit.
///
/// The half-finished-handover problem: a task released by guard 9 used to leave
/// nothing behind but a sentence. The next worker could not see which files were
/// touched, so it restarted from zero — and frequently repeated the approach that
/// had just failed.
#[derive(Debug, Clone, Default)]
pub struct WorkProgress {
    /// The guard-8 fingerprint, e.g. `f3+142-17u2s1`.
    pub fingerprint: String,
    /// Files actually touched, deduplicated, in observation order.
    pub changed_files: Vec<String>,
    /// The worktree's HEAD, so the next worker can read the previous attempt's
    /// commits. Empty when the worktree has no commit yet.
    pub head_sha: String,
}

impl WorkProgress {
    /// Whether anything was produced at all.
    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }

    /// A one-line handover note for the FAIL event.
    ///
    /// Deliberately concrete: "changed 3 files: a.js, b.rs, c.ts (head abc123)"
    /// is actionable, whereas "did some work" is not.
    pub fn handover_note(&self) -> String {
        if self.is_empty() {
            return "no files changed yet".to_string();
        }
        let mut files = self.changed_files.clone();
        files.sort();
        files.dedup();
        let shown: Vec<String> = files.iter().take(8).cloned().collect();
        let mut note = format!("changed {} file(s): {}", files.len(), shown.join(", "));
        if files.len() > shown.len() {
            note.push_str(", …");
        }
        if !self.head_sha.is_empty() {
            note.push_str(&format!(" (head {})", &self.head_sha[..self.head_sha.len().min(10)]));
        }
        note
    }
}

/// Count `git diff --numstat` output into (files, added, removed).
fn count_numstat(s: &str) -> (usize, i64, i64) {
    let mut files = 0;
    let mut add = 0;
    let mut del = 0;
    for line in s.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        files += 1;
        add += parts[0].trim().parse::<i64>().unwrap_or(0);
        del += parts[1].trim().parse::<i64>().unwrap_or(0);
    }
    (files, add, del)
}

fn retire_reason(v: &Verdict) -> Option<String> {
    match v {
        Verdict::Retire { reason, .. }
        | Verdict::Park { reason, .. }
        | Verdict::Stop { reason, .. } => Some(reason.clone()),
        _ => None,
    }
}

fn guard_of(v: &Verdict) -> GuardId {
    match v {
        Verdict::Retire { guard, .. } | Verdict::Park { guard, .. } | Verdict::Stop { guard, .. } => {
            *guard
        }
        _ => GuardId::HardTurns,
    }
}


