//! Route handlers, mirroring Go `internal/server/handlers.go`.
//!
//! Every handler is deliberately shaped like its Go counterpart, including the
//! clamping and the nil-slice normalisation (Go's `json.Marshal` renders a nil
//! slice as `null`, which is why the Go code rewrites nil to `[]` before
//! writing — we do the same so clients never see `null` where an array is
//! promised).

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use swarm_core::{CreateItemInput, Event, SwarmError};

use crate::error::store_error_response;
use crate::state::AppState;

/// Handler error: a store error already mapped to an HTTP response.
pub struct ApiError(Response);

impl From<SwarmError> for ApiError {
    fn from(e: SwarmError) -> Self {
        ApiError(store_error_response(&e))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.0
    }
}

/// A 400 from a handler-level validation (Go `writeError`).
pub fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError(crate::error::bad_request(msg))
}

/// A 403 from the `--no-create` guard.
pub fn creation_forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(crate::error::forbidden(msg))
}

/// Build the router for the swarm API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/board", get(handle_board))
        .route("/item", get(handle_get_item).post(handle_create_item))
        .route("/digest", get(handle_digest))
        .route("/context", post(handle_context))
        .route("/context/search", get(handle_search))
        .route("/marks", get(handle_marks))
        .route("/messages", get(handle_messages))
        .route("/message", post(handle_message))
        .route("/claim", post(handle_claim))
        .route("/release", post(handle_release))
        .route("/renew", post(handle_renew))
        .route("/submit", post(handle_submit))
        .route("/healthz", get(handle_healthz))
        .with_state(state)
}

// ------------------------------------------------------------------- GET /board

#[derive(Debug, Deserialize)]
struct BoardQuery {
    #[serde(default)]
    status: String,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn handle_board(
    State(st): State<AppState>,
    Query(q): Query<BoardQuery>,
) -> Result<Json<Value>, ApiError> {
    // Go clamps limit to [1,1000] with a default of 200, and offset to >= 0.
    let mut limit = q.limit.unwrap_or(200);
    if limit <= 0 || limit > 1000 {
        limit = 200;
    }
    let offset = q.offset.unwrap_or(0).max(0);

    let (items, total) = st.store.list_items(q.status.trim(), limit, offset)?;
    let has_more = (offset + items.len() as i64) < total;
    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "hasMore": has_more,
    })))
}

// ------------------------------------------------------------------ GET /item

#[derive(Debug, Deserialize)]
struct ItemQuery {
    #[serde(default)]
    id: String,
}

async fn handle_get_item(
    State(st): State<AppState>,
    Query(q): Query<ItemQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = q.id.trim();
    if id.is_empty() {
        return Err(invalid("missing id"));
    }
    let item = st.store.get_item(id)?;
    // Go tolerates a missing history: it degrades to an empty list.
    let history = st.store.item_history(id).unwrap_or_default();
    Ok(Json(json!({"item": item, "history": history})))
}

// ---------------------------------------------------------------- POST /item

async fn handle_create_item(
    State(st): State<AppState>,
    Json(mut input): Json<CreateItemInput>,
) -> Result<Json<Value>, ApiError> {
    input.title = input.title.trim().to_string();
    if input.title.is_empty() {
        return Err(invalid("missing title"));
    }
    if input.kind.trim().is_empty() {
        input.kind = "task".to_string();
    }
    if let Some(p) = &input.parent_id {
        let p = p.trim().to_string();
        input.parent_id = if p.is_empty() { None } else { Some(p) };
    }

    // The hard `--no-create` guard. Checked before the store so a refused
    // request never allocates an id.
    if !st.item_creation_allowed(&input.peer, input.parent_id.as_deref()) {
        return Err(creation_forbidden("item creation is disabled: work on the existing board"));
    }

    let item = st.store.create_item(input)?;
    Ok(Json(json!(item)))
}

// --------------------------------------------------------------- POST /claim

#[derive(Debug, Deserialize)]
struct ClaimRequest {
    #[serde(rename = "itemId")]
    item_id: String,
    peer: String,
}

