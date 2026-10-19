package store

import (
	"strings"

	"swarm-go/internal/server"
)

// eventColumns is the canonical SELECT list for shared-memory events.
const eventColumns = `id, peer, kind, text, item_id, tags, ts`

// AddContext appends a shared-memory note (FACT / OBSERVED / FAIL /
// PATCH_SUMMARY / CONFLICT) and returns it with its assigned id and timestamp.
func (s *Store) AddContext(entry server.ContextEntry) (server.ContextEntry, error) {
	text := strings.TrimSpace(entry.Text)
	if text == "" {
		return server.ContextEntry{}, invalid("add context", "text required")
	}
	kind := strings.ToUpper(strings.TrimSpace(entry.Kind))
	if kind == "" {
		kind = "OBSERVED"
	}
	ts := entry.TS
	if ts == 0 {
		ts = s.clock()
	}

	var itemID any
	if entry.ItemID != nil && strings.TrimSpace(*entry.ItemID) != "" {
		itemID = strings.TrimSpace(*entry.ItemID)
	}

	res, err := s.db.Exec(`INSERT INTO events(peer, kind, item_id, text, tags, ts) VALUES(?,?,?,?,?,?)`,
		strings.TrimSpace(entry.Peer), kind, itemID, text, encodeTags(entry.Tags), ts)
	if err != nil {
		return server.ContextEntry{}, mapErr("add context", err)
	}
	id, err := res.LastInsertId()
	if err != nil {
		return server.ContextEntry{}, mapErr("add context", err)
	}

	out := entry
	out.ID = id
	out.Kind = kind
	out.Text = text
	out.TS = ts
	if itemID != nil {
		v := itemID.(string)
		out.ItemID = &v
	}
	return out, nil
}

// SearchContext returns up to limit notes whose text contains q
// (case-insensitive), newest last. An empty q matches everything.
func (s *Store) SearchContext(q string, limit int) ([]server.ContextEntry, error) {
	if limit <= 0 {
		limit = 50
	}
	q = strings.TrimSpace(q)

	rows, err := s.db.Query(`SELECT `+eventColumns+` FROM events
		WHERE ? = '' OR instr(lower(text), lower(?)) > 0
		ORDER BY id DESC LIMIT ?`, q, q, limit)
	if err != nil {
		return nil, mapErr("search context", err)
	}
	defer rows.Close()

	out := []server.ContextEntry{}
	for rows.Next() {
		e, err := scanEvent(rows)
		if err != nil {
			return nil, mapErr("search context", err)
		}
		out = append(out, e)
	}
	if err := rows.Err(); err != nil {
		return nil, mapErr("search context", err)
	}
	// Newest-last, matching the JS store's ordering.
	for i, j := 0, len(out)-1; i < j; i, j = i+1, j-1 {
		out[i], out[j] = out[j], out[i]
	}
	return out, nil
}

// ItemHistory returns every note attached to itemID, oldest first.
func (s *Store) ItemHistory(itemID string) ([]server.ContextEntry, error) {
	itemID = strings.TrimSpace(itemID)
	if itemID == "" {
		return nil, invalid("item history", "missing itemId")
	}
	rows, err := s.db.Query(`SELECT `+eventColumns+` FROM events
		WHERE item_id = ? ORDER BY id ASC`, itemID)
	if err != nil {
		return nil, mapErr("item history", err)
	}
	defer rows.Close()

	out := []server.ContextEntry{}
	for rows.Next() {
		e, err := scanEvent(rows)
		if err != nil {
			return nil, mapErr("item history", err)
		}
		out = append(out, e)
	}
	if err := rows.Err(); err != nil {
		return nil, mapErr("item history", err)
	}
	return out, nil
}
