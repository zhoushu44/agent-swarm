// Package store extends the SQLite-backed store with submission resolution,
// autoscaler counters, and item lifecycle helpers needed by the runner.
package store

import (
	"database/sql"
	"errors"
	"fmt"
	"strings"

	"swarm-go/internal/server"
)

// SubmissionState is the lifecycle of a queued submission.
type SubmissionState string

const (
	SubmissionQueued   SubmissionState = "queued"
	SubmissionMerged   SubmissionState = "merged"
	SubmissionRejected SubmissionState = "rejected"
)

// Submission is one queued integration request.
type Submission struct {
	ID        int64
	ItemID    string
	Peer      string
	CommitSHA string
	CreatedAt int64
	State     SubmissionState
	Reason    string
}

// migrateSubmissions adds the state column if missing (idempotent).
func (s *Store) migrateSubmissions() error {
	_, err := s.db.Exec(`ALTER TABLE submissions ADD COLUMN state TEXT NOT NULL DEFAULT 'queued'`)
	if err != nil && !strings.Contains(err.Error(), "duplicate column name") {
		return fmt.Errorf("store: migrate submissions state: %w", err)
	}
	_, err = s.db.Exec(`ALTER TABLE submissions ADD COLUMN reason TEXT NOT NULL DEFAULT ''`)
	if err != nil && !strings.Contains(err.Error(), "duplicate column name") {
		return fmt.Errorf("store: migrate submissions reason: %w", err)
	}
	_, err = s.db.Exec(`CREATE INDEX IF NOT EXISTS submissions_state_idx ON submissions(state)`)
	if err != nil {
		return fmt.Errorf("store: migrate submissions state idx: %w", err)
	}
	return nil
}

// QueuedSubmissions returns all submissions waiting for integration, oldest first.
func (s *Store) QueuedSubmissions() ([]Submission, error) {
	if err := s.migrateSubmissions(); err != nil {
		return nil, err
	}
	rows, err := s.db.Query(`SELECT id, item_id, peer, commit_sha, created_at, state, reason
		FROM submissions WHERE state = ? ORDER BY created_at ASC`, SubmissionQueued)
	if err != nil {
		return nil, mapErr("queued submissions", err)
	}
	defer rows.Close()
	var out []Submission
	for rows.Next() {
		var sub Submission
		if err := rows.Scan(&sub.ID, &sub.ItemID, &sub.Peer, &sub.CommitSHA, &sub.CreatedAt, &sub.State, &sub.Reason); err != nil {
			return nil, mapErr("queued submissions scan", err)
		}
		out = append(out, sub)
	}
	return out, rows.Err()
}