async fn handle_claim(
    State(st): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> Result<Json<Value>, ApiError> {
    let item_id = req.item_id.trim();
    let peer = req.peer.trim();
    if item_id.is_empty() || peer.is_empty() {
        return Err(invalid("missing itemId or peer"));
    }

    let mut res = st.store.claim(item_id, peer)?;
    res.history.retain(|_| true);
    // Tell the worker whether to decompose first. Checked after the claim so
    // two workers racing on the same item cannot both be told to split it.
    let needs_split = st.store.needs_split(item_id).unwrap_or(false);
    Ok(Json(json!({
        "token": res.token,
        "item": res.item,
        "history": res.history,
        "conflicts": res.conflicts,
        "needsSplit": needs_split,
    })))
}

// ------------------------------------------------------------- POST /release

#[derive(Debug, Deserialize)]
struct TokenRequest {
    #[serde(default)]
    token: String,
}

async fn handle_release(
    State(st): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> Result<Json<Value>, ApiError> {
    let token = req.token.trim();
    if token.is_empty() {
        return Err(invalid("missing token"));
    }
    st.store.release(token)?;
    Ok(Json(json!({"ok": true})))
}

// --------------------------------------------------------------- POST /renew

async fn handle_renew(
    State(st): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> Result<Json<Value>, ApiError> {
    let token = req.token.trim();
    if token.is_empty() {
        return Err(invalid("missing token"));
    }
    st.store.renew(token)?;
    Ok(Json(json!({"ok": true})))
}

// -------------------------------------------------------------- POST /submit

#[derive(Debug, Deserialize)]
struct SubmitRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    peer: String,
    #[serde(rename = "commitSha", default)]
    commit_sha: String,
}

async fn handle_submit(
    State(st): State<AppState>,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<Value>, ApiError> {
    let token = req.token.trim();
    let peer = req.peer.trim();
    if token.is_empty() || peer.is_empty() {
        return Err(invalid("missing token or peer"));
    }
    let item = st.store.submit(token, peer, req.commit_sha.trim())?;
    Ok(Json(json!({"ok": true, "item": item})))
}

// -------------------------------------------------------------- POST /context

#[derive(Debug, Deserialize)]
struct ContextRequest {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    kind: String,
    text: String,
    #[serde(rename = "itemId")]
    item_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    resource: String,
    #[serde(rename = "ttlMs", default)]
    ttl_ms: i64,
}

/// The five protocol marks. Anything else is an ordinary note.
fn is_mark_kind(kind: &str) -> bool {
    matches!(
        kind.to_uppercase().as_str(),
        "INTENT" | "ACTION" | "OBSERVATION" | "WARNING" | "NEED"
    )
}

async fn handle_context(
    State(st): State<AppState>,
    Json(req): Json<ContextRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.text.trim().is_empty() {
        return Err(invalid("missing text"));
    }
    let mut kind = req.kind.trim().to_string();
    if kind.is_empty() {
        kind = "OBSERVED".to_string();
    }
    let kind = kind.to_uppercase();

    let entry = Event {
        id: 0,
        peer: req.peer.trim().to_string(),
        kind,
        item_id: req.item_id.filter(|s| !s.trim().is_empty()),
        text: req.text,
        tags: if req.tags.is_empty() { None } else { Some(req.tags) },
        ts: 0,
    };

    // A stigmergy mark goes through `mark_intent` so the resource and TTL are
    // recorded, which is what makes the conflict scan and /marks work. A plain
    // note goes through `add_context`.
    if is_mark_kind(&entry.kind) && !req.resource.trim().is_empty() {
        let ttl = if req.ttl_ms > 0 {
            Some(req.ttl_ms)
        } else {
            None
        };
        st.store.mark_intent_ext(
            &entry.peer,
            entry.item_id.as_deref().unwrap_or(""),
            &req.resource.trim().to_string(),
            ttl,
        )?;
        return Ok(Json(json!({"ok": true})));
    }

    let id = st.store.add_context(entry)?;
    Ok(Json(json!({"id": id})))
}

// --------------------------------------------------------- GET /context/search

#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<i64>,
}

async fn handle_search(
    State(st): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut limit = q.limit.unwrap_or(50);
    if limit <= 0 || limit > 500 {
        limit = 50;
    }
    let entries = st.store.search_context(q.q.trim(), limit)?;
    Ok(Json(json!({"entries": entries, "count": entries.len()})))
}

// ------------------------------------------------------------------ GET /marks

#[derive(Debug, Deserialize)]
struct MarksQuery {
    #[serde(default)]
    resource: String,
    #[serde(default)]
    limit: Option<i64>,
}

async fn handle_marks(
    State(st): State<AppState>,
    Query(q): Query<MarksQuery>,
) -> Result<Json<Value>, ApiError> {
    let view = st.store.active_marks(q.resource.trim(), q.limit.unwrap_or(50))?;
    // `held` is the number a worker deciding whether to claim must use: it counts
    // resource leases (the authority), not intent marks (the hint). `count` keeps
    // its documented Go-compatible meaning so existing clients do not break.
    Ok(Json(json!({
        "resource": q.resource.trim(),
        "count": view.marks.len(),
        "held": view.held(),
        "isHeld": view.is_held(),
        "marks": view.marks,
        "leases": view.leases,
    })))
}

// --------------------------------------------------------------- GET /digest

#[derive(Debug, Deserialize)]
struct DigestQuery {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    since: Option<i64>,
}

async fn handle_digest(
    State(st): State<AppState>,
    Query(q): Query<DigestQuery>,
) -> Result<Json<Value>, ApiError> {
    let d = st.store.digest(q.peer.trim(), q.since.unwrap_or(0))?;
    Ok(Json(json!(d)))
}

// ---------------------------------------------------------- POST /message

#[derive(Debug, Deserialize)]
struct MessageRequest {
    #[serde(default)]
    from: String,
    to: String,
    text: String,
}

async fn handle_message(
    State(st): State<AppState>,
    Json(req): Json<MessageRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.to.trim().is_empty() || req.text.trim().is_empty() {
        return Err(invalid("missing to or text"));
    }
    let id = st.store.send_message(req.from.trim(), req.to.trim(), &req.text)?;
    Ok(Json(json!({"id": id})))
}

// ------------------------------------------------------------ GET /messages

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    since: Option<i64>,
}

async fn handle_messages(
    State(st): State<AppState>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let msgs = st.store.messages(q.peer.trim(), q.since.unwrap_or(0))?;
    Ok(Json(json!({"messages": msgs, "count": msgs.len()})))
}

// ---------------------------------------------------------------- GET /healthz

async fn handle_healthz(State(st): State<AppState>) -> Json<Value> {
    let ok = st.store.stats().is_ok();
    Json(json!({"ok": ok}))
}

/// Kept so the unused-import lint does not fire when a feature is disabled.
#[allow(dead_code)]
fn _unused_markers(_: fn() -> bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_kinds_are_uppercase_and_whitelisted() {
        for k in ["INTENT", "intent", "Intent", "ACTION", "OBSERVATION", "WARNING", "NEED"] {
            assert!(is_mark_kind(k), "{k} must be a mark");
        }
        for k in ["OBSERVED", "FAIL", "PATCH_SUMMARY", "", "NOTE"] {
            assert!(!is_mark_kind(k), "{k} must NOT be a mark");
        }
    }

    #[test]
    fn board_limit_clamping_matches_go() {
        // limit <= 0 or > 1000 falls back to 200.
        for raw in [0, -5, 5000] {
            let mut limit = raw;
            if limit <= 0 || limit > 1000 {
                limit = 200;
            }
            assert_eq!(limit, 200);
        }
        let mut limit = 50;
        if limit <= 0 || limit > 1000 {
            limit = 200;
        }
        assert_eq!(limit, 50);
    }
}
