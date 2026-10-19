package store

import (
	"database/sql"
	"errors"
	"strings"

	"swarm-go/internal/server"
)

// Claim takes the lease on a pending item and returns the task package
// (token + item + its history), mirroring POST /claim in src/server.js.
//
// It returns server.ErrConflict when the item is already claimed or is not
// pending, and server.ErrNotFound when the item does not exist.
func (s *Store) Claim(itemID, peer string) (server.ClaimResult, error) {
	itemID = strings.TrimSpace(itemID)
	peer = strings.TrimSpace(peer)
	if itemID == "" || peer == "" {
		return server.ClaimResult{}, invalid("claim", "itemId and peer required")
	}

	tx, err := s.db.Begin()
	if err != nil {
		return server.ClaimResult{}, mapErr("claim", err)
	}
	defer tx.Rollback()

	it, err := getItemTx(tx, itemID)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return server.ClaimResult{}, notFound("claim item", itemID)
		}
		return server.ClaimResult{}, mapErr("claim item", err)
	}
	// 只认 pending / in_progress：in_progress 放行进清理循环——过期租约会在
	// 下面被回收并把 item 退回 pending，若在门口就拒掉，租约过期后没人能再
	// claim，item 会永远卡在进行中（对齐 JS store.tryClaim 的语义）。
	if it.Status != StatusPending && it.Status != StatusInProgress {
		return server.ClaimResult{}, conflict("claim item", itemID, "is "+it.Status)
	}

	now := s.clock()
	rows, err := tx.Query(`SELECT token, expires_at FROM claims WHERE item_id = ?`, itemID)
	if err != nil {
		return server.ClaimResult{}, mapErr("claim item", err)
	}
	type priorClaim struct {
		token   string
		expires int64
	}
	var priors []priorClaim
	for rows.Next() {
		var c priorClaim
		if err := rows.Scan(&c.token, &c.expires); err != nil {
			rows.Close()
			return server.ClaimResult{}, mapErr("claim item", err)
		}
		priors = append(priors, c)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return server.ClaimResult{}, mapErr("claim item", err)
	}
	for _, c := range priors {
		if c.expires > now {
			return server.ClaimResult{}, conflict("claim item", itemID, "already claimed")
		}
		// 过期租约：回收；item 还停在 in_progress 则退回 pending，否则没人
		// 再 claim 它会永远卡住。
		if _, err := tx.Exec(`DELETE FROM claims WHERE token = ?`, c.token); err != nil {
			return server.ClaimResult{}, mapErr("claim item", err)
		}
		if it.Status == StatusInProgress {
			if _, err := tx.Exec(`UPDATE items SET status = ? WHERE id = ?`, StatusPending, itemID); err != nil {
				return server.ClaimResult{}, mapErr("claim item", err)
			}
			it.Status = StatusPending
		}
	}

	tokSeq, err := nextCounterTx(tx, "c")
	if err != nil {
		return server.ClaimResult{}, err
	}
	token := idFor("c", tokSeq)
	expires := s.clock() + s.leaseMillis()
	if _, err := tx.Exec(`INSERT INTO claims(token, item_id, peer, expires_at) VALUES(?,?,?,?)`,
		token, itemID, peer, expires); err != nil {
		return server.ClaimResult{}, mapErr("claim", err)
	}
	if _, err := tx.Exec(`UPDATE items SET status = ? WHERE id = ?`, StatusInProgress, itemID); err != nil {
		return server.ClaimResult{}, mapErr("claim", err)
	}

	history, err := historyTx(tx, itemID)
	if err != nil {
		return server.ClaimResult{}, err
	}
	if err := tx.Commit(); err != nil {
		return server.ClaimResult{}, mapErr("claim", err)
	}

	it.Status = StatusInProgress
	return server.ClaimResult{
		Token:   token,
		Item:    it,
		History: history,
	}, nil
}

// Release drops a claim and puts the item back on the board as pending.
func (s *Store) Release(token string) error {
	token = strings.TrimSpace(token)
	if token == "" {
		return invalid("release", "missing token")
	}
	tx, err := s.db.Begin()
	if err != nil {
		return mapErr("release", err)
	}
	defer tx.Rollback()

	var itemID string
	if err := tx.QueryRow(`SELECT item_id FROM claims WHERE token = ?`, token).Scan(&itemID); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return notFound("release claim", token)
		}
		return mapErr("release", err)
	}
	if _, err := tx.Exec(`DELETE FROM claims WHERE token = ?`, token); err != nil {
		return mapErr("release", err)
	}
	if _, err := tx.Exec(`UPDATE items SET status = ? WHERE id = ? AND status = ?`,
		StatusPending, itemID, StatusInProgress); err != nil {
		return mapErr("release", err)
	}
	return mapErr("release", tx.Commit())
}

