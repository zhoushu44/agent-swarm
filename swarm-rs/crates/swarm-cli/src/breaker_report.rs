//! Which breaker fired, and how often — the missing observability layer.
//!
//! Go logs a breaker firing as a `WORKER_PARK` / `WORKER_RETIRE` event with free
//! text, so answering "why did that worker die?" means grepping and guessing.
//! [`BreakerReport`] keeps a counter per [`GuardId`] and renders a one-line
//! summary at the end of a run.
//!
//! This is the groundwork for `docs/OPTIMIZATION-SWARM.md` §3.3 (token
//! efficiency in the report): the same mechanism carries cost per guard.

use std::collections::BTreeMap;

use crate::breaker::GuardId;

/// Counts of every breaker firing during a run.
#[derive(Debug, Default, Clone)]
pub struct BreakerReport {
    counts: BTreeMap<&'static str, u64>,
    /// Last reason seen per guard, so the summary can show an example.
    last_reason: BTreeMap<&'static str, String>,
}

impl BreakerReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one firing.
    pub fn record(&mut self, guard: GuardId, reason: impl Into<String>) {
        *self.counts.entry(guard.as_str()).or_insert(0) += 1;
        self.last_reason
            .insert(guard.as_str(), reason.into());
    }

    pub fn count(&self, guard: GuardId) -> u64 {
        self.counts.get(guard.as_str()).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// The guard that fired most often, if any.
    pub fn worst(&self) -> Option<(&'static str, u64)> {
        self.counts
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(k, n)| (*k, *n))
    }

    /// A one-line summary, e.g.
    /// `breakers: no-progress=4 empty-output=1 item-hold=7 (total 12)`.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "breakers: none".to_string();
        }
        let parts: Vec<String> = self
            .counts
            .iter()
            .map(|(k, n)| format!("{}={}", k, n))
            .collect();
        format!("breakers: {} (total {})", parts.join(" "), self.total())
    }

    /// A multi-line breakdown with the last reason per guard.
    pub fn render_verbose(&self) -> String {
        if self.is_empty() {
            return "breakers: none".to_string();
        }
        let mut out = String::new();
        for (k, n) in &self.counts {
            out.push_str(&format!("  {:<22} {:>4}", k, n));
            if let Some(r) = self.last_reason.get(*k) {
                let short: String = r.chars().take(60).collect();
                out.push_str(&format!("   e.g. {}", short));
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_renders_none() {
        let r = BreakerReport::new();
        assert!(r.is_empty());
        assert_eq!(r.render(), "breakers: none");
        assert_eq!(r.worst(), None);
    }

    #[test]
    fn counts_per_guard() {
        let mut r = BreakerReport::new();
        r.record(GuardId::NoProgress, "3 turns");
        r.record(GuardId::NoProgress, "3 turns");
        r.record(GuardId::EmptyOutput, "static fingerprint");
        assert_eq!(r.count(GuardId::NoProgress), 2);
        assert_eq!(r.count(GuardId::EmptyOutput), 1);
        assert_eq!(r.count(GuardId::ItemHold), 0);
        assert_eq!(r.total(), 3);
    }

    #[test]
    fn worst_picks_the_most_frequent() {
        let mut r = BreakerReport::new();
        r.record(GuardId::NoProgress, "x");
        for _ in 0..7 {
            r.record(GuardId::ItemHold, "over the line");
        }
        assert_eq!(r.worst(), Some(("item-hold", 7)));
    }

    #[test]
    fn render_lists_every_guard() {
        let mut r = BreakerReport::new();
        r.record(GuardId::NoProgress, "x");
        r.record(GuardId::EmptyOutput, "y");
        let s = r.render();
        assert!(s.contains("no-progress=1"));
        assert!(s.contains("empty-output=1"));
        assert!(s.contains("total 2"));
    }

    #[test]
    fn verbose_shows_the_last_reason() {
        let mut r = BreakerReport::new();
        r.record(GuardId::NoProgress, "3 turns with no submission");
        let v = r.render_verbose();
        assert!(v.contains("no-progress"));
        assert!(v.contains("3 turns with no submission"));
    }

    #[test]
    fn reasons_are_truncated() {
        let mut r = BreakerReport::new();
        let long = "x".repeat(500);
        r.record(GuardId::ItemHold, long);
        let v = r.render_verbose();
        assert!(v.len() < 300, "long reasons must be truncated");
    }
}
