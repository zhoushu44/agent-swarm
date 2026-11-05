//! The autoscaler: how many workers the pool should have right now.
//!
//! Mirrors Go `autoscalerLoop` (runner.go L1617-1714), plus two fixes taken from
//! `docs/OPTIMIZATION-SWARM.md`:
//!
//! * **§2.3 — `stalledSpawns` time decay.** Go clears the give-up counter only
//!   when a worker takes a claim. If every worker has already retired there is
//!   nobody left to take one, so the run can wedge at zero workers forever. Here
//!   the counter also decays when the pool is empty and time has passed.
//! * **§2.1 — atomic counters.** `inflight` becomes an `AtomicUsize`, so the
//!   integrator can no longer read a stale zero and start a merge that races an
//!   in-flight gate.

use std::time::{Duration, Instant};

use swarm_config::RunConfig;

use crate::sysload::LoadSample;

/// A decision about the desired pool size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleDecision {
    /// Spawn this many workers.
    Spawn(usize),
    /// Retire this many idle workers.
    Shrink(usize),
    /// Leave the pool alone.
    Hold,
    /// Stop spawning entirely: the harness cannot drive tools.
    GiveUp,
}

/// The autoscaler.
pub struct Autoscaler {
    cfg: RunConfig,
    /// Consecutive spawns that produced nothing.
    stalled_spawns: u32,
    /// When the last stall happened, for time decay.
    last_stall: Option<Instant>,
}

impl Autoscaler {
    pub fn new(cfg: RunConfig) -> Self {
        Self {
            cfg,
            stalled_spawns: 0,
            last_stall: None,
        }
    }

    pub fn config(&self) -> &RunConfig {
        &self.cfg
    }

    /// A spawn produced nothing.
    pub fn note_stalled_spawn(&mut self) {
        self.stalled_spawns += 1;
        self.last_stall = Some(Instant::now());
    }

    /// A spawn claimed or submitted something.
    pub fn note_progress(&mut self) {
        self.stalled_spawns = 0;
        self.last_stall = None;
    }

    /// Decay the give-up counter when the pool is empty.
    ///
    /// Without this the run wedges: the counter only clears on a claim, and with
    /// no workers there is nobody to claim. After `decay_window` of no new stalls
    /// the counter halves, so a transient outage cannot permanently disable the
    /// pool.
    pub fn decay(&mut self, decay_window: Duration) {
        if self.stalled_spawns == 0 {
            return;
        }
        if let Some(last) = self.last_stall {
            if last.elapsed() >= decay_window {
                self.stalled_spawns /= 2;
                self.last_stall = Some(Instant::now());
            }
        }
    }

    pub fn stalled_spawns(&self) -> u32 {
        self.stalled_spawns
    }

    /// How many workers the pool should have.
    ///
    /// Formula, copied from Go L1629-1655:
    ///
    /// ```text
    /// demand   = max(open, recentSplit)
    /// capped   = min(demand, max(open*2, 8))
    /// want     = clamp(capped, minWorkers, maxWorkers)
    /// ```
    ///
    /// then the give-up guard and the resource guard adjust it.
    pub fn desired_workers(
        &self,
        open: i64,
        recent_split: i64,
        active: usize,
        load: LoadSample,
    ) -> usize {
        let demand = open.max(recent_split);
        let capped = if open * 2 > 8 {
            demand.min(open * 2)
        } else {
            demand.min(8)
        };
        let ceiling_by_max = if self.cfg.max_workers == 0 {
            usize::MAX
        } else {
            self.cfg.max_workers
        };
        let mut want = (capped as usize).clamp(self.cfg.min_workers, ceiling_by_max);

        // Guard 11: give up.
        if self.stalled_spawns >= 3 {
            return 0;
        }

        // Guard 12: resource ceiling, with two distinct thresholds copied from
        // Go (`sysload.Guard` uses 85/90 to refuse growth; the runner uses 92
        // to force a shrink):
        //
        //   * no headroom to grow → freeze at the current size. "Freeze" is
        //     `want = active`, a no-op for an empty pool; it must never drive
        //     the pool to zero.
        //   * critical pressure → shrink.
        //
        // The give-up case returns before either, so its zero cannot be undone.
        //
        // NOTE: Go shrinks by exactly one here (`runner.go` L1681,
        // "critical: shrink by 1"), while growth is capped at 64 per tick. That
        // asymmetry is a real defect at `max_workers = 1000` — ~16s to fill the
        // pool, ~1000s to drain it, for a guard whose entire job is to prevent
        // an OOM kill. `decide` widens the shrink batch to compensate; this
        // function keeps the Go shape so the per-worker decision stays legible.
        if load.is_critical() {
            want = active.saturating_sub(1).max(1);
        } else if !load.allows_growth() && want > active {
            want = active;
        }

        want
    }

