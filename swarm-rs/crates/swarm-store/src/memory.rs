//! Cross-run memory: what one session learned, available to the next.
//!
//! The project board in `.swarm/swarm.db` is deliberately disposable — it dies
//! with the task and holds nothing that outlives it. That is the right shape for
//! coordination, but it means every new run starts from zero knowledge: the same
//! dead end gets walked into again, the same quirk of a tool gets rediscovered,
//! the same "this endpoint is a trap" note is written and thrown away.
//!
//! This module is the other half. It is a small SQLite file in the user's home
//! directory, shared by every run and every project, holding short prose notes
//! keyed by scope.
//!
//! ## What belongs here
//!
//! Notes that would change what a *future* worker does differently:
//!
//!   * a tool or site that behaves in a surprising way
//!   * a dead end that looked promising and cost a lease to disprove
//!   * a project convention that is not written down anywhere in the repo
//!
//! ## What does not
//!
//!   * task state — that is the board's job, and it is per-run
//!   * anything derivable by reading the code
//!   * secrets. This file is plaintext in `$HOME`.
//!
//! ## Scoping
//!
//! `scope` is free-form and matched by exact string or prefix, so callers can mix
//! `project:<path>` (this repo), `tag:<name>` (this kind of work anywhere) and
//! `global` (this machine) in one query. Nothing enforces the prefixes; they are
//! a convention that keeps unrelated streams from crowding each other out.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Row};
use swarm_core::SwarmError;

use crate::schema;

/// One remembered note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub tags: Vec<String>,
    /// Where it came from: a run id, a peer name, or `"operator"`.
    pub source: String,
    pub created_at: i64,
    pub hits: i64,
}

fn scan_memory(r: &Row<'_>) -> rusqlite::Result<Memory> {
    let tags: Option<String> = r.get(4)?;
    Ok(Memory {
        id: r.get(0)?,
        scope: r.get(1)?,
        kind: r.get(2)?,
        text: r.get(3)?,
        tags: tags
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .unwrap_or_default(),
        source: r.get(5)?,
        created_at: r.get(6)?,
        hits: r.get(7)?,
    })
}

const MEMORY_COLUMNS: &str = "id, scope, kind, text, tags, source, created_at, hits";

/// The shared memory file.
///
/// Cheap to clone: the inner `Connection` is behind an `Arc<Mutex<_>>` in the
/// same shape `Store` uses, because the CLI and the server both hold one and the
/// server touches it from several tokio tasks.
#[derive(Clone)]
pub struct MemoryStore {
    inner: std::sync::Arc<std::sync::Mutex<Connection>>,
    path: PathBuf,
    clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("path", &self.path)
            .finish()
    }
}

