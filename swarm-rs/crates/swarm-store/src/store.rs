//! The store: single-writer SQLite over the Go-compatible schema.
//!
//! Concurrency model: one `Connection` behind a `Mutex`. The Go version does
//! the same thing with `db.SetMaxOpenConns(1)` (store.go L142) — serialising
//! writers is what avoids "database is locked" under the worker pool's
//! concurrent claims. A `std::sync::Mutex` is enough because every operation
//! here is short and synchronous; callers that need to await should use
//! `spawn_blocking` or the async wrapper added with the runner crate.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use swarm_core::{
    status, Claim, ClaimResult, ConflictInfo, CreateItemInput, Digest, Event, Item, MarksView, Result,
    ResourceLease, RunLogEntry, Stats, Submission, SwarmError, DEFAULT_LEASE_MILLIS,
};

use crate::resources::{artifact_key, item_resources};
use crate::schema;

/// Clock abstraction so tests can control lease expiry deterministically.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn wall_clock() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A SQLite-backed store. Cheap to clone (shares the connection).
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    now: Clock,
    lease_millis: AtomicI64,
}

impl Store {
    /// Open (and migrate) the database at `path`. An empty path means
    /// in-memory, matching Go's `Open("")`.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with_clock(path, None)
    }

    /// Open with an injectable clock (tests).
    pub fn open_with_clock(path: &str, now: Option<Clock>) -> Result<Self> {
        let path = if path.trim().is_empty() {
            ":memory:".to_string()
        } else {
            path.to_string()
        };
        let conn = Connection::open(&path)
            .map_err(|e| SwarmError::Store(format!("open {:?}: {}", path, e)))?;
        Self::from_connection(conn, now)
    }

    /// In-memory store for tests.
    pub fn open_memory() -> Result<Self> {
        Self::open("")
    }

    /// Wrap an existing connection, running the migration.
    pub fn from_connection(conn: Connection, now: Option<Clock>) -> Result<Self> {
        // WAL keeps a single writer from blocking readers; the Go version relies
        // on the same serialisation via SetMaxOpenConns(1).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "busy_timeout", 5000);
        schema::migrate(&conn).map_err(|e| SwarmError::Store(format!("migrate: {}", e)))?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                now: now.unwrap_or_else(|| Arc::new(wall_clock)),
                lease_millis: AtomicI64::new(DEFAULT_LEASE_MILLIS),
            }),
        })
    }


    /// Override the claim lease (milliseconds)./// Override the claim lease (milliseconds).
    pub fn with_lease_millis(self, ms: i64) -> Self {
        self.inner.lease_millis.store(ms, Ordering::SeqCst);
        self
    }

    fn now(&self) -> i64 {
        (self.inner.now)()
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.inner
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---------------------------------------------------------------- items

    /// Create an item, folding duplicates into the existing open item.
    ///
    /// Mirrors Go `CreateItem` (`internal/store/items.go` L358-395) including the
    /// two-level dedup that the doc comment always claimed and the code never
    /// had — `artifact_key` was re-exported as `dedup_key` and then called by
    /// nothing.
    ///
    /// The two levels, in Go's order:
    ///
    /// 1. **`f:` key** — the title names a file path, so fold against ANY open
    ///    item naming the same path. This is what catches "add X.js" versus
    ///    "create X.js with tests" landing as two items on one file.
    /// 2. **exact title** — the fallback for titles with no path in them.
    ///
    /// Seed callers (`peer` empty) bypass dedup entirely, so an operator can
    /// deliberately seed duplicates. Only worker-created items fold.
    ///
    /// Why it matters: without this, a worker that invents a slightly different
    /// title for work already on the board creates a second item for the same
    /// file, and two workers then contend on it under different ids.
    pub fn create_item(&self, input: CreateItemInput) -> Result<Item> {
        let title = input.title.trim().to_string();
        if title.is_empty() {
            return Err(SwarmError::Invalid("create item: title required".into()));
        }
        let kind = if input.kind.trim().is_empty() {
            "task".to_string()
        } else {
            input.kind.trim().to_string()
        };
        let peer = input.peer.trim().to_string();
        let now = self.now();
        let conn = self.conn();

        let total: i64 = conn
            .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .map_err(|e| SwarmError::Store(format!("count items: {}", e)))?;
        if total >= 10_000 {
            // Guard against a runaway splitter; the Go store has no explicit cap
            // but max_items bounds the runner. Keep a hard ceiling for safety.
            return Err(SwarmError::Conflict("create item: board is full".into()));
        }

        // ---- Dedup (worker-created items only) ----
        //
        // Go keys this on `in.Peer != ""` (`items.go` L365), but in this port the
        // seeder passes the literal peer `"seed"`, and `seed_progress` counts
        // items by `created_by = 'seed'`. Folding on `peer != ""` alone would
        // collapse deliberately duplicated seeds — which the seed tests do — so
        // the seed marker is excluded here too, matching how the server's
        // `--no-create` guard already treats it.
        let is_seed = peer.is_empty() || peer == "seed";
        if !is_seed {
            let key = artifact_key(&title);
            let open = "status NOT IN ('done','failed','split')";
            let dup: Option<String> = if key.is_empty() {
                // Level 2: no path in the title, so fall back to an exact match.
                conn.query_row(
                    &format!("SELECT id FROM items WHERE title = ? AND {} LIMIT 1", open),
                    params![title],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| SwarmError::Store(format!("create item dedup: {}", e)))?
            } else {
                // Level 1: fold on the file key across every open item.
                let mut stmt = conn
                    .prepare(&format!("SELECT id, title FROM items WHERE {}", open))
                    .map_err(|e| SwarmError::Store(format!("create item dedup scan: {}", e)))?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| SwarmError::Store(format!("create item dedup scan: {}", e)))?;
                let mut found = None;
                for row in rows.flatten() {
                    if artifact_key(&row.1) == key {
                        found = Some(row.0);
                        break;
                    }
                }
                found
            };
            if let Some(id) = dup {
                // Fold: return the existing item rather than inserting a twin.
                return get_item_inner(&conn, &id);
            }
        }

        let seq: i64 = next_counter(&conn, "i")?;
        let id = format!("i{}", seq);

        conn.execute(
            "INSERT INTO items(id, parent_id, title, detail, kind, priority, status, created_by, created_at, seq)
             VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![
                id,
                input.parent_id,
                title,
                input.detail,
                kind,
                input.priority,
                status::PENDING,
                peer,
                now,
                seq
            ],
        )
        .map_err(|e| SwarmError::Store(format!("insert item: {}", e)))?;

        get_item_inner(&conn, &id)
    }

    /// Fetch one item.
    pub fn get_item(&self, id: &str) -> Result<Item> {
        let conn = self.conn();
        get_item_inner(&conn, id)
    }

    /// List items, optionally filtered by status. Returns (rows, total count).
    pub fn list_items(&self, status_filter: &str, limit: i64, offset: i64) -> Result<(Vec<Item>, i64)> {
        let conn = self.conn();
        let total: i64 = conn
            .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .map_err(|e| SwarmError::Store(format!("count: {}", e)))?;

        let (sql, args): (String, Vec<Box<dyn rusqlite::ToSql>>) = if status_filter.trim().is_empty() {
            (
                format!(
                    "SELECT {} FROM items ORDER BY priority DESC, seq ASC",
                    ITEM_COLUMNS
                ),
                vec![],
            )
        } else {
            (
                format!(
                    "SELECT {} FROM items WHERE status = ? ORDER BY priority DESC, seq ASC",
                    ITEM_COLUMNS
                ),
                vec![Box::new(status_filter.to_string())],
            )
        };
        let sql = if limit > 0 {
            format!("{} LIMIT ? OFFSET ?", sql)
        } else {
            sql
        };
        let mut dyn_args: Vec<Box<dyn rusqlite::ToSql>> = args;
        if limit > 0 {
            dyn_args.push(Box::new(limit));
            dyn_args.push(Box::new(offset));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = dyn_args.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| SwarmError::Store(format!("prepare list: {}", e)))?;
        let rows = stmt
            .query_map(refs.as_slice(), scan_item)
            .map_err(|e| SwarmError::Store(format!("list items: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("scan item: {}", e)))?);
        }
        Ok((out, total))
    }

    /// Count items that a worker could claim right now.
    pub fn claimable_count(&self) -> Result<i64> {
        let now = self.now();
        let conn = self.conn();
        conn.query_row(
            "SELECT count(*) FROM items i
             WHERE i.status = 'pending'
               AND (i.not_before IS NULL OR i.not_before <= ?)
               AND NOT EXISTS (SELECT 1 FROM claims c WHERE c.item_id = i.id AND c.expires_at > ?)",
            params![now, now],
            |r| r.get(0),
        )
        .map_err(|e| SwarmError::Store(format!("claimable count: {}", e)))
    }

    /// Count non-terminal items.
    pub fn open_count(&self) -> Result<i64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT count(*) FROM items WHERE status IN ('pending','in_progress','submitted')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| SwarmError::Store(format!("open count: {}", e)))
    }

    /// Board counters.
    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn();
        let mut s = Stats::default();
        let mut stmt = conn
            .prepare("SELECT status, count(*) FROM items GROUP BY status")
            .map_err(|e| SwarmError::Store(format!("stats: {}", e)))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| SwarmError::Store(format!("stats query: {}", e)))?;
        for row in rows {
            let (st, n) = row.map_err(|e| SwarmError::Store(format!("stats row: {}", e)))?;
            s.total += n;
            match st.as_str() {
                status::PENDING => s.pending = n,
                status::IN_PROGRESS => s.in_progress = n,
                status::SUBMITTED => s.submitted = n,
                status::DONE => s.done = n,
                status::FAILED => s.failed = n,
                _ => {}
            }
        }
        Ok(s)
    }

    /// Set an item's status directly.
    pub fn set_status(&self, id: &str, new_status: &str) -> Result<()> {
        let conn = self.conn();
        let n = conn
            .execute(
                "UPDATE items SET status = ? WHERE id = ?",
                params![new_status, id],
            )
            .map_err(|e| SwarmError::Store(format!("set status: {}", e)))?;
        if n == 0 {
            return Err(SwarmError::NotFound(format!("item {:?}", id)));
        }
        Ok(())
    }

    /// Depth of an item in the split tree (0 = top level).
    pub fn item_depth(&self, id: &str) -> Result<u32> {
        let conn = self.conn();
        let mut depth = 0u32;
        let mut cur = id.to_string();
        loop {
            let parent: Option<Option<String>> = conn
                .query_row("SELECT parent_id FROM items WHERE id = ?", params![cur], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|e| SwarmError::Store(format!("depth: {}", e)))?;
            match parent.flatten() {
                Some(p) if depth < 32 => {
                    depth += 1;
                    cur = p;
                }
                _ => break,
            }
        }
        Ok(depth)
    }

    /// Number of items on the board.
    pub fn item_count(&self) -> Result<i64> {
        let conn = self.conn();
        conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .map_err(|e| SwarmError::Store(format!("item count: {}", e)))
    }

    /// Seed progress: (open seed items, total seed items).
    ///
    /// "Seed" means `created_by = 'seed'` or an item descended from one. This is
    /// what `--exit-when-done` keys on: the operator's own work, not the whole
    /// board.
    pub fn seed_progress(&self) -> Result<(i64, i64)> {
        let conn = self.conn();
        let total: i64 = conn
            .query_row(
                "WITH RECURSIVE seed(id) AS (
                    SELECT id FROM items WHERE created_by = 'seed'
                    UNION
                    SELECT i.id FROM items i JOIN seed s ON i.parent_id = s.id
                 ) SELECT count(*) FROM seed",
                [],
                |r| r.get(0),
            )
            .map_err(|e| SwarmError::Store(format!("seed total: {}", e)))?;
        let open: i64 = conn
            .query_row(
                "WITH RECURSIVE seed(id) AS (
                    SELECT id FROM items WHERE created_by = 'seed'
                    UNION
                    SELECT i.id FROM items i JOIN seed s ON i.parent_id = s.id
                 )
                 SELECT count(*) FROM items
                 WHERE id IN (SELECT id FROM seed)
                   AND status IN ('pending','in_progress','submitted')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| SwarmError::Store(format!("seed open: {}", e)))?;
        Ok((open, total))
    }

    /// How many items were created in the last `window_ms` (splits inflate this).
    pub fn recent_split_count(&self, window_ms: i64) -> Result<i64> {
        let since = self.now() - window_ms;
        let conn = self.conn();
        conn.query_row(
            "SELECT count(*) FROM items WHERE created_at >= ?",
            params![since],
            |r| r.get(0),
        )
        .map_err(|e| SwarmError::Store(format!("recent split: {}", e)))
    }

    // --------------------------------------------------------------- claims

    /// Take the lease on an item and return the task package.
    ///
    /// Accepts `pending` **and** `in_progress`: an expired lease is reaped
    /// inside this transaction and the item returned to `pending`. Rejecting
    /// `in_progress` at the door would leave the item stuck forever once its
    /// lease elapsed (Go claims.go L36-41).
    pub fn claim(&self, item_id: &str, peer: &str) -> Result<ClaimResult> {
        let item_id = item_id.trim();
        let peer = peer.trim();
        if item_id.is_empty() || peer.is_empty() {
            return Err(SwarmError::Invalid(
                "claim: itemId and peer required".into(),
            ));
        }
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("claim tx: {}", e)))?;

        let mut item = tx
            .query_row(
                &format!("SELECT {} FROM items WHERE id = ?", ITEM_COLUMNS),
                params![item_id],
                scan_item,
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("claim item: {}", e)))?
            .ok_or_else(|| SwarmError::NotFound(format!("claim item {:?}", item_id)))?;

        if item.status != status::PENDING && item.status != status::IN_PROGRESS {
            return Err(SwarmError::Conflict(format!(
                "claim item {:?} is {}",
                item_id, item.status
            )));
        }

        // Reap this item's claims: live ones block, expired ones are removed.
        let mut priors: Vec<(String, i64)> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT token, expires_at FROM claims WHERE item_id = ?")
                .map_err(|e| SwarmError::Store(format!("claim priors: {}", e)))?;
            let rows = stmt
                .query_map(params![item_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| SwarmError::Store(format!("claim priors query: {}", e)))?;
            for row in rows {
                priors.push(row.map_err(|e| SwarmError::Store(format!("claim prior row: {}", e)))?);
            }
        }
        for (token, expires) in priors {
            if expires > now {
                return Err(SwarmError::Conflict(format!(
                    "claim item {:?} already claimed",
                    item_id
                )));
            }
            tx.execute("DELETE FROM claims WHERE token = ?", params![token])
                .map_err(|e| SwarmError::Store(format!("claim reap: {}", e)))?;
            if item.status == status::IN_PROGRESS {
                tx.execute(
                    "UPDATE items SET status = ? WHERE id = ?",
                    params![status::PENDING, item_id],
                )
                .map_err(|e| SwarmError::Store(format!("claim reset: {}", e)))?;
                item.status = status::PENDING.to_string();
            }
        }

        let seq = next_counter(&tx, "c")?;
        let token = format!("c{}", seq);
        let expires = now + self.inner.lease_millis.load(Ordering::SeqCst);
        tx.execute(
            "INSERT INTO claims(token, item_id, peer, expires_at) VALUES(?,?,?,?)",
            params![token, item_id, peer, expires],
        )
        .map_err(|e| SwarmError::Store(format!("claim insert: {}", e)))?;
        tx.execute(
            "UPDATE items SET status = ? WHERE id = ?",
            params![status::IN_PROGRESS, item_id],
        )
        .map_err(|e| SwarmError::Store(format!("claim mark in_progress: {}", e)))?;

        // Resource-level leases. The item-level claim above is necessary but not
        // sufficient: two items naming the same file would both pass it, and the
        // collision would surface in the merge instead of at the door. Taking the
        // resource leases in the SAME transaction is what makes the check
        // atomic — a separate read-then-write would let two peers interleave.
        //
        // The resource keys come from `item_resources`, the same function the
        // conflict scan and the presplit axis use, so the three can never
        // disagree (README constraint 3).
        let resources = item_resources(&item.title, &item.detail);
        let blocked = resource_conflicts_tx(&tx, &resources, peer, now)?;
        if !blocked.is_empty() {
            // Roll back by returning early: the transaction is dropped without a
            // commit, so the claim insert above never lands.
            return Err(SwarmError::Conflict(format!(
                "claim item {:?} needs resources held by another peer: {}",
                item_id,
                blocked.join(", ")
            )));
        }
        for res in &resources {
            tx.execute(
                "INSERT INTO resource_leases(resource, token, peer, expires_at) VALUES(?,?,?,?)",
                params![res, token, peer, expires],
            )
            .map_err(|e| SwarmError::Store(format!("resource lease insert: {}", e)))?;
        }

        let history = history_tx(&tx, item_id)?;
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("claim commit: {}", e)))?;

        item.status = status::IN_PROGRESS.to_string();
        // Conflict scan runs AFTER the transaction so a slow scan never holds
        // the claim lock (Go claims.go L104-107).
        let conflicts = conflicts_for_item_inner(&conn, &item, peer, self.now());

        Ok(ClaimResult {
            token,
            item,
            history,
            conflicts,
        })
    }

    /// Report live intents other peers hold on the resources this item names.
    #[allow(dead_code)]
    fn conflicts_for_item(&self, item: &Item, exclude_peer: &str) -> Vec<ConflictInfo> {
        let conn = self.conn();
        conflicts_for_item_inner(&conn, item, exclude_peer, self.now())
    }

    /// Drop a claim and put the item back on the board as pending.
    pub fn release(&self, token: &str) -> Result<()> {
        let token = token.trim();
        if token.is_empty() {
            return Err(SwarmError::Invalid("release: missing token".into()));
        }
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("release tx: {}", e)))?;

        let item_id: Option<String> = tx
            .query_row(
                "SELECT item_id FROM claims WHERE token = ?",
                params![token],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("release lookup: {}", e)))?;
        let item_id = item_id.ok_or_else(|| SwarmError::NotFound(format!("claim {:?}", token)))?;

        tx.execute("DELETE FROM claims WHERE token = ?", params![token])
            .map_err(|e| SwarmError::Store(format!("release delete: {}", e)))?;
        // The resource leases are keyed on the claim token, so one delete
        // cascades: a released claim stops pinning every file it named.
        tx.execute(
            "DELETE FROM resource_leases WHERE token = ?",
            params![token],
        )
        .map_err(|e| SwarmError::Store(format!("release resource delete: {}", e)))?;
        tx.execute(
            "UPDATE items SET status = ? WHERE id = ? AND status = ?",
            params![status::PENDING, item_id, status::IN_PROGRESS],
        )
        .map_err(|e| SwarmError::Store(format!("release reset: {}", e)))?;
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("release commit: {}", e)))?;
        Ok(())
    }

    /// Extend a claim's lease.
    ///
    /// The resource leases must be extended with it, or a long-held item stops
    /// pinning its files halfway through and a second peer can start editing
    /// them while the first is still working.
    pub fn renew(&self, token: &str) -> Result<()> {
        let expires = self.now() + self.inner.lease_millis.load(Ordering::SeqCst);
        let conn = self.conn();
        let n = conn
            .execute(
                "UPDATE claims SET expires_at = ? WHERE token = ?",
                params![expires, token],
            )
            .map_err(|e| SwarmError::Store(format!("renew: {}", e)))?;
        if n == 0 {
            return Err(SwarmError::NotFound(format!("claim {:?}", token)));
        }
        conn.execute(
            "UPDATE resource_leases SET expires_at = ? WHERE token = ?",
            params![expires, token],
        )
        .map_err(|e| SwarmError::Store(format!("renew resources: {}", e)))?;
        Ok(())
    }

    /// Live claims held by one peer.
    pub fn active_claims_by_peer(&self, peer: &str) -> Result<Vec<Claim>> {
        let now = self.now();
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT token, item_id, peer, expires_at FROM claims
                 WHERE peer = ? AND expires_at > ?",
            )
            .map_err(|e| SwarmError::Store(format!("claims by peer: {}", e)))?;
        let rows = stmt
            .query_map(params![peer, now], |r| {
                Ok(Claim {
                    token: r.get(0)?,
                    item_id: r.get(1)?,
                    peer: r.get(2)?,
                    expires_at: r.get(3)?,
                })
            })
            .map_err(|e| SwarmError::Store(format!("claims by peer query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("claim row: {}", e)))?);
        }
        Ok(out)
    }

    /// Sweep expired leases, returning items to pending.
    pub fn reap_expired(&self) -> Result<usize> {
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("reap tx: {}", e)))?;

        let mut expired: Vec<(String, String, String)> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT token, item_id, peer FROM claims WHERE expires_at <= ?")
                .map_err(|e| SwarmError::Store(format!("reap query: {}", e)))?;
            let rows = stmt
                .query_map(params![now], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| SwarmError::Store(format!("reap rows: {}", e)))?;
            for row in rows {
                expired.push(row.map_err(|e| SwarmError::Store(format!("reap row: {}", e)))?);
            }
        }
        for (token, item_id, _peer) in &expired {
            tx.execute("DELETE FROM claims WHERE token = ?", params![token])
                .map_err(|e| SwarmError::Store(format!("reap delete: {}", e)))?;
            tx.execute(
                "DELETE FROM resource_leases WHERE token = ?",
                params![token],
            )
            .map_err(|e| SwarmError::Store(format!("reap resource delete: {}", e)))?;
            tx.execute(
                "UPDATE items SET status = ? WHERE id = ? AND status = ?",
                params![status::PENDING, item_id, status::IN_PROGRESS],
            )
            .map_err(|e| SwarmError::Store(format!("reap reset: {}", e)))?;
        }
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("reap commit: {}", e)))?;

        // A peer whose lease elapsed is gone: its intent marks must not keep
        // pinning resources (Go claims.go L341-348).
        //
        // The connection guard is dropped first: `release_marks` /
        // `reap_expired_marks` take the same (non-reentrant) mutex, so calling
        // them while holding it self-deadlocks.
        let peers: Vec<String> = expired
            .iter()
            .map(|(_, _, p)| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        drop(conn);
        for p in peers {
            let _ = self.release_marks(&p);
        }
        let _ = self.reap_expired_marks();
        Ok(expired.len())
    }

    // ---------------------------------------------------------- submissions

    /// Record a submission and mark the item submitted.
    pub fn submit(&self, token: &str, peer: &str, commit_sha: &str) -> Result<Item> {
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("submit tx: {}", e)))?;

        let item_id: Option<String> = tx
            .query_row(
                "SELECT item_id FROM claims WHERE token = ?",
                params![token],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("submit claim: {}", e)))?;
        let item_id = item_id.ok_or_else(|| SwarmError::NotFound(format!("claim {:?}", token)))?;

        tx.execute("DELETE FROM claims WHERE token = ?", params![token])
            .map_err(|e| SwarmError::Store(format!("submit drop claim: {}", e)))?;
        // The claim is consumed, so its resource leases go with it — otherwise a
        // submitted item would keep pinning its files forever and nothing that
        // names them could ever be claimed again.
        tx.execute(
            "DELETE FROM resource_leases WHERE token = ?",
            params![token],
        )
        .map_err(|e| SwarmError::Store(format!("submit resource delete: {}", e)))?;
        tx.execute(
            "INSERT INTO submissions(item_id, peer, commit_sha, created_at, state)
             VALUES(?,?,?,?,'queued')",
            params![item_id, peer, commit_sha, now],
        )
        .map_err(|e| SwarmError::Store(format!("submit insert: {}", e)))?;
        tx.execute(
            "UPDATE items SET status = ? WHERE id = ?",
            params![status::SUBMITTED, item_id],
        )
        .map_err(|e| SwarmError::Store(format!("submit mark: {}", e)))?;
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("submit commit: {}", e)))?;

        get_item_inner(&conn, &item_id)
    }

    /// Submissions still waiting for the integrator.
    pub fn queued_submissions(&self) -> Result<Vec<Submission>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, item_id, peer, commit_sha, created_at, state, reason
                 FROM submissions WHERE state = 'queued' ORDER BY id ASC",
            )
            .map_err(|e| SwarmError::Store(format!("queued subs: {}", e)))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Submission {
                    id: r.get(0)?,
                    item_id: r.get(1)?,
                    peer: r.get(2)?,
                    commit_sha: r.get(3)?,
                    created_at: r.get(4)?,
                    state: r.get(5)?,
                    reason: r.get(6)?,
                })
            })
            .map_err(|e| SwarmError::Store(format!("queued subs query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("sub row: {}", e)))?);
        }
        Ok(out)
    }

    /// Resolve a submission. On success the item is `done`; on failure it goes
    /// back to `pending` with a backoff, or `failed` past `max_fails`.
    pub fn resolve_submission(
        &self,
        id: i64,
        ok: bool,
        reason: &str,
        max_fails: u32,
    ) -> Result<()> {
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("resolve tx: {}", e)))?;

        let item_id: Option<String> = tx
            .query_row(
                "SELECT item_id FROM submissions WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("resolve lookup: {}", e)))?;
        let item_id =
            item_id.ok_or_else(|| SwarmError::NotFound(format!("submission {}", id)))?;

        if ok {
            tx.execute(
                "UPDATE submissions SET state='ok', reason='' WHERE id = ?",
                params![id],
            )
            .map_err(|e| SwarmError::Store(format!("resolve ok: {}", e)))?;
            tx.execute(
                "UPDATE items SET status = ?, done_at = ? WHERE id = ?",
                params![status::DONE, now, item_id],
            )
            .map_err(|e| SwarmError::Store(format!("resolve done: {}", e)))?;
        } else {
            tx.execute(
                "UPDATE submissions SET state='rejected', reason = ? WHERE id = ?",
                params![reason, id],
            )
            .map_err(|e| SwarmError::Store(format!("resolve reject: {}", e)))?;

            let fails: i64 = tx
                .query_row(
                    "SELECT count(*) FROM submissions WHERE item_id = ? AND state = 'rejected'",
                    params![item_id],
                    |r| r.get(0),
                )
                .map_err(|e| SwarmError::Store(format!("resolve fails: {}", e)))?;

            if fails as u32 >= max_fails {
                tx.execute(
                    "UPDATE items SET status = ? WHERE id = ?",
                    params![status::FAILED, item_id],
                )
                .map_err(|e| SwarmError::Store(format!("resolve failed: {}", e)))?;
            } else {
                // Exponential backoff, capped at 60s, so a repeatedly failing
                // item does not spin. `not_before` is cleared on terminal states
                // by construction (status becomes failed above).
                let backoff_ms = (1000i64 << fails.min(6)).min(60_000);
                tx.execute(
                    "UPDATE items SET status = ?, not_before = ? WHERE id = ?",
                    params![status::PENDING, now + backoff_ms, item_id],
                )
                .map_err(|e| SwarmError::Store(format!("resolve requeue: {}", e)))?;
            }
        }
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("resolve commit: {}", e)))?;
        Ok(())
    }

    // ------------------------------------------------------------- run log

    /// Append one entry to the run log.
    ///
    /// The run log is the append-only fact layer: a summary can always be
    /// recomputed from it, but it can never be reconstructed from a summary.
    /// `accepted` is recorded per entry so the acceptance rate is a derived
    /// number rather than a value the runner happens to still have in memory
    /// when the process exits.
    pub fn append_run_log(&self, entry: RunLogEntry) -> Result<i64> {
        let now = if entry.ts == 0 { self.now() } else { entry.ts };
        let conn = self.conn();
        conn.execute(
            "INSERT INTO run_log(kind, actor, item_id, title, detail, tokens, files, seconds, ts, accepted, old_value, new_value, field)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                entry.kind,
                entry.actor,
                entry.item_id,
                entry.title,
                entry.detail,
                entry.tokens,
                entry.files,
                entry.seconds,
                now,
                if entry.accepted { 1 } else { 0 },
                entry.old_value,
                entry.new_value,
                entry.field,
            ],
        )
        .map_err(|e| SwarmError::Store(format!("append run log: {}", e)))?;
        Ok(conn.last_insert_rowid())
    }

    /// Read the run log, newest last. `limit` of 0 means no limit.
    pub fn run_log(&self, limit: i64) -> Result<Vec<RunLogEntry>> {
        let conn = self.conn();
        // `COALESCE` on the provenance columns so a database written before they
        // existed (or by the Go version, which has no such columns) reads back as
        // empty strings rather than failing the whole query.
        let cols = "id, kind, actor, COALESCE(item_id,''), COALESCE(title,''), COALESCE(detail,''),
                    tokens, files, seconds, ts, accepted,
                    COALESCE(old_value,''), COALESCE(new_value,''), COALESCE(field,'')";
        let sql = if limit > 0 {
            format!(
                "SELECT {} FROM run_log ORDER BY id ASC LIMIT {}",
                cols, limit
            )
        } else {
            format!("SELECT {} FROM run_log ORDER BY id ASC", cols)
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| SwarmError::Store(format!("run log query: {}", e)))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RunLogEntry {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    actor: r.get(2)?,
                    item_id: r.get(3)?,
                    title: r.get(4)?,
                    detail: r.get(5)?,
                    tokens: r.get(6)?,
                    files: r.get(7)?,
                    seconds: r.get(8)?,
                    ts: r.get(9)?,
                    accepted: r.get::<_, i64>(10)? != 0,
                    old_value: r.get(11)?,
                    new_value: r.get(12)?,
                    field: r.get(13)?,
                })
            })
            .map_err(|e| SwarmError::Store(format!("run log rows: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("run log row: {}", e)))?);
        }
        Ok(out)
    }

    /// Acceptance counters derived from the run log: (accepted, attempted).
    ///
    /// Derived, not stored, so it can never disagree with the log itself. A run
    /// that wrote nothing returns (0, 0) rather than a fabricated rate.
    pub fn acceptance_counts(&self) -> Result<(i64, i64)> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COALESCE(SUM(accepted), 0), COUNT(*) FROM run_log",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| SwarmError::Store(format!("acceptance counts: {}", e)))
    }

    /// The content identity (git tree sha) of the last accepted integration for
    /// an item, if any.
    ///
    /// This is what makes duplicate detection possible. `submissions` records
    /// commit shas, but a commit sha changes on every merge (different parents,
    /// different timestamp) while the *tree* is the content — so comparing trees
    /// answers "is this the same work?" where comparing commits cannot.
    ///
    /// The tree is carried in `run_log.detail` for accepted entries (the runner
    /// writes it there). Returns `None` when the item has never been accepted,
    /// which the caller must treat as "not a duplicate" rather than as an error.
    pub fn last_accepted_tree(&self, item_id: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let tree: Option<String> = conn
            .query_row(
                "SELECT detail FROM run_log
                 WHERE item_id = ? AND kind = 'integration' AND accepted = 1
                   AND detail <> ''
                 ORDER BY id DESC LIMIT 1",
                params![item_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("last accepted tree: {}", e)))?;
        Ok(tree.filter(|t| !t.is_empty()))
    }

    // ------------------------------------------------------------- context

    /// Append a context entry.
    pub fn add_context(&self, ev: Event) -> Result<i64> {
        let now = if ev.ts == 0 { self.now() } else { ev.ts };
        let tags = ev
            .tags
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_default());
        let conn = self.conn();
        conn.execute(
            "INSERT INTO events(peer, kind, item_id, text, tags, ts) VALUES(?,?,?,?,?,?)",
            params![ev.peer, ev.kind, ev.item_id, ev.text, tags, now],
        )
        .map_err(|e| SwarmError::Store(format!("add context: {}", e)))?;
        Ok(conn.last_insert_rowid())
    }

    /// Per-worker digest for the start of a turn.
    pub fn digest(&self, peer: &str, since_ts: i64) -> Result<Digest> {
        let now = self.now();
        let claims = self.active_claims_by_peer(peer)?;
        let stats = self.stats()?;

        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts
                 FROM events WHERE ts > ? AND peer <> ?
                 ORDER BY ts ASC LIMIT 50",
            )
            .map_err(|e| SwarmError::Store(format!("digest ctx: {}", e)))?;
        let rows = stmt
            .query_map(params![since_ts, peer], |r| {
                let tags: Option<String> = r.get(5)?;
                Ok(Event {
                    id: r.get(0)?,
                    peer: r.get(1)?,
                    kind: r.get(2)?,
                    item_id: Some(r.get(3)?).filter(|s: &String| !s.is_empty()),
                    text: r.get(4)?,
                    tags: tags.and_then(|t| serde_json::from_str(&t).ok()),
                    ts: r.get(6)?,
                })
            })
            .map_err(|e| SwarmError::Store(format!("digest ctx query: {}", e)))?;
        let mut new_context = Vec::new();
        for r in rows {
            new_context.push(r.map_err(|e| SwarmError::Store(format!("ctx row: {}", e)))?);
        }

        let mut stmt = conn
            .prepare(
                "SELECT id, parent_id, title, detail, kind, priority, status,
                        created_by, created_at, done_at, not_before, seq
                 FROM items WHERE status = 'pending'
                 ORDER BY priority DESC, seq ASC LIMIT 20",
            )
            .map_err(|e| SwarmError::Store(format!("digest pending: {}", e)))?;
        let rows = stmt
            .query_map([], scan_item)
            .map_err(|e| SwarmError::Store(format!("digest pending query: {}", e)))?;
        let mut pending_preview = Vec::new();
        for r in rows {
            pending_preview.push(r.map_err(|e| SwarmError::Store(format!("pending row: {}", e)))?);
        }

        Ok(Digest {
            now,
            claims,
            new_context,
            stats,
            pending_preview,
        })
    }

    // --------------------------------------------------------------- marks

    /// Record an intent mark so other workers see who is editing a resource.
    pub fn mark_intent(
        &self,
        peer: &str,
        item_id: &str,
        resources: &[String],
        ttl_ms: i64,
    ) -> Result<()> {
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| SwarmError::Store(format!("mark tx: {}", e)))?;
        for res in resources {
            tx.execute(
                "INSERT INTO events(peer, kind, item_id, text, ts, mark_type, resource, ttl_ms)
                 VALUES(?, 'INTENT', ?, '', ?, 'intent', ?, ?)",
                params![peer, item_id, now, res, ttl_ms],
            )
            .map_err(|e| SwarmError::Store(format!("mark insert: {}", e)))?;
        }
        tx.commit()
            .map_err(|e| SwarmError::Store(format!("mark commit: {}", e)))?;
        Ok(())
    }

    /// Record an intent mark for one resource, with an optional TTL.
    ///
    /// This is the write path behind `POST /context` for the five protocol
    /// marks; the plain `mark_intent` (slice form) stays for the runner's
    /// per-turn mark refresh.
    pub fn mark_intent_ext(
        &self,
        peer: &str,
        item_id: &str,
        resource: &str,
        ttl_ms: Option<i64>,
    ) -> Result<()> {
        let now = self.now();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO events(peer, kind, item_id, text, ts, mark_type, resource, ttl_ms)
             VALUES(?, 'INTENT', ?, '', ?, 'intent', ?, ?)",
            params![peer, item_id, now, resource, ttl_ms],
        )
        .map_err(|e| SwarmError::Store(format!("mark: {}", e)))?;
        Ok(())
    }
    /// Drop every intent mark a peer holds.
    pub fn release_marks(&self, peer: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM events WHERE mark_type = 'intent' AND peer = ?",
            params![peer],
        )
        .map_err(|e| SwarmError::Store(format!("release marks: {}", e)))?;
        Ok(())
    }

    /// Drop intent marks whose TTL elapsed.
    pub fn reap_expired_marks(&self) -> Result<usize> {
        let now = self.now();
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM events
                 WHERE mark_type = 'intent' AND ttl_ms IS NOT NULL AND ts + ttl_ms <= ?",
                params![now],
            )
            .map_err(|e| SwarmError::Store(format!("reap marks: {}", e)))?;
        Ok(n)
    }

    // ------------------------------------------------------------- history

    /// Context entries attached to one item, oldest first (Go `ItemHistory`).
    pub fn item_history(&self, item_id: &str) -> Result<Vec<Event>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts
                 FROM events WHERE item_id = ? ORDER BY ts ASC, id ASC LIMIT 50",
            )
            .map_err(|e| SwarmError::Store(format!("history: {}", e)))?;
        let rows = stmt
            .query_map(params![item_id], scan_event)
            .map_err(|e| SwarmError::Store(format!("history query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("history row: {}", e)))?);
        }
        Ok(out)
    }

    // ----------------------------------------------------------- needs split

    /// Whether an item should be decomposed before a worker touches it.
    ///
    /// Copied from Go `items.go` L284-303: a **seed** item with **no parent**,
    /// not already terminal, and **no children yet**. The resource count is NOT
    /// part of the test — the runner's own `splitThreshold` decides whether a
    /// split actually happens, and `presplitOversizedItems` runs before any
    /// worker starts. This flag only answers "should the first worker on this
    /// item consider splitting".
    ///
    /// Checked AFTER a claim so two workers racing on the same item cannot both
    /// be told to split it.
    pub fn needs_split(&self, item_id: &str) -> Result<bool> {
        let item = self.get_item(item_id)?;
        if item.created_by.trim() != "seed" {
            return Ok(false);
        }
        if item.parent_id.is_some() {
            return Ok(false); // already a child
        }
        if status::is_terminal(&item.status) {
            return Ok(false);
        }
        Ok(self.child_count(item_id)? == 0)
    }

    /// How many children an item has.
    pub fn child_count(&self, parent_id: &str) -> Result<i64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT count(*) FROM items WHERE parent_id = ?",
            params![parent_id],
            |r| r.get(0),
        )
        .map_err(|e| SwarmError::Store(format!("child count: {}", e)))
    }

    // --------------------------------------------------------------- search

    /// Substring search over context text (Go `SearchContext`).
    pub fn search_context(&self, query: &str, limit: i64) -> Result<Vec<Event>> {
        let conn = self.conn();
        let mut rows = Vec::new();
        if query.trim().is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts
                     FROM events ORDER BY ts DESC, id DESC LIMIT ?",
                )
                .map_err(|e| SwarmError::Store(format!("search: {}", e)))?;
            let it = stmt
                .query_map(params![limit], scan_event)
                .map_err(|e| SwarmError::Store(format!("search query: {}", e)))?;
            for r in it {
                rows.push(r.map_err(|e| SwarmError::Store(format!("search row: {}", e)))?);
            }
        } else {
            let pattern = format!("%{}%", query.trim().to_lowercase());
            let mut stmt = conn
                .prepare(
                    "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts
                     FROM events WHERE LOWER(text) LIKE ?
                     ORDER BY ts DESC, id DESC LIMIT ?",
                )
                .map_err(|e| SwarmError::Store(format!("search: {}", e)))?;
            let it = stmt
                .query_map(params![pattern, limit], scan_event)
                .map_err(|e| SwarmError::Store(format!("search query: {}", e)))?;
            for r in it {
                rows.push(r.map_err(|e| SwarmError::Store(format!("search row: {}", e)))?);
            }
        }
        Ok(rows)
    }

    // ----------------------------------------------------------------- marks

    /// Live intent marks on a resource (Go `ActiveMarks`).
    ///
    /// **Reads BOTH coordination mechanisms.** The Go version answers only from
    /// intent marks in `events`. Since the resource-lease upgrade there are two
    /// independent answers to "who holds this resource?", and they disagree:
    ///
    /// * an intent mark is a *hint* a worker writes about what it plans to edit,
    ///   with its own TTL, and it can go stale;
    /// * a resource lease is the *authority* — written in the same transaction
    ///   that grants the claim, so if it exists, the resource really is held.
    ///
    /// Reporting only the hint is worse than reporting nothing: a worker that
    /// polls `/marks`, sees zero, and claims anyway gets a 409 from the lease it
    /// was never told about. The 304-worker stress run showed exactly this —
    /// workers reported `count: 0` while the lock was demonstrably held, and
    /// fell back to blind retry loops.
    ///
    /// So the response now carries the leases as a separate field, and `count`
    /// stays the mark count (its documented Go-compatible meaning) with the
    /// authoritative total under `held`.
    pub fn active_marks(&self, resource: &str, limit: i64) -> Result<MarksView> {
        let now = self.now();
        let conn = self.conn();
        let mut marks = Vec::new();
        let sql = if resource.trim().is_empty() {
            "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts FROM events
             WHERE mark_type = 'intent'
               AND (ttl_ms IS NULL OR ts + ttl_ms > ?)
             ORDER BY ts DESC LIMIT ?".to_string()
        } else {
            "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts FROM events
             WHERE mark_type = 'intent' AND resource = ?
               AND (ttl_ms IS NULL OR ts + ttl_ms > ?)
             ORDER BY ts DESC LIMIT ?".to_string()
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| SwarmError::Store(format!("marks: {}", e)))?;
        let it = if resource.trim().is_empty() {
            stmt.query_map(params![now, limit], scan_event)
        } else {
            stmt.query_map(params![resource, now, limit], scan_event)
        }
        .map_err(|e| SwarmError::Store(format!("marks query: {}", e)))?;
        for r in it {
            marks.push(r.map_err(|e| SwarmError::Store(format!("mark row: {}", e)))?);
        }

        // The authoritative half: who actually holds a lease on this resource.
        //
        // This reads through the SAME guard (`conn` derefs to the connection),
        // NOT through `self.resource_leases_for()`. The store's `Mutex` is not
        // reentrant, so calling a method that takes the lock again while this
        // guard is live self-deadlocks — the exact trap `reap_expired` records
        // having hit (README, `SWARM-RUN-ISSUES.md`). The first version of this
        // join did precisely that and hung the `/marks` test.
        let leases = resource_leases_tx(&conn, resource, limit, now)?;
        Ok(MarksView { marks, leases })
    }

    /// Live resource leases on one resource (all resources when empty).
    ///
    /// Takes the connection guard from the caller rather than acquiring it, so a
    /// caller that already holds the lock (like `active_marks`, which reads both
    /// halves of the coordination state in one call) can use it without
    /// self-deadlocking.
    #[allow(dead_code)]
    fn resource_leases_for(&self, resource: &str, limit: i64) -> Result<Vec<ResourceLease>> {
        let conn = self.conn();
        resource_leases_tx(&conn, resource, limit, self.now())
    }

    // -------------------------------------------------------------- messages

    /// Send a direct message between peers (Go `SendMessage`).
    pub fn send_message(&self, from: &str, to: &str, text: &str) -> Result<i64> {
        let now = self.now();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO messages(\"from\", \"to\", text, ts, read) VALUES(?,?,?,?,0)",
            params![from, to, text, now],
        )
        .map_err(|e| SwarmError::Store(format!("send message: {}", e)))?;
        Ok(conn.last_insert_rowid())
    }

    /// Messages for a peer since a timestamp (Go `Messages`).
    pub fn messages(&self, peer: &str, since: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, \"from\", \"to\", text, ts, \"read\" FROM messages
                 WHERE \"to\" = ? AND ts > ? ORDER BY ts ASC, id ASC LIMIT 200",
            )
            .map_err(|e| SwarmError::Store(format!("messages: {}", e)))?;
        let rows = stmt
            .query_map(params![peer, since], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "from": r.get::<_, String>(1)?,
                    "to": r.get::<_, String>(2)?,
                    "text": r.get::<_, String>(3)?,
                    "ts": r.get::<_, i64>(4)?,
                    "read": r.get::<_, i64>(5)? != 0,
                }))
            })
            .map_err(|e| SwarmError::Store(format!("messages query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("message row: {}", e)))?);
        }
        Ok(out)
    }
    /// Whether any submission for this peer is still queued (used by the reaper
    /// before removing a worktree).
    pub fn has_queued_submission_for_peer(&self, peer: &str) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM submissions WHERE peer = ? AND state = 'queued'",
                params![peer],
                |r| r.get(0),
            )
            .map_err(|e| SwarmError::Store(format!("has queued: {}", e)))?;
        Ok(n > 0)
    }

    /// Fail every open item (budget exhaustion).
    pub fn fail_all_open_items(&self, reason: &str) -> Result<usize> {
        let conn = self.conn();
        let n = conn
            .execute(
                "UPDATE items SET status = 'failed'
                 WHERE status IN ('pending','in_progress','submitted')",
                [],
            )
            .map_err(|e| SwarmError::Store(format!("fail all ({}): {}", reason, e)))?;
        Ok(n)
    }
}

