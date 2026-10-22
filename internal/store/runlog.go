package store

// Structured run log.
//
// A run's stdout is transient; the database is not. Everything an operator or
// a later agent needs to understand "what happened and why" must land in a
// table, in a shape that can be diffed, grepped and replayed.
//
// Design rules (learned from runs where only stdout existed):
//   - One row per meaningful event, never a wall of prose.
//   - Every row carries the actor (worker / integrator / system) and the item
//     it concerns, so a reader can filter by either.
//   - Outcomes are a closed vocabulary, so a later agent can branch on them
//     without parsing English.
//   - Numbers (tokens, files, seconds) are columns, not text, so trends are
//     queryable.

import (
	"database/sql"
	"fmt"
	"strings"
	"time"

	"swarm-go/internal/server"
)

// Run event kinds. Closed vocabulary: a consumer may switch on these.
const (
	// Run lifecycle
	EvRunStart    = "RUN_START"    // swarm started, with its config
	EvRunStop     = "RUN_STOP"     // swarm stopped, with the reason
	EvSeedLoaded  = "SEED_LOADED"  // seed items created
	EvSeedDone    = "SEED_DONE"    // all seed items reached a terminal state
	EvWallClock   = "WALL_CLOCK"   // wall-clock ceiling hit

	// Item lifecycle
	EvItemCreated = "ITEM_CREATED" // an item appeared (seed / worker / auto-split)
	EvItemClaimed = "ITEM_CLAIMED" // a worker took the lease
	EvItemSplit   = "ITEM_SPLIT"   // an item was decomposed into children
	EvItemDone    = "ITEM_DONE"    // merged into integration and gated green
	EvItemFailed  = "ITEM_FAILED"  // rejected, or exhausted its budget
	EvItemReleased = "ITEM_RELEASED" // lease dropped before finishing (stall, timeout)

	// Work
	EvSubmit      = "SUBMIT"       // a worker asked for integration
	EvMerge       = "MERGE"        // integrator merged a branch
	EvMergeReject = "MERGE_REJECT" // integrator refused (gate / conflict / scratch)
	EvArbitrate   = "ARBITRATE"    // a merge conflict was resolved by the model
	EvGate        = "GATE"         // gate command result
	EvWorkerPark  = "WORKER_PARK"  // a worker was parked (no-progress / stall)
	EvWorkerRetire = "WORKER_RETIRE" // a worker left because there was nothing to do
)

// RunEvent is one row of the structured log.
type RunEvent struct {
	ID     int64  `json:"id"`
	Kind   string `json:"kind"`
	Actor  string `json:"actor"`   // worker name, "integrator", "system"
	ItemID string `json:"itemId,omitempty"`
	Title  string `json:"title,omitempty"`
	Detail string `json:"detail,omitempty"` // one line, human readable
	// Metrics are columns so trends are queryable.
	Tokens  int64 `json:"tokens,omitempty"`
	Files   int   `json:"files,omitempty"`
	Seconds int   `json:"seconds,omitempty"`
	TS      int64 `json:"ts"`
}

// migrateRunLog creates the run_log table. Idempotent.
func (s *Store) migrateRunLog() error {
	_, err := s.db.Exec(`CREATE TABLE IF NOT EXISTS run_log (
		id      INTEGER PRIMARY KEY AUTOINCREMENT,
		kind    TEXT NOT NULL,
		actor   TEXT NOT NULL DEFAULT '',
		item_id TEXT,
		title   TEXT,
		detail  TEXT,
		tokens  INTEGER NOT NULL DEFAULT 0,
		files   INTEGER NOT NULL DEFAULT 0,
		seconds INTEGER NOT NULL DEFAULT 0,
		ts      INTEGER NOT NULL DEFAULT 0
	);
	CREATE INDEX IF NOT EXISTS run_log_kind_idx ON run_log(kind);
	CREATE INDEX IF NOT EXISTS run_log_item_idx ON run_log(item_id);
	CREATE INDEX IF NOT EXISTS run_log_ts_idx ON run_log(ts);`)
	if err != nil {
		return mapErr("migrate run log", err)
	}
	return nil
}

// LogRunEvent appends one structured event. Never fails a run: a logging
// problem is reported, not fatal.
func (s *Store) LogRunEvent(ev RunEvent) error {
	if err := s.migrateRunLog(); err != nil {
		return err
	}
	kind := strings.ToUpper(strings.TrimSpace(ev.Kind))
	if kind == "" {
		return invalid("log run event", "kind required")
	}
	ts := ev.TS
	if ts == 0 {
		ts = s.clock()
	}
	var itemID any
	if strings.TrimSpace(ev.ItemID) != "" {
		itemID = strings.TrimSpace(ev.ItemID)
	}
	_, err := s.db.Exec(
		`INSERT INTO run_log(kind, actor, item_id, title, detail, tokens, files, seconds, ts)
		 VALUES(?,?,?,?,?,?,?,?,?)`,
		kind, strings.TrimSpace(ev.Actor), itemID,
		strings.TrimSpace(ev.Title), strings.TrimSpace(ev.Detail),
		ev.Tokens, ev.Files, ev.Seconds, ts)
	if err != nil {
		return mapErr("log run event", err)
	}
	return nil
}

