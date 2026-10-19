package store

import (
	"strings"

	"swarm-go/internal/server"
)

// Digest is the single-call turn briefing: board census, pending preview, the
// caller's live claims, everything published since `since`, and the caller's
// messages. It mirrors GET /digest in src/server.js and the semantics of
// server.MemStore so both stores behave identically.
func (s *Store) Digest(peer string, since int64) (server.Digest, error) {
	peer = strings.TrimSpace(peer)
	d := server.Digest{Now: s.clock()}

	// Board census by status.
	rows, err := s.db.Query(`SELECT status, COUNT(*) FROM items GROUP BY status`)
	if err != nil {
		return server.Digest{}, mapErr("digest stats", err)
	}
	for rows.Next() {
		var status string
		var n int
		if err := rows.Scan(&status, &n); err != nil {
			rows.Close()
			return server.Digest{}, mapErr("digest stats", err)
		}
		d.Stats.Total += n
		switch status {
		case StatusPending:
			d.Stats.Pending = n
		case StatusInProgress:
			d.Stats.InProgress = n
		case StatusSubmitted:
			d.Stats.Submitted = n
		case StatusDone:
			d.Stats.Done = n
		case StatusFailed:
			d.Stats.Failed = n
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return server.Digest{}, mapErr("digest stats", err)
	}

	// Pending preview: the top of the queue, priority desc then FIFO.
	preview, _, err := s.ListItems(StatusPending, 5, 0)
	if err != nil {
		return server.Digest{}, err
	}
	d.PendingPreview = preview

	// The caller's live claims. server.Claim carries no peer field, so (like
	// MemStore) ownership is derived from the claimed item's creator.
	d.Claims = []server.Claim{}
	claimRows, err := s.db.Query(`SELECT c.token, c.item_id, c.expires_at, i.title, i.created_by
		FROM claims c JOIN items i ON i.id = c.item_id
		ORDER BY c.token ASC`)
	if err != nil {
		return server.Digest{}, mapErr("digest claims", err)
	}
	for claimRows.Next() {
		var (
			c       server.Claim
			created string
		)
		if err := claimRows.Scan(&c.Token, &c.ItemID, &c.ExpiresAt, &c.Title, &created); err != nil {
			claimRows.Close()
			return server.Digest{}, mapErr("digest claims", err)
		}
		if peer != "" && created != peer {
			continue
		}
		d.Claims = append(d.Claims, c)
	}
	claimRows.Close()
	if err := claimRows.Err(); err != nil {
		return server.Digest{}, mapErr("digest claims", err)
	}

	// Shared memory published since the caller's last turn.
	d.NewContext = []server.ContextEntry{}
	ctxRows, err := s.db.Query(`SELECT `+eventColumns+` FROM events WHERE ts >= ? ORDER BY id ASC`, since)
	if err != nil {
		return server.Digest{}, mapErr("digest context", err)
	}
	for ctxRows.Next() {
		e, err := scanEvent(ctxRows)
		if err != nil {
			ctxRows.Close()
			return server.Digest{}, mapErr("digest context", err)
		}
		d.NewContext = append(d.NewContext, e)
	}
	ctxRows.Close()
	if err := ctxRows.Err(); err != nil {
		return server.Digest{}, mapErr("digest context", err)
	}

	// The caller's messages.
	msgs, err := s.Messages(peer, since)
	if err != nil {
		return server.Digest{}, err
	}
	d.Messages = msgs

	return d, nil
}