/// Lock-free item fetch for callers that already hold the connection.
fn get_item_inner(conn: &Connection, id: &str) -> Result<Item> {
    conn.query_row(
        &format!("SELECT {} FROM items WHERE id = ?", ITEM_COLUMNS),
        params![id],
        scan_item,
    )
    .optional()
    .map_err(|e| SwarmError::Store(format!("get item: {}", e)))?
    .ok_or_else(|| SwarmError::NotFound(format!("item {:?}", id)))
}

/// Live resource leases, read through a connection the caller already holds.
///
/// The free-function shape is deliberate: the store's `Mutex<Connection>` is not
/// reentrant, so a helper that acquires the lock itself cannot be called from
/// inside another locked section. `active_marks` reads marks and leases together
/// and therefore needs this form.
fn resource_leases_tx(
    conn: &Connection,
    resource: &str,
    limit: i64,
    now: i64,
) -> Result<Vec<ResourceLease>> {
    // One query for both cases: an empty resource matches everything, which is
    // exactly the "all resources" behaviour, and it keeps a single `query_map`
    // (and therefore one concrete row type) instead of two.
    let mut stmt = conn
        .prepare(
            "SELECT resource, token, peer, expires_at FROM resource_leases
             WHERE (? = '' OR resource = ?) AND expires_at > ?
             ORDER BY expires_at ASC LIMIT ?",
        )
        .map_err(|e| SwarmError::Store(format!("leases: {}", e)))?;
    let res = resource.trim();
    let it = stmt
        .query_map(params![res, res, now, limit], |r| {
            Ok(ResourceLease {
                resource: r.get(0)?,
                token: r.get(1)?,
                peer: r.get(2)?,
                expires_at: r.get(3)?,
            })
        })
        .map_err(|e| SwarmError::Store(format!("lease query: {}", e)))?;
    let mut out: Vec<ResourceLease> = Vec::new();
    for r in it {
        out.push(r.map_err(|e| SwarmError::Store(format!("lease row: {}", e)))?);
    }
    Ok(out)
}