// HasQueuedSubmissionForPeer reports whether peer still has a queued submission.
func (s *Store) HasQueuedSubmissionForPeer(peer string) (bool, error) {
	if err := s.migrateSubmissions(); err != nil {
		return false, err
	}
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM submissions WHERE peer = ? AND state = ?`,
		strings.TrimSpace(peer), SubmissionQueued).Scan(&n)
	if err != nil {
		return false, mapErr("has queued submission", err)
	}
	return n > 0, nil
}

// ResolveSubmission marks a submission merged or rejected and updates the item.
// ok=true → item done (cascade parent). ok=false → item back to pending with
// exponential retry backoff; after maxItemFails rejections it is marked failed.
func (s *Store) ResolveSubmission(subID int64, ok bool, reason string, maxItemFails int) error {
	if err := s.migrateSubmissions(); err != nil {
		return err
	}
	tx, err := s.db.Begin()
	if err != nil {
		return mapErr("resolve submission", err)
	}
	defer tx.Rollback()

	var sub Submission
	var itemStatus string
	err = tx.QueryRow(`SELECT s.item_id, s.peer, s.commit_sha, s.state, i.status
		FROM submissions s JOIN items i ON i.id = s.item_id WHERE s.id = ?`, subID).
		Scan(&sub.ItemID, &sub.Peer, &sub.CommitSHA, &sub.State, &itemStatus)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return notFound("resolve submission", fmt.Sprint(subID))
		}
		return mapErr("resolve submission", err)
	}
	if sub.State != SubmissionQueued {
		return nil // already resolved
	}

	state := SubmissionMerged
	if !ok {
		state = SubmissionRejected
	}
	if _, err := tx.Exec(`UPDATE submissions SET state = ?, reason = ? WHERE id = ?`, state, reason, subID); err != nil {
		return mapErr("resolve submission", err)
	}

	if ok {
		now := s.clock()
		if _, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`,
			StatusDone, now, sub.ItemID); err != nil {
			return mapErr("resolve submission done", err)
		}
		// cascade parent: if all siblings are done/failed, parent settles too.
		if err := s.cascadeParentTx(tx, sub.ItemID); err != nil {
			return err
		}
		return tx.Commit()
	}

	// rejected: count prior rejections for this item
	var fails int
	if err := tx.QueryRow(`SELECT COUNT(*) FROM submissions WHERE item_id = ? AND state = ?`,
		sub.ItemID, SubmissionRejected).Scan(&fails); err != nil {
		return mapErr("resolve submission count fails", err)
	}
	if maxItemFails <= 0 {
		maxItemFails = 3
	}
	if fails >= maxItemFails {
		// poison item: terminal failed
		if _, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`,
			StatusFailed, s.clock(), sub.ItemID); err != nil {
			return mapErr("resolve submission fail item", err)
		}
		if err := s.cascadeParentTx(tx, sub.ItemID); err != nil {
			return err
		}
	} else if itemStatus == StatusSubmitted {
		// exponential backoff before the item becomes claimable again
		backoffMs := int64(5000) << uint(min(fails, 4))
		if backoffMs > 60000 {
			backoffMs = 60000
		}
		notBefore := s.clock() + backoffMs
		if _, err := tx.Exec(`UPDATE items SET status = ?, not_before = ? WHERE id = ?`,
			StatusPending, notBefore, sub.ItemID); err != nil {
			return mapErr("resolve submission retry", err)
		}
	}
	return tx.Commit()
}

// cascadeParentTx settles a parent when all its children are done/failed.
func (s *Store) cascadeParentTx(tx *sql.Tx, itemID string) error {
	var parentID sql.NullString
	if err := tx.QueryRow(`SELECT parent_id FROM items WHERE id = ?`, itemID).Scan(&parentID); err != nil {
		return nil
	}
	if !parentID.Valid || parentID.String == "" {
		return nil
	}
	rows, err := tx.Query(`SELECT status FROM items WHERE parent_id = ?`, parentID.String)
	if err != nil {
		return mapErr("cascade parent", err)
	}
	var statuses []string
	for rows.Next() {
		var st string
		if err := rows.Scan(&st); err != nil {
			rows.Close()
			return mapErr("cascade parent scan", err)
		}
		statuses = append(statuses, st)
	}
	rows.Close()
	if len(statuses) == 0 {
		return nil
	}
	settled, allDone := true, true
	for _, st := range statuses {
		if st != StatusDone && st != StatusFailed {
			settled = false
			break
		}
		if st != StatusDone {
			allDone = false
		}
	}
	if !settled {
		return nil
	}
	want := StatusDone
	if !allDone {
		want = StatusFailed
	}
	var cur string
	if err := tx.QueryRow(`SELECT status FROM items WHERE id = ?`, parentID.String).Scan(&cur); err != nil {
		return nil
	}
	if cur == want {
		return nil
	}
	now := s.clock()
	if _, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`, want, now, parentID.String); err != nil {
		return mapErr("cascade parent update", err)
	}
	return s.cascadeParentTx(tx, parentID.String)
}

// OpenCount returns items that still need work (pending + in_progress + submitted).
func (s *Store) OpenCount() (int, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM items WHERE status IN (?, ?, ?)`,
		StatusPending, StatusInProgress, StatusSubmitted).Scan(&n)
	return n, mapErr("open count", err)
}

// ClaimableCount returns pending items that can be claimed right now.
// Excludes container items (those with children), items in retry backoff,
// and items already under an active claim.
func (s *Store) ClaimableCount() (int, error) {
	now := s.clock()
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM items i
		WHERE i.status = ?
		  AND (i.not_before IS NULL OR i.not_before <= ?)
		  AND NOT EXISTS (SELECT 1 FROM items c WHERE c.parent_id = i.id)
		  AND NOT EXISTS (SELECT 1 FROM claims cl WHERE cl.item_id = i.id AND cl.expires_at > ?)`,
		StatusPending, now, now).Scan(&n)
	return n, mapErr("claimable count", err)
}