impl MemoryStore {
    /// Default location: `$SWARM_MEMORY`, else `~/.swarm/memory.db`.
    ///
    /// An env override exists mostly for tests and for keeping a work machine's
    /// memory out of a shared home directory.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("SWARM_MEMORY") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".into());
        Path::new(&home).join(".swarm").join("memory.db")
    }

    /// Open (creating if needed) the memory file at `path`.
    ///
    /// An empty path means "in-memory", which is what tests and `--no-memory`
    /// use.
    pub fn open(path: &str) -> Result<Self, SwarmError> {
        Self::open_with_clock(path, None)
    }

    pub fn open_with_clock(
        path: &str,
        clock: Option<std::sync::Arc<dyn Fn() -> i64 + Send + Sync>>,
    ) -> Result<Self, SwarmError> {
        let (conn, path_buf) = if path.trim().is_empty() {
            (
                Connection::open_in_memory()
                    .map_err(|e| SwarmError::Store(format!("memory open: {e}")))?,
                PathBuf::from(":memory:"),
            )
        } else {
            let p = PathBuf::from(path);
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        SwarmError::Store(format!("memory mkdir {}: {e}", parent.display()))
                    })?;
                }
            }
            (
                Connection::open(&p)
                    .map_err(|e| SwarmError::Store(format!("memory open {path}: {e}")))?,
                p,
            )
        };
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| SwarmError::Store(format!("memory pragma: {e}")))?;
        conn.execute_batch(schema::MEMORY_SCHEMA)
            .map_err(|e| SwarmError::Store(format!("memory migrate: {e}")))?;
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(conn)),
            path: path_buf,
            clock: clock.unwrap_or_else(|| {
                std::sync::Arc::new(|| {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0)
                })
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember one note. Returns the new row id.
    pub fn remember(
        &self,
        scope: &str,
        kind: &str,
        text: &str,
        tags: &[String],
        source: &str,
    ) -> Result<i64, SwarmError> {
        let scope = scope.trim();
        let text = text.trim();
        if scope.is_empty() {
            return Err(SwarmError::Invalid("remember: scope required".into()));
        }
        if text.is_empty() {
            return Err(SwarmError::Invalid("remember: text required".into()));
        }
        let tags = if tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(tags).unwrap_or_default())
        };
        let now = (self.clock)();
        let conn = self.conn();
        // Dedup on (scope, text): the same lesson learned twice in a run is one
        // note, not two. Without this a worker that posts its FACT on every turn
        // fills the file with copies of itself.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM memories WHERE scope = ? AND text = ?",
                params![scope, text],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| SwarmError::Store(format!("memory lookup: {e}")))?;
        if let Some(id) = existing {
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO memories(scope, kind, text, tags, source, created_at)
             VALUES(?,?,?,?,?,?)",
            params![scope, kind, text, tags, source, now],
        )
        .map_err(|e| SwarmError::Store(format!("memory insert: {e}")))?;
        Ok(conn.last_insert_rowid())
    }

    /// Notes matching any of `scopes`, newest first.
    ///
    /// Prefix matching (`scope = ? OR scope LIKE '?/%'`) is what lets a caller
    /// ask for `project:/work/repo` and also pick up `project:/work/repo/sub`.
    /// The escape on `%`/`_` matters: a Windows path or a tag with an underscore
    /// would otherwise turn into a wildcard and match unrelated scopes.
    pub fn recall(&self, scopes: &[String], limit: i64) -> Result<Vec<Memory>, SwarmError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let mut sql = String::from("SELECT ");
        sql.push_str(MEMORY_COLUMNS);
        sql.push_str(" FROM memories WHERE ");
        for (i, _) in scopes.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("(scope = ? OR scope LIKE ? ESCAPE '\\')");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| SwarmError::Store(format!("memory recall prepare: {e}")))?;
        let mut binds: Vec<String> = Vec::with_capacity(scopes.len() * 2);
        for s in scopes {
            let esc = s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            binds.push(s.clone());
            binds.push(format!("{esc}/%"));
        }
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut all: Vec<&dyn rusqlite::ToSql> = params_vec;
        all.push(&limit);
        let rows = stmt
            .query_map(all.as_slice(), scan_memory)
            .map_err(|e| SwarmError::Store(format!("memory recall: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("memory row: {e}")))?);
        }
        Ok(out)
    }

    /// Every note, newest first, regardless of scope.
    ///
    /// For `swarm memory` with no filter — the operator asking "what do I know"
    /// should not have to enumerate scopes first, and an empty query is not the
    /// same question as "only global".
    pub fn all(&self, limit: i64) -> Result<Vec<Memory>, SwarmError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MEMORY_COLUMNS} FROM memories ORDER BY created_at DESC, id DESC LIMIT ?"
            ))
            .map_err(|e| SwarmError::Store(format!("memory all prepare: {e}")))?;
        let rows = stmt
            .query_map(params![limit], scan_memory)
            .map_err(|e| SwarmError::Store(format!("memory all: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("memory all row: {e}")))?);
        }
        Ok(out)
    }

    /// Substring search across every scope, for when the caller does not know
    /// where a note lives.
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Memory>, SwarmError> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.recall(&["global".into()], limit);
        }
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {MEMORY_COLUMNS} FROM memories
                 WHERE LOWER(text) LIKE ?1 OR LOWER(scope) LIKE ?1 OR LOWER(COALESCE(tags,'')) LIKE ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2"
            ))
            .map_err(|e| SwarmError::Store(format!("memory search prepare: {e}")))?;
        let rows = stmt
            .query_map(params![format!("%{q}%"), limit], scan_memory)
            .map_err(|e| SwarmError::Store(format!("memory search: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| SwarmError::Store(format!("memory search row: {e}")))?);
        }
        Ok(out)
    }

    /// Bump `hits`/`last_hit_at` for notes that were actually handed to a
    /// worker. Called with the ids `recall` returned.
    ///
    /// Exists so `prune` can tell "nobody ever looked at this again" from "this
    /// is why the run succeeded", without asking the model to rate its own
    /// memories.
    pub fn note_served(&self, ids: &[i64]) -> Result<(), SwarmError> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = (self.clock)();
        let conn = self.conn();
        let mut stmt = conn
            .prepare("UPDATE memories SET hits = hits + 1, last_hit_at = ? WHERE id = ?")
            .map_err(|e| SwarmError::Store(format!("memory note_served prepare: {e}")))?;
        for id in ids {
            stmt.execute(params![now, id])
                .map_err(|e| SwarmError::Store(format!("memory note_served: {e}")))?;
        }
        Ok(())
    }

    /// Delete notes that were never served and are older than `max_age_ms`.
    ///
    /// A note nobody read is either irrelevant or badly worded; either way it is
    /// noise in the next recall. Never touches a note that was served at least
    /// once — that one earned its place. Returns how many rows went.
    pub fn prune(&self, max_age_ms: i64) -> Result<usize, SwarmError> {
        let cutoff = (self.clock)() - max_age_ms;
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM memories WHERE hits = 0 AND created_at < ?",
                params![cutoff],
            )
            .map_err(|e| SwarmError::Store(format!("memory prune: {e}")))?;
        Ok(n)
    }

    /// Total row count, for `swarm memory` output.
    pub fn len(&self) -> Result<i64, SwarmError> {
        let conn = self.conn();
        conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .map_err(|e| SwarmError::Store(format!("memory len: {e}")))
    }

    pub fn is_empty(&self) -> Result<bool, SwarmError> {
        Ok(self.len()? == 0)
    }

    /// Forget one note by id.
    pub fn forget(&self, id: i64) -> Result<bool, SwarmError> {
        let conn = self.conn();
        let n = conn
            .execute("DELETE FROM memories WHERE id = ?", params![id])
            .map_err(|e| SwarmError::Store(format!("memory forget: {e}")))?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> MemoryStore {
        MemoryStore::open("").unwrap()
    }

    #[test]
    fn remember_then_recall_within_scope() {
        let m = mem();
        m.remember("project:/a", "FACT", "the build needs --offline", &[], "w1")
            .unwrap();
        m.remember("project:/b", "FACT", "unrelated note", &[], "w1")
            .unwrap();
        let got = m.recall(&["project:/a".into()], 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "the build needs --offline");
    }

    #[test]
    fn recall_matches_child_scopes() {
        let m = mem();
        m.remember("project:/a/sub", "FACT", "subproject note", &[], "w1")
            .unwrap();
        let got = m.recall(&["project:/a".into()], 10).unwrap();
        assert_eq!(got.len(), 1, "a prefix scope must see its children");
    }

    #[test]
    fn recall_does_not_treat_path_chars_as_wildcards() {
        let m = mem();
        // A note in a sibling directory whose name differs only by a character
        // that would be a LIKE wildcard if the escape were missing.
        m.remember("project:/a_b", "FACT", "wrong sibling", &[], "w1")
            .unwrap();
        m.remember("project:/axb", "FACT", "wrong sibling 2", &[], "w1")
            .unwrap();
        let got = m.recall(&["project:/a_b".into()], 10).unwrap();
        assert_eq!(got.len(), 1, "`_` must not act as a wildcard");
        assert_eq!(got[0].text, "wrong sibling");
    }

    #[test]
    fn the_same_note_twice_is_one_row() {
        let m = mem();
        let a = m
            .remember("global", "FACT", "cmd.exe eats quotes", &[], "w1")
            .unwrap();
        let b = m
            .remember("global", "FACT", "cmd.exe eats quotes", &[], "w9")
            .unwrap();
        assert_eq!(a, b, "re-posting the same fact must not duplicate it");
        assert_eq!(m.len().unwrap(), 1);
    }

    #[test]
    fn search_spans_every_scope() {
        let m = mem();
        m.remember("project:/a", "FACT", "playwright needs a sandbox flag", &[], "w1")
            .unwrap();
        m.remember("tag:browser", "FACT", "playwright chromium is 400MB", &[], "w2")
            .unwrap();
        let hits = m.search("playwright", 10).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn prune_drops_only_never_served_old_notes() {
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let c = clock.clone();
        let m = MemoryStore::open_with_clock(
            "",
            Some(std::sync::Arc::new(move || c.load(std::sync::atomic::Ordering::SeqCst))),
        )
        .unwrap();
        let stale = m.remember("global", "FACT", "nobody read me", &[], "w1").unwrap();
        let served = m.remember("global", "FACT", "this one helped", &[], "w1").unwrap();
        m.note_served(&[served]).unwrap();

        clock.fetch_add(10_000_000, std::sync::atomic::Ordering::SeqCst);
        let dropped = m.prune(1_000_000).unwrap();
        assert_eq!(dropped, 1, "only the never-served note goes");
        assert!(m.recall(&["global".into()], 10).unwrap().iter().any(|x| x.id == served));
        assert!(!m.recall(&["global".into()], 10).unwrap().iter().any(|x| x.id == stale));
    }

    #[test]
    fn empty_scope_is_rejected() {
        let m = mem();
        assert!(m.remember("", "FACT", "text", &[], "w1").is_err());
        assert!(m.remember("global", "FACT", "   ", &[], "w1").is_err());
    }
}