/// Resources currently leased by a *different* peer.
///
/// Runs inside the claim transaction so the check and the insert are one atomic
/// step. Returns the blocked resource keys (already normalized) so the caller
/// can name them in the error — an operator seeing
/// `conflict: claim item "x" needs resources held by another peer: src/a.js`
/// can act on it, whereas a bare 409 leaves them guessing.
fn resource_conflicts_tx(
    tx: &rusqlite::Transaction<'_>,
    resources: &[String],
    exclude_peer: &str,
    now: i64,
) -> Result<Vec<String>> {
    let mut blocked = Vec::new();
    for res in resources {
        let mut stmt = tx
            .prepare(
                "SELECT peer FROM resource_leases
                 WHERE resource = ? AND peer <> ? AND expires_at > ? LIMIT 1",
            )
            .map_err(|e| SwarmError::Store(format!("resource conflict: {}", e)))?;
        let held: Option<String> = stmt
            .query_row(params![res, exclude_peer, now], |r| r.get(0))
            .optional()
            .map_err(|e| SwarmError::Store(format!("resource conflict query: {}", e)))?;
        if held.is_some() {
            blocked.push(res.clone());
        }
    }
    Ok(blocked)
}

/// Lock-free conflict scan for callers that already hold the connection.
fn conflicts_for_item_inner(
    conn: &Connection,
    item: &Item,
    exclude_peer: &str,
    now: i64,
) -> Vec<ConflictInfo> {
    let resources = item_resources(&item.title, &item.detail);
    if resources.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for res in resources {
        let mut stmt = match conn.prepare(
            "SELECT peer, COALESCE(item_id,''), ttl_ms FROM events
             WHERE mark_type = 'intent' AND resource = ? AND peer <> ?
               AND (ttl_ms IS NULL OR ts + ttl_ms > ?)",
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows = match stmt.query_map(params![res, exclude_peer, now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in rows.flatten() {
            let (peer, item_id, ttl) = row;
            out.push(ConflictInfo {
                resource: res.clone(),
                peer,
                item_id,
                remaining_ms: ttl.unwrap_or(0),
            });
        }
    }
    out
}
/// Canonical SELECT list, in `scan_item` order (Go `items.go` L96).
const ITEM_COLUMNS: &str = "id, parent_id, title, detail, kind, priority, status, \
                            created_by, created_at, done_at, not_before, seq";

fn scan_item(row: &Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        title: row.get(2)?,
        detail: row.get(3)?,
        kind: row.get(4)?,
        priority: row.get(5)?,
        status: row.get(6)?,
        created_by: row.get(7)?,
        created_at: row.get(8)?,
        done_at: row.get(9)?,
        not_before: row.get(10)?,
        seq: row.get(11)?,
    })
}

fn next_counter(conn: &Connection, name: &str) -> Result<i64> {
    counter_impl(conn, name)
}

/// Both `Connection` and `Transaction` deref to a connection-like executor, so
/// the counter bump is written once against `Connection` and called through
/// `Deref` at both call sites.
fn counter_impl(conn: &Connection, name: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO counters(name, value) VALUES(?, 1)
         ON CONFLICT(name) DO UPDATE SET value = value + 1",
        params![name],
    )
    .map_err(|e| SwarmError::Store(format!("counter {}: {}", name, e)))?;
    conn.query_row(
        "SELECT value FROM counters WHERE name = ?",
        params![name],
        |r| r.get(0),
    )
    .map_err(|e| SwarmError::Store(format!("counter read {}: {}", name, e)))
}

