//! The HTTP API workers talk to.
//!
//! Mirrors Go `internal/server`: one route per resource, JSON in/out, and a
//! small error vocabulary that maps store errors onto the same status codes the
//! Go handlers use (404 / 409 / 400 / 500).
//!
//! Route surface (byte-identical paths — the model is told these in the
//! prompt, so they are a contract, not an implementation detail):
//!
//! ```text
//! GET  /board              GET  /item        POST /item
//! GET  /digest             POST /context     GET  /context/search
//! GET  /marks              GET  /messages    POST /message
//! POST /claim              POST /release     POST /renew      POST /submit
//! GET  /healthz
//! ```

pub mod error;
pub mod routes;
pub mod state;

pub use routes::router;
pub use state::{AppState, ServerConfig};