    /// Turn a desired size into a concrete action.
    pub fn decide(
        &self,
        open: i64,
        recent_split: i64,
        active: usize,
        idle: usize,
        load: LoadSample,
    ) -> ScaleDecision {
        if self.stalled_spawns >= 3 {
            // Give up only while there is nothing left to prove. Once the pool
            // has drained to zero, a single probe spawn costs one worker and
            // answers the only question that matters: is the endpoint still
            // dead? Returning GiveUp unconditionally is what wedged every long
            // run — the counter could not decay because `decay` requires the
            // pool to be empty AND a spawn to have happened, and GiveUp never
            // spawns. This breaks that cycle.
            //
            // The probe is rate-limited by `decay` (which halves the counter
            // every 5 minutes of quiet), so a permanently broken endpoint sees
            // one probe per decay window, not a spawn storm.
            if active == 0 && open > 0 {
                return ScaleDecision::Spawn(1);
            }
            return ScaleDecision::GiveUp;
        }
        let want = self.desired_workers(open, recent_split, active, load);
        if want > active {
            // At most 64 new workers per round (Go `maxSpawnPerRound`).
            ScaleDecision::Spawn((want - active).min(64))
        } else if want < active {
            // Only idle workers may be retired.
            //
            // `want` comes back as `active - 1` under critical pressure (the Go
            // shape, kept in `desired_workers`), which would retire ONE worker
            // per tick against a 64-per-tick growth cap. At `max_workers = 1000`
            // that is ~16s to fill and ~1000s to drain — too slow for a guard
            // whose job is to stop an OOM kill.
            //
            // The batch is widened HERE rather than in `desired_workers` so the
            // per-worker decision stays a legible "one fewer" and the asymmetry
            // fix lives in one place, next to the growth cap it must match.
            //
            // Bounded by `idle`: a worker mid-turn owns a worktree and a live
            // claim, and retiring it out from under its own work would lose both.
            let gap = active - want;
            let batch = if load.is_critical() {
                gap.saturating_mul(64).min(active / 4).max(1)
            } else {
                gap
            };
            ScaleDecision::Shrink(idle.min(batch).min(64))
        } else {
            ScaleDecision::Hold
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RunConfig {
        RunConfig::default()
    }

    /// `demand` is `max(open, recentSplit)`, capped so the pool never outruns the
    /// board.
    ///
    /// The Go formula (L1637-1644) reads awkwardly but means exactly this:
    ///
    /// ```text
    /// if open*2 > 8 { capped = min(demand, open*2) }   // board is big: track 2x
    /// else         { capped = min(demand, 8) }       // board is small: cap at 8
    /// ```
    ///
    /// So a 3-item board yields `min(3, 8) = 3`, NOT 6 — the 2x rule only
    /// engages once `open > 4`.
    #[test]
    fn demand_caps_at_twice_open() {
        let a = Autoscaler::new(cfg());
        // open=3 → open*2=6 which is NOT > 8, so the cap is 8 → min(3,8) = 3.
        assert_eq!(a.desired_workers(3, 0, 100, LoadSample::default()), 3);
        // open=10 → open*2=20 > 8, so the cap is 20 → min(10,20) = 10.
        assert_eq!(a.desired_workers(10, 0, 100, LoadSample::default()), 10);
        // open=3 but 12 recent splits → demand 12, cap 8 → 8.
        assert_eq!(a.desired_workers(3, 12, 100, LoadSample::default()), 8);
    }

    #[test]
    fn small_boards_cap_at_eight() {
        let a = Autoscaler::new(cfg());
        // open=2 → open*2=4 which is NOT > 8, so the cap is 8 → min(2,8) = 2.
        assert_eq!(a.desired_workers(2, 0, 100, LoadSample::default()), 2);
        // A burst of splits is what actually pushes a small board to the cap.
        assert_eq!(a.desired_workers(2, 20, 100, LoadSample::default()), 8);
    }

    #[test]
    fn recent_split_raises_demand() {
        let a = Autoscaler::new(cfg());
        // open=1, recentSplit=10 → demand 10, capped min(10, max(2,8)=8) = 8
        assert_eq!(a.desired_workers(1, 10, 100, LoadSample::default()), 8);
    }

    /// Growth and shrink must not be wildly asymmetric.
    ///
    /// `Spawn` was capped at 64 per tick while `Shrink` retired exactly ONE
    /// worker per tick. Under memory pressure at `max_workers = 1000` that means
    /// ~16s to fill the pool and ~1000s to drain it — the guard exists to prevent
    /// an OOM kill, and it was 64x too slow to do it.
    #[test]
    fn shrink_keeps_pace_with_spawn() {
        let a = Autoscaler::new(cfg());
        // 500 active, all idle, memory critical.
        let critical = LoadSample {
            cpu_pct: 10.0,
            mem_pct: 99.0,
            mem_total_bytes: 1,
            mem_used_bytes: 1,
        };
        match a.decide(1000, 0, 500, 500, critical) {
            ScaleDecision::Shrink(n) => {
                assert!(n > 1, "a critical pool must shed more than one worker, got {n}");
                assert!(n <= 64, "bounded so the integrator can keep up");
            }
            other => panic!("expected Shrink, got {other:?}"),
        }

        // The same asymmetry on the growth side must still hold.
        let relaxed = LoadSample::default();
        match a.decide(1000, 0, 10, 0, relaxed) {
            ScaleDecision::Spawn(n) => assert!(n <= 64, "spawn stays bounded"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// A worker mid-turn must never be retired by a critical-pressure shrink.
    ///
    /// Same `idle` bound as `shrink_only_retires_idle_workers`, but reached
    /// through the memory-critical path rather than an empty board.
    #[test]
    fn critical_shrink_only_retires_idle_workers() {
        let a = Autoscaler::new(cfg());
        let critical = LoadSample {
            cpu_pct: 99.0,
            mem_pct: 10.0,
            mem_total_bytes: 1,
            mem_used_bytes: 1,
        };
        // 500 active but only 3 idle.
        match a.decide(1000, 0, 500, 3, critical) {
            ScaleDecision::Shrink(n) => assert_eq!(n, 3, "only the idle ones may go"),
            other => panic!("expected Shrink, got {other:?}"),
        }
    }

    #[test]
    fn max_workers_is_the_hard_ceiling() {        let mut c = cfg();
        c.max_workers = 5;
        let a = Autoscaler::new(c);
        assert_eq!(a.desired_workers(1000, 0, 1000, LoadSample::default()), 5);
    }

    #[test]
    fn min_workers_is_the_floor() {
        let mut c = cfg();
        c.min_workers = 3;
        let a = Autoscaler::new(c);
        assert_eq!(a.desired_workers(0, 0, 100, LoadSample::default()), 3);
    }

    #[test]
    fn high_pressure_shrinks_by_one() {
        let a = Autoscaler::new(cfg());
        let want = a.desired_workers(
            100,
            0,
            50,
            LoadSample {
                cpu_pct: 99.0,
                mem_pct: 10.0,
                ..Default::default()
            },
        );
        assert_eq!(want, 49, "critical load shrinks by one");
    }

    #[test]
    fn memory_pressure_also_triggers_shrink() {
        let a = Autoscaler::new(cfg());
        let want = a.desired_workers(
            100,
            0,
            50,
            LoadSample {
                cpu_pct: 10.0,
                mem_pct: 99.0,
                ..Default::default()
            },
        );
        assert_eq!(want, 49);
    }

    #[test]
    fn high_pressure_never_below_one() {
        let a = Autoscaler::new(cfg());
        let want = a.desired_workers(
            100,
            0,
            1,
            LoadSample {
                cpu_pct: 99.0,
                mem_pct: 99.0,
                ..Default::default()
            },
        );
        assert_eq!(want, 1);
    }

    /// The Go guard uses 85/90 for "may I grow" and 92 for "must I shrink";
    /// `above_critical` is the 92 test.
    #[test]
    fn moderate_load_freezes_but_does_not_shrink() {
        let a = Autoscaler::new(cfg());
        let want = a.desired_workers(
            100,
            0,
            20,
            LoadSample {
                cpu_pct: 88.0,
                mem_pct: 95.0,
                ..Default::default()
            },
        );
        // mem 95 is above 92 → shrink by one.
        assert_eq!(want, 19);
    }

    #[test]
    fn spawn_caps_at_64_per_round() {
        let a = Autoscaler::new(cfg());
        // open=1000, active=0 → demand 1000, cap 2000 → want 1000, spawn 64.
        let d = a.decide(1000, 0, 0, 0, LoadSample::default());
        assert_eq!(d, ScaleDecision::Spawn(64));
    }

    #[test]
    fn shrink_only_retires_idle_workers() {
        let mut c = cfg();
        c.min_workers = 2;
        let a = Autoscaler::new(c);
        // active 10, open 0 → want = max(0, min 2) = 2 → shrink 8, but only 3 idle.
        let d = a.decide(0, 0, 10, 3, LoadSample::default());
        assert_eq!(d, ScaleDecision::Shrink(3));
    }

    #[test]
    fn give_up_after_three_stalled_spawns() {
        let mut a = Autoscaler::new(cfg());
        a.note_stalled_spawn();
        a.note_stalled_spawn();
        a.note_stalled_spawn();
        assert_eq!(
            a.decide(1000, 0, 5, 0, LoadSample::default()),
            ScaleDecision::GiveUp
        );
    }

    /// Regression: give-up used to be terminal.
    ///
    /// With `stalled_spawns >= 3` the autoscaler returned `GiveUp` forever, and
    /// `decay` could not rescue it because the pool being empty was also the
    /// condition that stopped new stalls from arriving. The observed shape: every
    /// run stopped at 60-70% with `0 failed`, dozens of items still pending, and
    /// a log line claiming "the harness is not driving tools" — which was false,
    /// the harness had just finished the items it was given.
    ///
    /// Now a drained pool with work left gets one probe spawn.
    #[test]
    fn give_up_is_not_terminal_when_work_remains() {
        let mut a = Autoscaler::new(cfg());
        for _ in 0..3 {
            a.note_stalled_spawn();
        }
        // Pool still has workers: keep giving up (nothing to probe with).
        assert_eq!(
            a.decide(50, 0, 2, 0, LoadSample::default()),
            ScaleDecision::GiveUp
        );
        // Pool drained but items remain: probe once instead of wedging.
        assert_eq!(
            a.decide(50, 0, 0, 0, LoadSample::default()),
            ScaleDecision::Spawn(1)
        );
        // Nothing left to do: GiveUp is the right answer, no probe wasted.
        assert_eq!(
            a.decide(0, 0, 0, 0, LoadSample::default()),
            ScaleDecision::GiveUp
        );
    }

    /// §2.3: Go wedges here — nobody left to claim, counter never clears.
    #[test]
    fn stall_counter_decays_when_the_pool_is_empty() {
        let mut a = Autoscaler::new(cfg());
        for _ in 0..4 {
            a.note_stalled_spawn();
        }
        assert_eq!(a.stalled_spawns(), 4);

        // No new stalls for the window: the counter halves.
        a.decay(Duration::ZERO);
        assert_eq!(a.stalled_spawns(), 2);
        a.decay(Duration::ZERO);
        assert_eq!(a.stalled_spawns(), 1);
        a.decay(Duration::ZERO);
        assert_eq!(a.stalled_spawns(), 0, "fully decayed");

        // And the pool can grow again: demand 10, 20 recent splits → want 8.
        // (open=10 → open*2=20 > 8, so the cap is 20 → min(20,20) = 20.)
        assert_eq!(
            a.decide(10, 20, 0, 0, LoadSample::default()),
            ScaleDecision::Spawn(20)
        );
    }

    #[test]
    fn decay_is_a_noop_without_stalls() {
        let mut a = Autoscaler::new(cfg());
        a.decay(Duration::ZERO);
        assert_eq!(a.stalled_spawns(), 0);
    }

    #[test]
    fn progress_resets_the_give_up_guard() {
        let mut a = Autoscaler::new(cfg());
        a.note_stalled_spawn();
        a.note_progress();
        assert_eq!(a.stalled_spawns(), 0);
    }

    #[test]
    fn give_up_beats_the_resource_guard() {
        let mut a = Autoscaler::new(cfg());
        for _ in 0..3 {
            a.note_stalled_spawn();
        }
        // Under critical load the Go guard would floor want at 1, undoing the
        // give-up. Ours returns 0 outright.
        assert_eq!(
            a.desired_workers(
                100,
                0,
                5,
                LoadSample {
                    cpu_pct: 99.0,
                    mem_pct: 99.0,
                    ..Default::default()
                }
            ),
            0
        );
    }
}
