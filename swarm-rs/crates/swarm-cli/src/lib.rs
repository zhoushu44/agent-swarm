//! The runner: worker pool, autoscaler, integrator, renewer, reaper.
//!
//! This is the Go `cmd/swarm/runner.go` (1889 lines) in Rust. The file is large
//! because the *behaviour* is large: fourteen circuit breakers, each added after
//! a real run demonstrated the failure it prevents.
//!
//! ## The fourteen breakers
//!
//! | # | Guard | Trigger |
//! |---|---|
//! | 1 | idle suppression | nothing claimable and no claim of mine |
//! | 2 | budget gate | `tokensSpent > budgetTokens` |
//! | 3 | hard turns | `turn > maxTurns * hardTurnFactor` |
//! | 4 | no-claim-no-submit | `heldItem == "" && turn > maxItemTurns` |
//! | 5 | worktree conflict | syncSkew >= 8 retire, >= 3 redo |
//! | 6 | idle detection | reply says IDLE, or open==0 && no claim |
//! | 7 | no-progress guard | `!submitted && !idle` for 3 turns |
//! | 8 | empty-output | artifact fingerprint static for 3 turns |
//! | 9 | item hold | turns > maxItemTurns / tokens > maxItemTokens / held > maxItemSeconds |
//! | 10 | 503 backoff | 5s→10→…→60s, turn not consumed |
//! | 11 | autoscaler give-up | 3 consecutive stalled spawns |
//! | 12 | resource guard | CPU/Mem > 92% |
//! | 13 | exit-when-done | all seed items terminal |
//! | 14 | silence / wall clock | 10m / 6h |
//!
//! Every one of them has a test named `guard_NN_*` in `tests/breakers.rs`.

pub mod autoscaler;
pub mod breaker;
pub mod breaker_report;
pub mod compact;
pub mod runner;
pub mod sysload;
pub mod worker;

pub use autoscaler::{Autoscaler, ScaleDecision};
pub use breaker::{CircuitBreaker, GuardId, Verdict, World};
pub use breaker_report::BreakerReport;
pub use compact::{apply_plan, parse_plan, plan_compaction, verify_coverage, CompactPlan};
pub use breaker::hold_outran_clock;
pub use runner::{
    baseline_note_for_testing, progress_for_testing, CancellationToken, GateState, Runner,
    WorkProgress,
};
pub use sysload::{sample, LoadSample};
pub use worker::{ItemLedger, ItemLedgers, WorkerState, WorkerStatus};
