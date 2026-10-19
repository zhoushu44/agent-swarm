package store

import (
	"database/sql"
	"encoding/json"
	"fmt"

	"swarm-go/internal/server"
)

// idFor renders a sequenced id such as "t12" / "c7" from a counter value.
func idFor(prefix string, n int64) string {
	return fmt.Sprintf("%s%d", prefix, n)
}

// nextCounterTx atomically increments the named counter and returns the new
// value. It must run inside a transaction so ids stay unique.
func nextCounterTx(tx *sql.Tx, name string) (int64, error) {
	if _, err := tx.Exec(`INSERT INTO counters(name, value) VALUES(?, 1)
		ON CONFLICT(name) DO UPDATE SET value = value + 1`, name); err != nil {
		return 0, mapErr("next "+name, err)
	}
	var v int64
	if err := tx.QueryRow(`SELECT value FROM counters WHERE name = ?`, name).Scan(&v); err != nil {
		return 0, mapErr("next "+name, err)
	}
	return v, nil
}

// getItemTx loads one item inside a transaction (sql.ErrNoRows when absent).
func getItemTx(tx *sql.Tx, id string) (server.Item, error) {
	row := tx.QueryRow(`SELECT `+itemColumns+` FROM items WHERE id = ?`, id)
	return scanItem(row)
}

// historyTx returns the shared-memory entries attached to an item, oldest
// first. It is what POST /claim hands back as the task package history.
func historyTx(tx *sql.Tx, itemID string) ([]server.ContextEntry, error) {
	rows, err := tx.Query(`SELECT id, peer, kind, text, item_id, tags, ts FROM events
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

// scanEvent reads one events row.
func scanEvent(row rowScanner) (server.ContextEntry, error) {
	var (
		e      server.ContextEntry
		itemID sql.NullString
		tags   sql.NullString
	)
	if err := row.Scan(&e.ID, &e.Peer, &e.Kind, &e.Text, &itemID, &tags, &e.TS); err != nil {
		return server.ContextEntry{}, err
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
	return e, nil
}

// encodeTags serialises tags for storage; nil is stored as NULL.
func encodeTags(tags []string) any {
	if len(tags) == 0 {
		return nil
	}
	buf, err := json.Marshal(tags)
	if err != nil {
		return nil
	}
	return string(buf)
}
