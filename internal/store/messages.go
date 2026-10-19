package store

import (
	"strings"

	"swarm-go/internal/server"
)

// SendMessage stores a peer-to-peer note and returns it with id/timestamp.
func (s *Store) SendMessage(m server.Message) (server.Message, error) {
	to := strings.TrimSpace(m.To)
	text := strings.TrimSpace(m.Text)
	if to == "" || text == "" {
		return server.Message{}, invalid("send message", "to and text required")
	}
	ts := m.TS
	if ts == 0 {
		ts = s.clock()
	}
	res, err := s.db.Exec(`INSERT INTO messages("from", "to", text, ts, read) VALUES(?,?,?,?,0)`,
		strings.TrimSpace(m.From), to, text, ts)
	if err != nil {
		return server.Message{}, mapErr("send message", err)
	}
	id, err := res.LastInsertId()
	if err != nil {
		return server.Message{}, mapErr("send message", err)
	}
	out := m
	out.ID = id
	out.To = to
	out.Text = text
	out.TS = ts
	return out, nil
}

// Messages returns the notes addressed to peer (all recipients when peer is
// empty) with ts >= since, oldest first.
func (s *Store) Messages(peer string, since int64) ([]server.Message, error) {
	peer = strings.TrimSpace(peer)

	rows, err := s.db.Query(`SELECT id, "from", "to", text, ts, read FROM messages
		WHERE (? = '' OR "to" = ?) AND ts >= ?
		ORDER BY id ASC`, peer, peer, since)
	if err != nil {
		return nil, mapErr("messages", err)
	}
	defer rows.Close()

	out := []server.Message{}
	for rows.Next() {
		var m server.Message
		var read int
		if err := rows.Scan(&m.ID, &m.From, &m.To, &m.Text, &m.TS, &read); err != nil {
			return nil, mapErr("messages", err)
		}
		m.Read = read != 0
		out = append(out, m)
	}
	if err := rows.Err(); err != nil {
		return nil, mapErr("messages", err)
	}
	return out, nil
}