// Renew extends a live claim's lease by one lease period.
func (s *Store) Renew(token string) error {
	token = strings.TrimSpace(token)
	if token == "" {
		return invalid("renew", "missing token")
	}
	expires := s.clock() + s.leaseMillis()
	res, err := s.db.Exec(`UPDATE claims SET expires_at = ? WHERE token = ?`, expires, token)
	if err != nil {
		return mapErr("renew", err)
	}
	n, err := res.RowsAffected()
	if err != nil {
		return mapErr("renew", err)
	}
	if n == 0 {
		return notFound("renew claim", token)
	}
	return nil
}

// Submit records a commit for the claimed item and moves it to "submitted".
// The integrator (not the worker) later flips it to done/failed.
func (s *Store) Submit(token, peer, commitSha string) (server.Item, error) {
	token = strings.TrimSpace(token)
	peer = strings.TrimSpace(peer)
	commitSha = strings.TrimSpace(commitSha)
	if token == "" {
		return server.Item{}, invalid("submit", "missing token")
	}
	if commitSha == "" {
		return server.Item{}, invalid("submit", "missing commitSha")
	}

	tx, err := s.db.Begin()
	if err != nil {
		return server.Item{}, mapErr("submit", err)
	}
	defer tx.Rollback()

	var itemID, claimPeer string
	if err := tx.QueryRow(`SELECT item_id, peer FROM claims WHERE token = ?`, token).Scan(&itemID, &claimPeer); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return server.Item{}, notFound("submit claim", token)
		}
		return server.Item{}, mapErr("submit", err)
	}
	if peer == "" {
		peer = claimPeer
	}

	now := s.clock()
	if _, err := tx.Exec(`INSERT INTO submissions(item_id, peer, commit_sha, created_at) VALUES(?,?,?,?)`,
		itemID, peer, commitSha, now); err != nil {
		return server.Item{}, mapErr("submit", err)
	}
	if _, err := tx.Exec(`DELETE FROM claims WHERE token = ?`, token); err != nil {
		return server.Item{}, mapErr("submit", err)
	}
	if _, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`,
		StatusSubmitted, now, itemID); err != nil {
		return server.Item{}, mapErr("submit", err)
	}
	it, err := getItemTx(tx, itemID)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return server.Item{}, notFound("submit item", itemID)
		}
		return server.Item{}, mapErr("submit", err)
	}
	if err := tx.Commit(); err != nil {
		return server.Item{}, mapErr("submit", err)
	}
	return it, nil
}

// ReapExpired releases claims whose lease has elapsed and returns their items
// to the pending pool. It returns the number of reaped claims.
func (s *Store) ReapExpired() (int, error) {
	now := s.clock()
	tx, err := s.db.Begin()
	if err != nil {
		return 0, mapErr("reap", err)
	}
	defer tx.Rollback()

	rows, err := tx.Query(`SELECT token, item_id FROM claims WHERE expires_at <= ?`, now)
	if err != nil {
		return 0, mapErr("reap", err)
	}
	type dead struct{ token, itemID string }
	var expired []dead
	for rows.Next() {
		var d dead
		if err := rows.Scan(&d.token, &d.itemID); err != nil {
			rows.Close()
			return 0, mapErr("reap", err)
		}
		expired = append(expired, d)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return 0, mapErr("reap", err)
	}

	for _, d := range expired {
		if _, err := tx.Exec(`DELETE FROM claims WHERE token = ?`, d.token); err != nil {
			return 0, mapErr("reap", err)
		}
		if _, err := tx.Exec(`UPDATE items SET status = ? WHERE id = ? AND status = ?`,
			StatusPending, d.itemID, StatusInProgress); err != nil {
			return 0, mapErr("reap", err)
		}
	}
	if err := tx.Commit(); err != nil {
		return 0, mapErr("reap", err)
	}
	return len(expired), nil
}

// ActiveClaim returns the live claim (if any) on an item.
func (s *Store) ActiveClaim(itemID string) (*server.Claim, error) {
	var c server.Claim
	err := s.db.QueryRow(`SELECT token, item_id, peer, expires_at FROM claims WHERE item_id = ?`,
		strings.TrimSpace(itemID)).Scan(&c.Token, &c.ItemID, new(string), &c.ExpiresAt)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, mapErr("active claim", err)
	}
	return &c, nil
}
