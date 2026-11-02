//! Shared types for swarm-rs.
//!
//! Everything here mirrors a Go type from `internal/server` (the wire protocol
//! package) so that a Go client and a Rust server agree byte-for-byte on JSON.
//!
//! Field names are the wire contract: they are `camelCase` because that is what
//! `internal/server` emits, and `serde` is configured per-struct to match.

use serde::{Deserialize, Serialize};

/// Item lifecycle states, mirrored from `internal/store/store.go` L38-49.
pub mod status {
    pub const PENDING: &str = "pending";
    pub const IN_PROGRESS: &str = "in_progress";
    pub const SUBMITTED: &str = "submitted";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    /// Decomposed into children; counts as terminal for progress accounting.
    pub const SPLIT: &str = "split";

    /// Terminal states: an item here is finished for seed-progress purposes.
    ///
    /// `split` is included deliberately — leaving it out froze seed progress at
    /// "2/4" on a run where all children had finished (see store.go L44-48).
    pub fn is_terminal(s: &str) -> bool {
        matches!(s, DONE | FAILED | SPLIT)
    }
}

/// Default claim lease in milliseconds (`store.DefaultLeaseMillis`).
pub const DEFAULT_LEASE_MILLIS: i64 = 300_000;

/// An item on the board.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    #[serde(default)]
    pub seq: i64,
}

fn default_kind() -> String {
    "task".to_string()
}
fn default_status() -> String {
    status::PENDING.to_string()
}

/// A live claim on an item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub token: String,
    pub item_id: String,
    pub peer: String,
    pub expires_at: i64,
}

/// Another peer already editing a resource this item names (stigmergy hint).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConflictInfo {
    pub resource: String,
    pub peer: String,
    pub item_id: String,
    pub remaining_ms: i64,
}

/// The task package handed to a worker on a successful claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimResult {
    pub token: String,
    pub item: Item,
    #[serde(default)]
    pub history: Vec<Event>,
    #[serde(default)]
    pub conflicts: Vec<ConflictInfo>,
}

/// A context / observation entry on the shared board.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: i64,
    pub peer: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub ts: i64,
}

/// Board counters returned by `/board` and used by the status line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub total: i64,
    pub pending: i64,
    pub in_progress: i64,
    pub submitted: i64,
    pub done: i64,
    pub failed: i64,
}

/// One queued integration request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    pub id: i64,
    pub item_id: String,
    pub peer: String,
    #[serde(default)]
    pub commit_sha: String,
    #[serde(default)]
    pub created_at: i64,
    /// queued / ok / rejected
    #[serde(default = "default_queued")]
    pub state: String,
    #[serde(default)]
    pub reason: String,
}

fn default_queued() -> String {
    "queued".to_string()
}

/// A live resource lease: the authoritative answer to "who holds this file".
///
/// Distinct from [`Event`] intent marks. A mark is a worker's *hint* about what
/// it plans to touch, with its own TTL, and it can outlive the work. A lease is
/// written in the same transaction that grants the claim, so its existence means
/// the resource really is held.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLease {
    pub resource: String,
    /// The claim token this lease belongs to.
    pub token: String,
    pub peer: String,
    pub expires_at: i64,
}

/// What `GET /marks` returns: the hint and the authority, kept separate.
///
/// `marks` keeps its Go-compatible meaning (intent marks). `leases` is the
/// authoritative half. A worker deciding whether to claim must look at `held`,
/// not `count` — polling only the marks and seeing zero is how the 304-worker
/// stress run ended up with blind retry loops.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarksView {
    pub marks: Vec<Event>,
    pub leases: Vec<ResourceLease>,
}

impl MarksView {
    /// Whether any peer actually holds this resource right now.
    pub fn is_held(&self) -> bool {
        !self.leases.is_empty()
    }

    /// Marks + leases, the number a caller comparing against "am I clear to
    /// claim?" should use.
    pub fn held(&self) -> usize {
        self.leases.len()
    }
}

/// One row of the append-only run log (`run_log` table).///
/// The fact layer of a run: what happened, to which item, at what cost, and
/// whether it was accepted. Summary numbers are derived from these rows, so a
/// report can always be recomputed but never fabricated.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunLogEntry {
    #[serde(default)]
    pub id: i64,
    /// e.g. `integration`, `rejected`, `split`.
    pub kind: String,
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub files: i64,
    #[serde(default)]
    pub seconds: i64,
    #[serde(default)]
    pub ts: i64,
    /// Whether this entry's work was accepted (passed the gate).
    #[serde(default)]
    pub accepted: bool,
    /// State-transition provenance: the field that changed.
    #[serde(default)]
    pub field: String,
    /// State-transition provenance: what the field was.
    #[serde(default)]
    pub old_value: String,
    /// State-transition provenance: what it became.
    #[serde(default)]
    pub new_value: String,
}

/// Payload for creating an item (`POST /item`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateItemInput {
    #[serde(default)]
    pub peer: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub priority: i64,
}

/// Per-worker digest handed out at the start of a turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest {
    pub now: i64,
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub new_context: Vec<Event>,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub pending_preview: Vec<Item>,
}

/// Sentinel errors the HTTP layer maps onto status codes.
///
/// Mirrors `internal/server`'s `ErrNotFound` / `ErrConflict` / `ErrInvalid`.
#[derive(Debug, thiserror::Error)]
pub enum SwarmError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("store: {0}")]
    Store(String),
}

impl SwarmError {
    /// HTTP status code for this error, matching the Go handlers.
    pub fn http_status(&self) -> u16 {
        match self {
            SwarmError::NotFound(_) => 404,
            SwarmError::Conflict(_) => 409,
            SwarmError::Invalid(_) => 400,
            SwarmError::Store(_) => 500,
        }
    }
}

pub type Result<T> = std::result::Result<T, SwarmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_json_is_camel_case_and_skips_nones() {
        let it = Item {
            id: "i1".into(),
            title: "t".into(),
            kind: "task".into(),
            status: "pending".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&it).unwrap();
        assert_eq!(v["id"], "i1");
        assert!(
            v.get("parentId").is_none(),
            "None must be omitted from the wire"
        );
        assert_eq!(v["kind"], "task");
        assert_eq!(v["status"], "pending");
    }

    /// `None` must be omitted from the wire, and absent fields must come back
    /// as their defaults — that is how a Go client omitting `kind` still lands
    /// on "task".
    #[test]
    fn item_deserializes_with_defaults() {
        let it: Item = serde_json::from_str(r#"{"id":"i1","title":"t"}"#).unwrap();
        assert_eq!(it.kind, "task");
        assert_eq!(it.status, "pending");
        assert_eq!(it.parent_id, None);
        assert_eq!(it.detail, "");
    }

    #[test]
    fn split_is_terminal() {
        assert!(status::is_terminal(status::SPLIT));
        assert!(status::is_terminal(status::DONE));
        assert!(!status::is_terminal(status::PENDING));
    }

    #[test]
    fn error_status_codes_match_go_handlers() {
        assert_eq!(SwarmError::NotFound("x".into()).http_status(), 404);
        assert_eq!(SwarmError::Conflict("x".into()).http_status(), 409);
        assert_eq!(SwarmError::Invalid("x".into()).http_status(), 400);
    }
}
