package store

// Stigmergy mark layer — pheromone-style coordination traces.
//
// Design: agents do not message each other; they leave traces in the shared
// environment. A worker declares "I am about to touch resource R" (an Intent
// mark) before it edits, and "I did X, result Y" (an Action mark) after.
// Other workers read the traces and route around conflicts themselves — no
// scheduler decides who may run in parallel.
//
// This is the markspace / PheroPath model: "Context is attached to the file,
// not the prompt." Traces live in infrastructure the agent cannot bypass, so
// coordination does not depend on the model choosing to comply.
//
// Storage: the existing events table, widened by three nullable columns
// (mark_type / resource / ttl_ms). Additive only — old rows and old callers
// are unaffected.

import (
	"database/sql"
	"encoding/json"
	"strings"
	"time"

	"swarm-go/internal/server"
)

// Mark types. Purpose determines lifecycle: facts are permanent, intents
// expire if not acted on, observations decay.
const (
	MarkIntent       = "INTENT"       // "I plan to touch resource R" — expires after TTL
	MarkAction       = "ACTION"       // "I touched R, result Y" — permanent ground truth
	MarkObservation  = "OBSERVATION"  // "I saw Y about the world" — decays
	MarkWarning      = "WARNING"      // "R's earlier conclusion is stale" — spikes then decays
	MarkNeed         = "NEED"         // "needs a human decision" — persists until resolved
	MarkClaimExpired = "CLAIM_EXPIRED" // bookkeeping: an intent outlived its lease
)

// DefaultIntentTTL is how long an unrefreshed intent stays sticky.
const DefaultIntentTTL = 15 * time.Minute

// IsKnownMark reports whether t is one of the five protocol marks.
func IsKnownMark(t string) bool {
	switch strings.ToUpper(strings.TrimSpace(t)) {
	case MarkIntent, MarkAction, MarkObservation, MarkWarning, MarkNeed, MarkClaimExpired:
		return true
	}
	return false
}

// migrateMarks widens the events table with the three mark columns.
// Idempotent: duplicate-column errors are swallowed.
func (s *Store) migrateMarks() error {
	stmts := []string{
		`ALTER TABLE events ADD COLUMN mark_type TEXT`,
		`ALTER TABLE events ADD COLUMN resource TEXT`,
		`ALTER TABLE events ADD COLUMN ttl_ms INTEGER`,
		`CREATE INDEX IF NOT EXISTS events_resource_idx ON events(resource)`,
		`CREATE INDEX IF NOT EXISTS events_mark_idx ON events(mark_type)`,
	}
	for _, stmt := range stmts {
		if _, err := s.db.Exec(stmt); err != nil {
			if strings.Contains(err.Error(), "duplicate column name") {
				continue
			}
			return mapErr("migrate marks", err)
		}
	}
	return nil
}

// AddMark writes a coordination trace. Empty resource is allowed (a global
// fact), but marks with a resource are what make conflict avoidance work.
func (s *Store) AddMark(entry server.ContextEntry) (server.ContextEntry, error) {
	if err := s.migrateMarks(); err != nil {
		return server.ContextEntry{}, err
	}
	mark := strings.ToUpper(strings.TrimSpace(entry.Kind))
	if !IsKnownMark(mark) {
		return server.ContextEntry{}, invalid("add mark", "unknown mark type: "+entry.Kind)
	}
	text := strings.TrimSpace(entry.Text)
	if text == "" {
		return server.ContextEntry{}, invalid("add mark", "text required")
	}
	ts := entry.TS
	if ts == 0 {
		ts = s.clock()
	}
	var itemID any
	if entry.ItemID != nil && strings.TrimSpace(*entry.ItemID) != "" {
		itemID = strings.TrimSpace(*entry.ItemID)
	}
	resource := strings.TrimSpace(entry.Resource)
	var ttlMs any
	if entry.TTLMs > 0 {
		ttlMs = entry.TTLMs
	}

	res, err := s.db.Exec(
		`INSERT INTO events(peer, kind, item_id, text, tags, ts, mark_type, resource, ttl_ms)
		 VALUES(?,?,?,?,?,?,?,?,?)`,
		strings.TrimSpace(entry.Peer), mark, itemID, text, encodeTags(entry.Tags), ts, mark, resource, ttlMs)
	if err != nil {
		return server.ContextEntry{}, mapErr("add mark", err)
	}
	id, err := res.LastInsertId()
	if err != nil {
		return server.ContextEntry{}, mapErr("add mark", err)
	}
	out := entry
	out.ID = id
	out.Kind = mark
	out.Text = text
	out.TS = ts
	out.Resource = resource
	if itemID != nil {
		v := itemID.(string)
		out.ItemID = &v
	}
	return out, nil
}

