// Package store implements the swarm persistence layer on SQLite.
//
// It mirrors the schema and behaviour of the original src/store.js and
// satisfies the server.Store contract (internal/server), so it can be handed
// straight to server.New:
//
//	st, err := store.Open(".swarm/swarm.db")
//	if err != nil { ... }
//	defer st.Close()
//	http.ListenAndServe(addr, server.New(st))
//
// Driver note: this host has no C compiler and CGO_ENABLED=0, so the cgo-based
// github.com/mattn/go-sqlite3 cannot build here. The pure-Go driver
// modernc.org/sqlite is used instead; it registers the driver name "sqlite"
// (NOT "sqlite3"), so the database is opened with sql.Open("sqlite", dsn).
package store

import (
	"database/sql"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	_ "modernc.org/sqlite" // pure-Go SQLite driver; driver name is "sqlite"

	"swarm-go/internal/server"
)

// DriverName is the database/sql driver registered by modernc.org/sqlite.
const DriverName = "sqlite"

// DefaultLeaseMillis is how long a claim stays valid before it may be reaped.
const DefaultLeaseMillis int64 = 300000

// Item lifecycle states, mirrored from the wire protocol.
const (
	StatusPending    = "pending"
	StatusInProgress = "in_progress"
	StatusSubmitted  = "submitted"
	StatusDone       = "done"
	StatusFailed     = "failed"
)

// schema is idempotent and runs on every Open, so an existing database is
// upgraded in place.
const schema = `
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
`

// Store is a SQLite-backed implementation of server.Store.
type Store struct {
	db    *sql.DB
	mu    sync.Mutex
	now   func() int64
	lease int64
}

// compile-time proof that Store satisfies the HTTP layer's contract.
var _ server.Store = (*Store)(nil)

// Open opens (and migrates) the SQLite database at path.
//
// An empty path means ":memory:". Use "file::memory:?cache=shared" for an
// ephemeral store shared between connections.
func Open(path string) (*Store, error) {
	if strings.TrimSpace(path) == "" {
		path = ":memory:"
	}
	db, err := sql.Open(DriverName, path)
	if err != nil {
		return nil, fmt.Errorf("store: open %q: %w", path, err)
	}
	// One connection serialises SQLite writers, which avoids
	// "database is locked" under the worker pool's concurrent claims.
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	db.SetConnMaxLifetime(0)
	if err := db.Ping(); err != nil {
		db.Close()
		return nil, fmt.Errorf("store: ping %q: %w", path, err)
	}
	s, err := New(db, nil)
	if err != nil {
		db.Close()
		return nil, err
	}
	return s, nil
}

// OpenMemory opens an ephemeral in-memory store (handy for tests).
func OpenMemory() (*Store, error) {
	return Open("file::memory:?cache=shared")
}

// New wraps an already-open *sql.DB, runs the schema migration and returns a
// Store. now may be nil, in which case wall-clock milliseconds are used.
func New(db *sql.DB, now func() int64) (*Store, error) {
	if db == nil {
		return nil, fmt.Errorf("store: nil db")
	}
	if now == nil {
		now = func() int64 { return time.Now().UnixMilli() }
	}
	s := &Store{db: db, now: now, lease: DefaultLeaseMillis}
	if _, err := db.Exec(schema); err != nil {
		return nil, fmt.Errorf("store: migrate: %w", err)
	}
	return s, nil
}

// Close releases the underlying database handle.
func (s *Store) Close() error { return s.db.Close() }

// DB exposes the raw handle (migrations, diagnostics, tests).
func (s *Store) DB() *sql.DB { return s.db }

// SetClock overrides the clock used for created_at / lease expiry.
func (s *Store) SetClock(now func() int64) {
	if now == nil {
		return
	}
	s.mu.Lock()
	s.now = now
	s.mu.Unlock()
}

// SetLease changes the claim lease duration in milliseconds.
func (s *Store) SetLease(ms int64) {
	if ms <= 0 {
		return
	}
	s.mu.Lock()
	s.lease = ms
	s.mu.Unlock()
}

func (s *Store) clock() int64 {
	s.mu.Lock()
	fn := s.now
	s.mu.Unlock()
	return fn()
}

func (s *Store) leaseMillis() int64 {
	s.mu.Lock()
	v := s.lease
	s.mu.Unlock()
	return v
}

// ---- id sequences -------------------------------------------------------

// nextSeqTx returns the next id for prefix ("t" for items, "c" for claims).
// It must run inside a transaction so ids stay unique across processes.
func nextSeqTx(tx *sql.Tx, prefix string) (string, error) {
	if _, err := tx.Exec(`INSERT INTO counters(name, value) VALUES(?, 1)
		ON CONFLICT(name) DO UPDATE SET value = value + 1`, prefix); err != nil {
		return "", fmt.Errorf("store: next %s id: %w", prefix, err)
	}
	var v int64
	if err := tx.QueryRow(`SELECT value FROM counters WHERE name = ?`, prefix).Scan(&v); err != nil {
		return "", fmt.Errorf("store: next %s id: %w", prefix, err)
	}
	return fmt.Sprintf("%s%d", prefix, v), nil
}

// ---- error helpers ------------------------------------------------------

// mapErr converts a raw SQL error into the server sentinels the HTTP layer
// understands.
func mapErr(op string, err error) error {
	if err == nil {
		return nil
	}
	if errors.Is(err, sql.ErrNoRows) {
		return fmt.Errorf("store: %s: %w", op, server.ErrNotFound)
	}
	return fmt.Errorf("store: %s: %w", op, err)
}

// notFound builds a 404-mapped error.
func notFound(op, id string) error {
	return fmt.Errorf("store: %s %q: %w", op, id, server.ErrNotFound)
}

// conflict builds a 409-mapped error.
func conflict(op, id, why string) error {
	return fmt.Errorf("store: %s %q %s: %w", op, id, why, server.ErrConflict)
}

// invalid builds a 400-mapped error.
func invalid(op, why string) error {
	return fmt.Errorf("store: %s: %s: %w", op, why, server.ErrInvalid)
}