/// Scan one events row into an `Event`.
fn scan_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let tags: Option<String> = row.get(5)?;
    let item_id: String = row.get(3)?;
    Ok(Event {
        id: row.get(0)?,
        peer: row.get(1)?,
        kind: row.get(2)?,
        item_id: if item_id.is_empty() { None } else { Some(item_id) },
        text: row.get(4)?,
        tags: tags.and_then(|t| serde_json::from_str(&t).ok()),
        ts: row.get(6)?,
    })
}


fn history_tx(tx: &rusqlite::Transaction<'_>, item_id: &str) -> Result<Vec<Event>> {
    let mut stmt = tx
        .prepare(
            "SELECT id, peer, kind, COALESCE(item_id,''), text, tags, ts
             FROM events WHERE item_id = ? ORDER BY ts ASC, id ASC LIMIT 50",
        )
        .map_err(|e| SwarmError::Store(format!("history: {}", e)))?;
    let rows = stmt
        .query_map(params![item_id], |r| {
            let tags: Option<String> = r.get(5)?;
            Ok(Event {
                id: r.get(0)?,
                peer: r.get(1)?,
                kind: r.get(2)?,
                item_id: Some(r.get(3)?).filter(|s: &String| !s.is_empty()),
                text: r.get(4)?,
                tags: tags.and_then(|t| serde_json::from_str(&t).ok()),
                ts: r.get(6)?,
            })
        })
        .map_err(|e| SwarmError::Store(format!("history query: {}", e)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| SwarmError::Store(format!("history row: {}", e)))?);
    }
    Ok(out)
}

/// Re-export so callers do not need to reach into `resources`.
pub use crate::resources::{artifact_key as dedup_key, item_resources as resources_of};