// ActiveMarks returns live (unexpired) marks touching resource, newest last.
// Expired INTENT marks are treated as absent; ACTION marks never expire.
// resource == "" returns every live mark (used by the turn brief).
func (s *Store) ActiveMarks(resource string, limit int) ([]server.ContextEntry, error) {
	if err := s.migrateMarks(); err != nil {
		return nil, err
	}
	if limit <= 0 {
		limit = 50
	}
	now := s.clock()
	// Live = permanent (ttl_ms IS NULL) or not yet past ts + ttl_ms.
	ttlClause := `(ttl_ms IS NULL OR (ts + ttl_ms) > ?)`
	args := []any{now}
	where := `mark_type IS NOT NULL AND ` + ttlClause
	if resource != "" {
		where += ` AND resource = ?`
		args = append(args, resource)
	}
	args = append(args, limit)

	rows, err := s.db.Query(
		`SELECT `+eventColumns+`, COALESCE(mark_type,''), COALESCE(resource,''), COALESCE(ttl_ms,0)
		 FROM events WHERE `+where+` ORDER BY id ASC LIMIT ?`, args...)
	if err != nil {
		return nil, mapErr("active marks", err)
	}
	defer rows.Close()

	out := []server.ContextEntry{}
	for rows.Next() {
		e, markType, res, ttlMs, serr := scanMarkRow(rows)
		if serr != nil {
			return nil, serr
		}
		e.Kind = markType
		e.Resource = res
		e.TTLMs = ttlMs
		out = append(out, e)
	}
	return out, rows.Err()
}

// MarksFor returns live marks on any of the given resources (batch form of
// ActiveMarks). Used by the turn brief to pull only relevant traces.
func (s *Store) MarksFor(resources []string, limit int) ([]server.ContextEntry, error) {
	if err := s.migrateMarks(); err != nil {
		return nil, err
	}
	if len(resources) == 0 {
		return nil, nil
	}
	if limit <= 0 {
		limit = 100
	}
	now := s.clock()
	placeholders := strings.TrimSuffix(strings.Repeat("?,", len(resources)), ",")
	// placeholder order: ttl_now, resource..., limit
	args := []any{now}
	for _, r := range resources {
		args = append(args, r)
	}
	args = append(args, limit)

	rows, err := s.db.Query(
		`SELECT `+eventColumns+`, COALESCE(mark_type,''), COALESCE(resource,''), COALESCE(ttl_ms,0)
		 FROM events
		 WHERE mark_type IS NOT NULL AND (ttl_ms IS NULL OR (ts + ttl_ms) > ?)
		   AND resource IN (`+placeholders+`)
		 ORDER BY id ASC LIMIT ?`, args...)
	if err != nil {
		return nil, mapErr("marks for", err)
	}
	defer rows.Close()

	out := []server.ContextEntry{}
	for rows.Next() {
		e, markType, res, ttlMs, serr := scanMarkRow(rows)
		if serr != nil {
			return nil, serr
		}
		e.Kind = markType
		e.Resource = res
		e.TTLMs = ttlMs
		out = append(out, e)
	}
	return out, rows.Err()
}

// Conflict is one live intent held by another peer on a resource.
type Conflict struct {
	Resource   string `json:"resource"`
	Peer       string `json:"peer"`
	ItemID     string `json:"itemId,omitempty"`
	MarkID     int64  `json:"markId"`
	RemainingMs int64 `json:"remainingMs"`
}

