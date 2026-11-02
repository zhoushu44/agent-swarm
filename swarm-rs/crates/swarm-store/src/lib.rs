//! SQLite persistence layer, mirroring `internal/store` from the Go version.
//!
//! Two things are load-bearing and must not be "improved" away:
//!
//! 1. **The schema is byte-compatible with the Go version.** A `swarm.db`
//!    written by the Go binary must open here and vice versa, so the DDL below
//!    is the Go DDL (`internal/store/store.go` L53-115) plus the additive
//!    `ALTER TABLE`s the Go code performs on open.
//! 2. **`claim` accepts `in_progress` items.** An expired lease is reaped
//!    inside the claim transaction and the item is returned to `pending`.
//!    Rejecting `in_progress` at the door would leave an item stuck forever
//!    once its lease elapsed (Go claims.go L36-41 explains this at length).

pub mod resources;
pub mod schema;
pub mod store;

pub use store::Store;
