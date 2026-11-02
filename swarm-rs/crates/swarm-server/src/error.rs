//! Error → HTTP status mapping, matching Go `writeStoreError` / `writeError`.
//!
//! The three sentinels are the contract: 404 not found, 409 conflict (a claim
//! someone else took — "races are normal, not errors"), 400 invalid input.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A body describing why a request failed.
#[derive(Debug, serde::Serialize)]
pub struct ErrorBody {
    pub error: String,
}

/// Map a `SwarmError` onto the same status codes the Go handlers use.
///
/// The three sentinels are the contract: 404 not found, 409 conflict (a claim
/// someone else took — "races are normal, not errors"), 400 invalid input.
pub fn store_error_response(err: &swarm_core::SwarmError) -> Response {
    let (status, msg) = match err {
        swarm_core::SwarmError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
        swarm_core::SwarmError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
        swarm_core::SwarmError::Invalid(m) => (StatusCode::BAD_REQUEST, m.clone()),
        swarm_core::SwarmError::Store(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
    };
    // 5xx must not leak internals to the caller.
    let msg = if status.is_server_error() {
        "internal error".to_string()
    } else {
        msg
    };
    (status, Json(ErrorBody { error: msg })).into_response()
}

/// A 400 with a plain message (Go `writeError`).
pub fn bad_request(msg: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: msg.into(),
        }),
    )
        .into_response()
}

/// A 403, used when `--no-create` forbids new top-level work.
pub fn forbidden(msg: impl Into<String>) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody {
            error: msg.into(),
        }),
    )
        .into_response()
}

/// A 501, used when a store does not implement an optional capability.
pub fn not_implemented(msg: impl Into<String>) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ErrorBody {
            error: msg.into(),
        }),
    )
        .into_response()
}

/// A 200 with an arbitrary JSON body (Go `writeJSON`).
pub fn ok_json<T: serde::Serialize>(body: T) -> Response {
    Json(body).into_response()
}

/// A 200 with `{"ok":true}`.
pub fn ok_flag() -> Response {
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(r: Response) -> StatusCode {
        r.status()
    }

    #[test]
    fn error_status_codes_match_go() {
        assert_eq!(
            status_of(store_error_response(&swarm_core::SwarmError::NotFound(
                "x".into()
            ))),
            404
        );
        assert_eq!(
            status_of(store_error_response(&swarm_core::SwarmError::Conflict(
                "x".into()
            ))),
            409
        );
        assert_eq!(
            status_of(store_error_response(&swarm_core::SwarmError::Invalid(
                "x".into()
            ))),
            400
        );
        assert_eq!(
            status_of(store_error_response(&swarm_core::SwarmError::Store(
                "db exploded".into()
            ))),
            500
        );
    }
}