// ConflictsFor lists live INTENT marks on the given resources, optionally
// excluding the calling peer (a peer never conflicts with itself).
// Returned by Claim so the worker learns "w3 is editing src/foo.js#fn"
// without any central scheduler making the call for it.
func (s *Store) ConflictsFor(resources []string, excludePeer string) ([]Conflict, error) {
	if err := s.migrateMarks(); err != nil {
		return nil, err
	}
	if len(resources) == 0 {
		return nil, nil
	}
	now := s.clock()
	placeholders := strings.TrimSuffix(strings.Repeat("?,", len(resources)), ",")
	// placeholder order: mark_type, ttl_now, resource..., [peer]
	args := []any{MarkIntent, now}
	for _, r := range resources {
		args = append(args, r)
	}
	if p := strings.TrimSpace(excludePeer); p != "" {
		args = append(args, p)
	}

	exclude := ""
	if strings.TrimSpace(excludePeer) != "" {
		exclude = ` AND peer != ?`
	}
	rows, err := s.db.Query(
		`SELECT id, peer, COALESCE(item_id,''), resource, ts, COALESCE(ttl_ms,0)
		 FROM events
		 WHERE mark_type = ?
		   AND (ttl_ms IS NULL OR (ts + ttl_ms) > ?)
		   AND resource IN (`+placeholders+`)`+exclude+`
		 ORDER BY id ASC`, args...)
	if err != nil {
		return nil, mapErr("conflicts for", err)
	}
	defer rows.Close()

	out := []Conflict{}
	for rows.Next() {
		var (
			c        Conflict
			itemID   sql.NullString
			ts       int64
			ttlMs    int64
		)
		if serr := rows.Scan(&c.MarkID, &c.Peer, &itemID, &c.Resource, &ts, &ttlMs); serr != nil {
			return nil, mapErr("conflicts for scan", serr)
		}
		c.ItemID = itemID.String
		if ttlMs > 0 {
			c.RemainingMs = ts + ttlMs - now
			if c.RemainingMs < 0 {
				c.RemainingMs = 0
			}
		}
		out = append(out, c)
	}
	return out, rows.Err()
}

// ReleaseMarks drops every live INTENT mark held by peer (called when a
// worker's claim ends, so a crashed worker's traces do not linger).
func (s *Store) ReleaseMarks(peer string) (int, error) {
	if err := s.migrateMarks(); err != nil {
		return 0, err
	}
	peer = strings.TrimSpace(peer)
	if peer == "" {
		return 0, invalid("release marks", "peer required")
	}
	// Soft-delete by rewriting kind: keep the row as an audit trail but make
	// it invisible to ActiveMarks / ConflictsFor.
	res, err := s.db.Exec(
		`UPDATE events SET kind = 'OBSERVED', mark_type = NULL, ttl_ms = NULL
		 WHERE mark_type = ? AND peer = ?`, MarkIntent, peer)
	if err != nil {
		return 0, mapErr("release marks", err)
	}
	n, err := res.RowsAffected()
	if err != nil {
		return 0, mapErr("release marks rows", err)
	}
	return int(n), nil
}

// ReapExpiredMarks clears intent marks whose TTL has elapsed, returning how
// many were reaped. The reaper calls this alongside ReapExpired.
func (s *Store) ReapExpiredMarks() (int, error) {
	if err := s.migrateMarks(); err != nil {
		return 0, err
	}
	now := s.clock()
	res, err := s.db.Exec(
		`UPDATE events SET kind = 'OBSERVED', mark_type = NULL, ttl_ms = NULL
		 WHERE mark_type = ? AND ttl_ms IS NOT NULL AND (ts + ttl_ms) <= ?`,
		MarkIntent, now)
	if err != nil {
		return 0, mapErr("reap expired marks", err)
	}
	n, err := res.RowsAffected()
	if err != nil {
		return 0, mapErr("reap expired marks rows", err)
	}
	return int(n), nil
}

// scanMarkRow reads one mark row: the canonical event columns plus the three
// mark columns. Column order matches eventColumns exactly
// (id, peer, kind, text, item_id, tags, ts) so scanEvent stays the single
// source of truth for that prefix.
func scanMarkRow(rows *sql.Rows) (server.ContextEntry, string, string, int64, error) {
	var (
		id       int64
		peer     string
		kind     string
		text     string
		itemID   sql.NullString
		tags     sql.NullString
		ts       int64
		markType string
		resource string
		ttlMs    int64
	)
	if err := rows.Scan(&id, &peer, &kind, &text, &itemID, &tags, &ts,
		&markType, &resource, &ttlMs); err != nil {
		return server.ContextEntry{}, "", "", 0, mapErr("scan mark", err)
	}
	e := server.ContextEntry{
		ID:   id,
		Peer: peer,
		Kind: kind,
		Text: text,
		TS:   ts,
	}
	if itemID.Valid {
		v := itemID.String
		e.ItemID = &v
	}
	if tags.Valid && tags.String != "" {
		var list []string
		if err := json.Unmarshal([]byte(tags.String), &list); err == nil {
			e.Tags = list
		}
	}
	return e, markType, resource, ttlMs, nil
}