// RecentSplitCount returns items created in the last windowMs that have a parent.
func (s *Store) RecentSplitCount(windowMs int64) (int, error) {
	if windowMs <= 0 {
		windowMs = 300000
	}
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM items WHERE parent_id IS NOT NULL AND created_at > ?`,
		s.clock()-windowMs).Scan(&n)
	return n, mapErr("recent split count", err)
}

// ItemCount returns the total number of items.
func (s *Store) ItemCount() (int, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM items`).Scan(&n)
	return n, mapErr("item count", err)
}

// ItemDepth returns the depth of itemID in the split tree (root = 0).
func (s *Store) ItemDepth(itemID string) (int, error) {
	depth := 0
	cur := strings.TrimSpace(itemID)
	for cur != "" {
		var parent sql.NullString
		err := s.db.QueryRow(`SELECT parent_id FROM items WHERE id = ?`, cur).Scan(&parent)
		if err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				break
			}
			return depth, mapErr("item depth", err)
		}
		if !parent.Valid || parent.String == "" {
			break
		}
		depth++
		cur = parent.String
		if depth > 32 {
			break // cycle guard
		}
	}
	return depth, nil
}

// ActiveClaimsByPeer returns the live claims held by peer.
func (s *Store) ActiveClaimsByPeer(peer string) ([]server.Claim, error) {
	now := s.clock()
	rows, err := s.db.Query(`SELECT c.token, c.item_id, c.expires_at, i.title
		FROM claims c JOIN items i ON i.id = c.item_id
		WHERE c.peer = ? AND c.expires_at > ?`, strings.TrimSpace(peer), now)
	if err != nil {
		return nil, mapErr("active claims by peer", err)
	}
	defer rows.Close()
	var out []server.Claim
	for rows.Next() {
		var c server.Claim
		if err := rows.Scan(&c.Token, &c.ItemID, &c.ExpiresAt, &c.Title); err != nil {
			return nil, mapErr("active claims scan", err)
		}
		out = append(out, c)
	}
	return out, rows.Err()
}

// FailAllOpenItems marks every open item failed (used on budget exhaustion).
func (s *Store) FailAllOpenItems(peer, reason string) (int, error) {
	tx, err := s.db.Begin()
	if err != nil {
		return 0, mapErr("fail all open", err)
	}
	defer tx.Rollback()
	rows, err := tx.Query(`SELECT id FROM items WHERE status IN (?, ?, ?)`,
		StatusPending, StatusInProgress, StatusSubmitted)
	if err != nil {
		return 0, mapErr("fail all open", err)
	}
	var ids []string
	for rows.Next() {
		var id string
		if err := rows.Scan(&id); err != nil {
			rows.Close()
			return 0, mapErr("fail all open scan", err)
		}
		ids = append(ids, id)
	}
	rows.Close()
	now := s.clock()
	for _, id := range ids {
		if _, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`,
			StatusFailed, now, id); err != nil {
			return 0, mapErr("fail all open update", err)
		}
	}
	if err := tx.Commit(); err != nil {
		return 0, mapErr("fail all open commit", err)
	}
	return len(ids), nil
}

// Stats returns a board census.
func (s *Store) Stats() (server.Stats, error) {
	var st server.Stats
	rows, err := s.db.Query(`SELECT status, COUNT(*) FROM items GROUP BY status`)
	if err != nil {
		return st, mapErr("stats", err)
	}
	defer rows.Close()
	for rows.Next() {
		var status string
		var n int
		if err := rows.Scan(&status, &n); err != nil {
			return st, mapErr("stats scan", err)
		}
		st.Total += n
		switch status {
		case StatusPending:
			st.Pending = n
		case StatusInProgress:
			st.InProgress = n
		case StatusSubmitted:
			st.Submitted = n
		case StatusDone:
			st.Done = n
		case StatusFailed:
			st.Failed = n
		}
	}
	return st, rows.Err()
}

// SetStatus updates an item's status (internal helper for auto-split).
func (s *Store) SetStatus(itemID, status string) error {
	_, err := s.db.Exec(`UPDATE items SET status = ? WHERE id = ?`, status, strings.TrimSpace(itemID))
	return mapErr("set status", err)
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