// RunLog returns events, newest first, optionally filtered by kind.
func (s *Store) RunLog(kind string, limit int) ([]RunEvent, error) {
	if err := s.migrateRunLog(); err != nil {
		return nil, err
	}
	if limit <= 0 {
		limit = 200
	}
	kind = strings.ToUpper(strings.TrimSpace(kind))
	var rows *sql.Rows
	var err error
	if kind == "" {
		rows, err = s.db.Query(
			`SELECT id, kind, actor, COALESCE(item_id,''), COALESCE(title,''), COALESCE(detail,''),
			        tokens, files, seconds, ts
			 FROM run_log ORDER BY id DESC LIMIT ?`, limit)
	} else {
		rows, err = s.db.Query(
			`SELECT id, kind, actor, COALESCE(item_id,''), COALESCE(title,''), COALESCE(detail,''),
			        tokens, files, seconds, ts
			 FROM run_log WHERE kind = ? ORDER BY id DESC LIMIT ?`, kind, limit)
	}
	if err != nil {
		return nil, mapErr("run log", err)
	}
	defer rows.Close()
	out := []RunEvent{}
	for rows.Next() {
		var e RunEvent
		var itemID, title, detail string
		if serr := rows.Scan(&e.ID, &e.Kind, &e.Actor, &itemID, &title, &detail,
			&e.Tokens, &e.Files, &e.Seconds, &e.TS); serr != nil {
			return nil, mapErr("run log scan", serr)
		}
		e.ItemID, e.Title, e.Detail = itemID, title, detail
		out = append(out, e)
	}
	return out, rows.Err()
}

// ItemTimeline returns every event for one item, oldest first — the "why is
// this item in this state" view a later agent needs.
func (s *Store) ItemTimeline(itemID string) ([]RunEvent, error) {
	if err := s.migrateRunLog(); err != nil {
		return nil, err
	}
	itemID = strings.TrimSpace(itemID)
	if itemID == "" {
		return nil, invalid("item timeline", "missing itemId")
	}
	rows, err := s.db.Query(
		`SELECT id, kind, actor, COALESCE(item_id,''), COALESCE(title,''), COALESCE(detail,''),
		        tokens, files, seconds, ts
		 FROM run_log WHERE item_id = ? ORDER BY id ASC`, itemID)
	if err != nil {
		return nil, mapErr("item timeline", err)
	}
	defer rows.Close()
	out := []RunEvent{}
	for rows.Next() {
		var e RunEvent
		var iid, title, detail string
		if serr := rows.Scan(&e.ID, &e.Kind, &e.Actor, &iid, &title, &detail,
			&e.Tokens, &e.Files, &e.Seconds, &e.TS); serr != nil {
			return nil, mapErr("item timeline scan", serr)
		}
		e.ItemID, e.Title, e.Detail = iid, title, detail
		out = append(out, e)
	}
	return out, rows.Err()
}

// FormatEvent renders one event as a single log line, for both stdout and
// the DB detail column. Format:
//
//	[MM:SS] KIND actor item title — detail (tokens/files/seconds)
func FormatEvent(ev RunEvent, startTS int64) string {
	ts := time.UnixMilli(ev.TS)
	rel := ""
	if startTS > 0 && ev.TS >= startTS {
		d := time.Duration(ev.TS-startTS) * time.Millisecond
		rel = fmtDuration(d) + " "
	}
	line := rel + ts.Format("15:04:05") + " " + pad(ev.Kind, 12)
	if ev.Actor != "" {
		line += " " + pad(ev.Actor, 11)
	}
	if ev.ItemID != "" {
		line += " " + pad(ev.ItemID, 6)
	}
	if ev.Title != "" {
		line += " " + ev.Title
	}
	if ev.Detail != "" {
		line += " — " + ev.Detail
	}
	var metrics []string
	if ev.Tokens > 0 {
		metrics = append(metrics, fmt.Sprintf("%d tok", ev.Tokens))
	}
	if ev.Files > 0 {
		metrics = append(metrics, fmt.Sprintf("%d files", ev.Files))
	}
	if ev.Seconds > 0 {
		metrics = append(metrics, fmt.Sprintf("%ds", ev.Seconds))
	}
	if len(metrics) > 0 {
		line += "  [" + strings.Join(metrics, ", ") + "]"
	}
	return line
}

func pad(s string, n int) string {
	if len(s) >= n {
		return s
	}
	return s + strings.Repeat(" ", n-len(s))
}

func fmtDuration(d time.Duration) string {
	m := int(d.Minutes())
	s := int(d.Seconds()) % 60
	return fmt.Sprintf("%02d:%02d", m, s)
}

// Timeline is a helper for report rendering: the item's events as lines.
func (s *Store) TimelineLines(itemID string, startTS int64) []string {
	evs, err := s.ItemTimeline(itemID)
	if err != nil {
		return []string{"  (timeline unavailable: " + err.Error() + ")"}
	}
	out := make([]string, 0, len(evs))
	for _, e := range evs {
		out = append(out, "  "+FormatEvent(e, startTS))
	}
	return out
}

// ensure server import stays used if Item is referenced elsewhere.
var _ = server.Item{}
