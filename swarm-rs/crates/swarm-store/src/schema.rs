//! Schema DDL, copied from Go `internal/store/store.go` L53-115.
//!
//! The Go code runs this on every open with `CREATE TABLE IF NOT EXISTS`, then
//! performs additive `ALTER TABLE ... ADD COLUMN` migrations that tolerate
//! "duplicate column name". We keep the same shape so an existing database is
//! upgraded in place, but drive it from an explicit version number so a partial
//! migration is visible instead of silently skipped.

use rusqlite::Connection;

/// Base schema: identical to the Go `schema` constant.
pub const BASE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
	id         TEXT PRIMARY KEY,
	parent_id  TEXT,
	title      TEXT NOT NULL,
	detail     TEXT NOT NULL DEFAULT '',
	kind       TEXT NOT NULL DEFAULT 'task',
	priority   INTEGER NOT NULL DEFAULT 0,
	status     TEXT NOT NULL DEFAULT 'pending',
	created_by TEXT NOT NULL DEFAULT '',
	created_at INTEGER NOT NULL DEFAULT 0,
	done_at    INTEGER,
	not_before INTEGER,
	seq        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS items_status_idx ON items(status);
CREATE INDEX IF NOT EXISTS items_parent_idx ON items(parent_id);

CREATE TABLE IF NOT EXISTS claims (
	token      TEXT PRIMARY KEY,
	item_id    TEXT NOT NULL,
	peer       TEXT NOT NULL DEFAULT '',
	expires_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS claims_item_idx ON claims(item_id);
CREATE INDEX IF NOT EXISTS claims_peer_idx ON claims(peer);

CREATE TABLE IF NOT EXISTS submissions (
	id         INTEGER PRIMARY KEY AUTOINCREMENT,
	item_id    TEXT NOT NULL,
	peer       TEXT NOT NULL DEFAULT '',
	commit_sha TEXT NOT NULL DEFAULT '',
	created_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS submissions_item_idx ON submissions(item_id);

CREATE TABLE IF NOT EXISTS events (
	id      INTEGER PRIMARY KEY AUTOINCREMENT,
	peer    TEXT NOT NULL DEFAULT '',
	kind    TEXT NOT NULL DEFAULT 'OBSERVED',
	item_id TEXT,
	text    TEXT NOT NULL DEFAULT '',
	tags    TEXT,
	ts      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS events_item_idx ON events(item_id);
CREATE INDEX IF NOT EXISTS events_ts_idx ON events(ts);

CREATE TABLE IF NOT EXISTS messages (
	id      INTEGER PRIMARY KEY AUTOINCREMENT,
	"from"  TEXT NOT NULL DEFAULT '',
	"to"    TEXT NOT NULL DEFAULT '',
	text    TEXT NOT NULL DEFAULT '',
	ts      INTEGER NOT NULL DEFAULT 0,
	read    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_to_idx ON messages("to");

CREATE TABLE IF NOT EXISTS counters (
	name  TEXT PRIMARY KEY,
	value INTEGER NOT NULL DEFAULT 0
);
"#;

/// `run_log` table, from Go `internal/store/runlog.go` L71-83.
///
/// The append-only fact layer: every accepted or rejected integration lands here
/// with its cost, so a run can be audited and re-derived after the fact. Go
/// never removes a row and neither does Rust — this is the "L4 audit log" of the
/// Super-agent design exploration (`docs/SUPER-AGENT-IMPROVEMENT-PLAN.md` §5):
/// the summary numbers are derived views, this is the authority.
pub const RUN_LOG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS run_log (
	id      INTEGER PRIMARY KEY AUTOINCREMENT,
	kind    TEXT NOT NULL,
	actor   TEXT NOT NULL DEFAULT '',
	item_id TEXT,
	title   TEXT,
	detail  TEXT,
	tokens  INTEGER NOT NULL DEFAULT 0,
	files   INTEGER NOT NULL DEFAULT 0,
	seconds INTEGER NOT NULL DEFAULT 0,
	ts      INTEGER NOT NULL DEFAULT 0,
	accepted INTEGER NOT NULL DEFAULT 0,
	old_value TEXT NOT NULL DEFAULT '',
	new_value TEXT NOT NULL DEFAULT '',
	field    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS run_log_kind_idx ON run_log(kind);
CREATE INDEX IF NOT EXISTS run_log_item_idx ON run_log(item_id);
CREATE INDEX IF NOT EXISTS run_log_ts_idx ON run_log(ts);
"#;

/// The cross-run memory store, kept in its own database file.
///
/// Deliberately NOT part of `BASE_SCHEMA`: the project `swarm.db` is disposable
/// per run and must stay compatible with the Go binary, while memory has to
/// outlive every run and every project. Two lifetimes, two files.
///
/// `scope` is free-form so one file can serve several unrelated work streams
/// without them bleeding into each other:
///
///   * `project:<abs path>` — what was learned working in one repo
///   * `tag:<name>`         — cross-project lessons ("playwright", "1688")
///   * `global`             — operator preferences, tool quirks
///
/// Rows are written once and read many times, so `hits` / `last_hit_at` exist to
/// let `prune` drop notes nobody ever looked at again.
pub const MEMORY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
	id          INTEGER PRIMARY KEY AUTOINCREMENT,
	scope       TEXT NOT NULL,
	kind        TEXT NOT NULL DEFAULT 'FACT',
	text        TEXT NOT NULL,
	tags        TEXT,
	source      TEXT NOT NULL DEFAULT '',
	created_at  INTEGER NOT NULL DEFAULT 0,
	hits        INTEGER NOT NULL DEFAULT 0,
	last_hit_at INTEGER
);
CREATE INDEX IF NOT EXISTS memories_scope_idx ON memories(scope);
CREATE INDEX IF NOT EXISTS memories_kind_idx ON memories(kind);
CREATE INDEX IF NOT EXISTS memories_created_idx ON memories(created_at);
"#;

/// Additive columns the Go version adds after the base schema.
///
/// Each entry is `(table, column, definition)`. Applied idempotently by checking `PRAGMA table_info` first, which is more predictable than relying on
/// the driver's duplicate-column error message.
const ADDITIVE_COLUMNS: &[(&str, &str, &str)] = &[
    ("events", "mark_type", "TEXT"),
    ("events", "resource", "TEXT"),
    ("events", "ttl_ms", "INTEGER"),
    (
        "submissions",
        "state",
        "TEXT NOT NULL DEFAULT 'queued'",
    ),
    ("submissions", "reason", "TEXT NOT NULL DEFAULT ''"),
    // Whether the integration this row records was accepted. `NOT NULL DEFAULT 0`
    // so a Go-written row (which has no such column) upgrades in place to
    // "unknown/not recorded" rather than NULL, keeping the aggregate query total.
    ("run_log", "accepted", "INTEGER NOT NULL DEFAULT 0"),
    // State-transition provenance (agent-fleet-manager's change_log shape):
    // what a field was, what it became, and why. `detail` is already taken by
    // the content identity, so these get their own columns.
    ("run_log", "old_value", "TEXT NOT NULL DEFAULT ''"),
    ("run_log", "new_value", "TEXT NOT NULL DEFAULT ''"),
    ("run_log", "field", "TEXT NOT NULL DEFAULT ''"),
];

const ADDITIVE_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS events_resource_idx ON events(resource)",
    "CREATE INDEX IF NOT EXISTS events_mark_idx ON events(mark_type)",
    "CREATE INDEX IF NOT EXISTS submissions_state_idx ON submissions(state)",
];

/// Resource-level leases, added by the Super-agent upgrade
/// (`docs/SUPER-AGENT-IMPROVEMENT-PLAN.md` §2).
///
/// The Go version locks *items*: `claims(token, item_id, peer, expires_at)`. That
/// is the wrong granularity when two items name the same file — both workers
/// claim, both edit, and the merge is where the collision finally shows up.
///
/// This table locks the *resource* the item names, keyed on the same normalized
/// token `resources::item_resources` produces, so the two can never disagree.
/// `token` is the primary key so releasing or reaping a claim cascades by that
/// one key.
///
/// Go opens this database with `CREATE TABLE IF NOT EXISTS` for the tables it
/// knows; an unknown table is simply ignored, so adding this is compatible with
/// the Go `swarm.db` (README constraint 1).
pub const RESOURCE_LEASE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS resource_leases (
    resource   TEXT NOT NULL,
    token      TEXT NOT NULL,
    peer       TEXT NOT NULL,
    expires_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (resource, token)
);
CREATE INDEX IF NOT EXISTS resource_leases_res_idx ON resource_leases(resource);
CREATE INDEX IF NOT EXISTS resource_leases_token_idx ON resource_leases(token);
CREATE INDEX IF NOT EXISTS resource_leases_exp_idx ON resource_leases(expires_at);
"#;

/// Older builds declared `token` as the sole primary key, which cannot hold the
/// one-row-per-resource lease a single claim needs: the second resource of the
/// same claim collided with `UNIQUE constraint failed:
/// resource_leases.token` and every claim returned 500. Rebuild the table with
/// the composite key when that shape is detected.
///
/// Detection is by primary-key column count rather than a version row, because
/// a `swarm.db` may predate the version bookkeeping and still carry the bad
/// table.
fn rebuild_resource_leases_if_token_only_pk(conn: &Connection) -> rusqlite::Result<()> {
    let pk_cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(resource_leases)")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?;
        let mut cols = Vec::new();
        for row in rows {
            let (name, pk) = row?;
            if pk > 0 {
                cols.push(name);
            }
        }
        cols
    };
    // A fresh or already-correct table has (resource, token); the broken one has
    // just (token). Anything else is left alone rather than guessed at.
    if pk_cols.len() != 1 || pk_cols[0] != "token" {
        return Ok(());
    }
    conn.execute_batch(
        r#"
        BEGIN;
        ALTER TABLE resource_leases RENAME TO resource_leases_old_pk;
        CREATE TABLE resource_leases (
            resource   TEXT NOT NULL,
            token      TEXT NOT NULL,
            peer       TEXT NOT NULL,
            expires_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (resource, token)
        );
        INSERT INTO resource_leases(resource, token, peer, expires_at)
            SELECT resource, token, peer, expires_at FROM resource_leases_old_pk;
        DROP TABLE resource_leases_old_pk;
        CREATE INDEX IF NOT EXISTS resource_leases_res_idx ON resource_leases(resource);
        CREATE INDEX IF NOT EXISTS resource_leases_token_idx ON resource_leases(token);
        CREATE INDEX IF NOT EXISTS resource_leases_exp_idx ON resource_leases(expires_at);
        COMMIT;
        "#,
    )
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Apply the full schema and all migrations. Idempotent.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(BASE_SCHEMA)?;
    conn.execute_batch(RUN_LOG_SCHEMA)?;
    conn.execute_batch(RESOURCE_LEASE_SCHEMA)?;
    // Must run after the CREATE: an existing broken table is not recreated by
    // `CREATE TABLE IF NOT EXISTS`, so the rebuild has to look at what is there.
    rebuild_resource_leases_if_token_only_pk(conn)?;

    for (table, column, def) in ADDITIVE_COLUMNS {
        if !has_column(conn, table, column)? {
            conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, def),
                [],
            )?;
        }
    }
    for idx in ADDITIVE_INDEXES {
        conn.execute(idx, [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn migrate_is_idempotent() {
        let c = mem();
        migrate(&c).unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap();

        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // items, claims, submissions, events, messages, counters, run_log,
        // resource_leases
        assert_eq!(n, 8, "expected 8 tables, got {}", n);
    }

    /// The Go `swarm.db` must open here unchanged (README constraint 1). An
    /// additive table is ignored by Go rather than breaking it.
    #[test]
    fn adds_resource_leases_without_touching_go_tables() {
        let c = mem();
        // Exactly the Go base schema, no resource_leases.
        c.execute_batch(
            r#"
            CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT,
                title TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                kind TEXT NOT NULL DEFAULT 'task', priority INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'pending', created_by TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0, done_at INTEGER,
                not_before INTEGER, seq INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE claims (token TEXT PRIMARY KEY, item_id TEXT NOT NULL,
                peer TEXT NOT NULL DEFAULT '', expires_at INTEGER NOT NULL DEFAULT 0);
            INSERT INTO items(id,title) VALUES('i1','legacy row');
            INSERT INTO claims(token,item_id,peer,expires_at) VALUES('c1','i1','w1',99999999999999);
            "#,
        )
        .unwrap();

        migrate(&c).unwrap();

        // The Go rows survived.
        let title: String = c
            .query_row("SELECT title FROM items WHERE id='i1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "legacy row");
        let claims: i64 = c.query_row("SELECT count(*) FROM claims", [], |r| r.get(0)).unwrap();
        assert_eq!(claims, 1, "Go claims must not be disturbed");
        // And the new table exists and is empty — nothing to backfill, because a
        // claim taken by the Go version simply has no resource lease until it is
        // renewed.
        let leases: i64 = c
            .query_row("SELECT count(*) FROM resource_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(leases, 0);
    }

    /// Regression: `token` used to be the sole primary key, so the second
    /// resource of one claim hit `UNIQUE constraint failed:
    /// resource_leases.token` and every `POST /claim` answered 500. A claim
    /// that names two files must be able to hold two leases.
    #[test]
    fn one_claim_can_hold_several_resource_leases() {
        let c = mem();
        migrate(&c).unwrap();
        c.execute(
            "INSERT INTO resource_leases(resource, token, peer, expires_at) VALUES(?,?,?,?)",
            rusqlite::params!["src/a.rs", "c1", "w1", 100],
        )
        .unwrap();
        c.execute(
            "INSERT INTO resource_leases(resource, token, peer, expires_at) VALUES(?,?,?,?)",
            rusqlite::params!["src/b.rs", "c1", "w1", 100],
        )
        .expect("second lease for the same claim must be allowed");
        let n: i64 = c
            .query_row("SELECT count(*) FROM resource_leases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// An existing database written by the buggy build is repaired in place,
    /// keeping whatever leases it already had.
    #[test]
    fn migrates_token_only_pk_table_to_composite_key() {
        let c = mem();
        // The old, broken shape plus a surviving row.
        c.execute_batch(
            r#"
            CREATE TABLE resource_leases (
                resource   TEXT NOT NULL,
                token      TEXT PRIMARY KEY,
                peer       TEXT NOT NULL,
                expires_at INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO resource_leases(resource, token, peer, expires_at)
                VALUES('src/old.rs', 'c9', 'w7', 12345);
            "#,
        )
        .unwrap();

        migrate(&c).unwrap();

        let (res, tok, exp): (String, String, i64) = c
            .query_row(
                "SELECT resource, token, expires_at FROM resource_leases",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((res.as_str(), tok.as_str(), exp), ("src/old.rs", "c9", 12345));

        // And the repaired table now accepts a second lease for that token.
        c.execute(
            "INSERT INTO resource_leases(resource, token, peer, expires_at) VALUES(?,?,?,?)",
            rusqlite::params!["src/new.rs", "c9", "w7", 12345],
        )
        .expect("repaired table must allow a second lease per token");
    }

    #[test]
    fn additive_columns_present() {
        let c = mem();
        migrate(&c).unwrap();
        for (table, column, _) in ADDITIVE_COLUMNS {
            assert!(
                has_column(&c, table, column).unwrap(),
                "{}.{} missing",
                table,
                column
            );
        }
    }

    #[test]
    fn opens_a_go_shaped_database() {
        // Simulate a database created by the *old* Go schema (no run_log, no
        // additive columns) and confirm the migration upgrades it in place.
        let c = mem();
        c.execute_batch(
            r#"
            CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT,
                title TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                kind TEXT NOT NULL DEFAULT 'task', priority INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'pending', created_by TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0, done_at INTEGER,
                not_before INTEGER, seq INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE submissions (id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id TEXT NOT NULL, peer TEXT NOT NULL DEFAULT '',
                commit_sha TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                peer TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT 'OBSERVED',
                item_id TEXT, text TEXT NOT NULL DEFAULT '', tags TEXT,
                ts INTEGER NOT NULL DEFAULT 0);
            INSERT INTO items(id,title) VALUES('i1','legacy row');
            "#,
        )
        .unwrap();

        migrate(&c).unwrap();

        let title: String = c
            .query_row("SELECT title FROM items WHERE id='i1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "legacy row", "existing data must survive migration");

        let state: String = c
            .query_row(
                "SELECT state FROM submissions LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "queued".into());
        assert_eq!(state, "queued");
    }
}
