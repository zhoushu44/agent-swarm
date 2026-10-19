package store

import (
	"database/sql"
	"errors"
	"regexp"
	"strings"

	"swarm-go/internal/server"
)

// artifactRE pulls file-path tokens (anything with an extension) out of an
// item title. Mirrors the JS ARTIFACT_RE in store.js: two workers asked to
// "add src/util/compact.js" and "create src/util/compact.js with tests" are
// delivering the SAME artifact, but an exact-title dedup lets both through
// and they collide on add/add at merge time. Folding by the extracted file
// path collapses every phrasing of the same deliverable into one item.
var artifactRE = regexp.MustCompile(`[A-Za-z0-9_@.\/\\-]+\.[A-Za-z][A-Za-z0-9]{0,5}\b`)

// artifactKey returns the normalized dedup key for a title: "f:<path>" when
// the title names at least one file (paths lowercased, backslashes → slashes,
// leading ./ stripped, sorted+joined), else "" (caller falls back to exact
// title match).
func artifactKey(title string) string {
	hits := artifactRE.FindAllString(title, -1)
	if len(hits) == 0 {
		return ""
	}
	seen := map[string]bool{}
	var uniq []string
	for _, h := range hits {
		n := strings.ToLower(strings.TrimPrefix(strings.ReplaceAll(h, `\`, `/`), `./`))
		if !seen[n] {
			seen[n] = true
			uniq = append(uniq, n)
		}
	}
	return "f:" + strings.Join(uniq, "|")
}

// itemColumns is the canonical SELECT list for items, in the order scanItem
// expects them.
const itemColumns = `id, parent_id, title, detail, kind, priority, status,
	created_by, created_at, done_at, not_before`

// rowScanner is satisfied by both *sql.Row and *sql.Rows.
type rowScanner interface {
	Scan(dest ...any) error
}

// scanItem reads one item row.
func scanItem(row rowScanner) (server.Item, error) {
	var (
		it       server.Item
		parentID sql.NullString
		doneAt   sql.NullInt64
		notBef   sql.NullInt64
	)
	if err := row.Scan(&it.ID, &parentID, &it.Title, &it.Detail, &it.Kind,
		&it.Priority, &it.Status, &it.CreatedBy, &it.CreatedAt, &doneAt, &notBef); err != nil {
		return server.Item{}, err
	}
	if parentID.Valid {
		v := parentID.String
		it.ParentID = &v
	}
	if doneAt.Valid {
		v := doneAt.Int64
		it.DoneAt = &v
	}
	if notBef.Valid {
		v := notBef.Int64
		it.NotBefore = &v
	}
	return it, nil
}

// ListItems returns one page of items, optionally filtered by status.
//
// Ordering matches the JS store: priority desc, then creation order asc.
func (s *Store) ListItems(status string, limit, offset int) ([]server.Item, int, error) {
	status = strings.TrimSpace(status)
	if limit <= 0 {
		limit = 200
	}
	if offset < 0 {
		offset = 0
	}

	var total int
	var err error
	if status == "" {
		err = s.db.QueryRow(`SELECT COUNT(*) FROM items`).Scan(&total)
	} else {
		err = s.db.QueryRow(`SELECT COUNT(*) FROM items WHERE status = ?`, status).Scan(&total)
	}
	if err != nil {
		return nil, 0, mapErr("list items count", err)
	}

	q := `SELECT ` + itemColumns + ` FROM items`
	args := []any{}
	if status != "" {
		q += ` WHERE status = ?`
		args = append(args, status)
	}
	q += ` ORDER BY priority DESC, seq ASC, created_at ASC LIMIT ? OFFSET ?`
	args = append(args, limit, offset)

	rows, err := s.db.Query(q, args...)
	if err != nil {
		return nil, 0, mapErr("list items", err)
	}
	defer rows.Close()

	items := make([]server.Item, 0, limit)
	for rows.Next() {
		it, err := scanItem(rows)
		if err != nil {
			return nil, 0, mapErr("list items scan", err)
		}
		items = append(items, it)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, mapErr("list items", err)
	}
	return items, total, nil
}

// GetItem returns one item by id.
func (s *Store) GetItem(id string) (server.Item, error) {
	id = strings.TrimSpace(id)
	if id == "" {
		return server.Item{}, invalid("get item", "missing id")
	}
	row := s.db.QueryRow(`SELECT `+itemColumns+` FROM items WHERE id = ?`, id)
	it, err := scanItem(row)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return server.Item{}, notFound("get item", id)
		}
		return server.Item{}, mapErr("get item", err)
	}
	return it, nil
}

// CreateItem inserts a new pending item and returns it.
func (s *Store) CreateItem(in server.CreateItemInput) (server.Item, error) {
	title := strings.TrimSpace(in.Title)
	if title == "" {
		return server.Item{}, invalid("create item", "title required")
	}
	kind := strings.TrimSpace(in.Kind)
	if kind == "" {
		kind = "task"
	}

	var parent *string
	if in.ParentID != nil {
		p := strings.TrimSpace(*in.ParentID)
		if p != "" {
			parent = &p
		}
	}

	tx, err := s.db.Begin()
	if err != nil {
		return server.Item{}, mapErr("create item", err)
	}
	defer tx.Rollback()

	if parent != nil {
		var n int
		if err := tx.QueryRow(`SELECT COUNT(*) FROM items WHERE id = ?`, *parent).Scan(&n); err != nil {
			return server.Item{}, mapErr("create item parent", err)
		}
		if n == 0 {
			return server.Item{}, notFound("create item parent", *parent)
		}
	}

	// Dedup: worker-created items (CreateItem callers that pass a Peer are the
	// HTTP /item path — LLM workers) fold into an existing OPEN item instead of
	// adding a duplicate. Two levels, aligned with the JS dedupKey:
	//   1. f: key — title names a file path → fold against ANY open item naming
	//      the same path (catches "add X.js" vs "create X.js with tests").
	//   2. exact title — fallback for titles with no file path.
	// Seed callers (Peer="") bypass dedup so deliberate duplicate seeds work.
	if strings.TrimSpace(in.Peer) != "" {
		var dupID string
		if key := artifactKey(title); key != "" {
			// Global fold on the file key across all open items.
			rows, derr := tx.Query(`SELECT id, title FROM items WHERE status NOT IN ('done','failed','split')`)
			if derr != nil {
				return server.Item{}, mapErr("create item dedup scan", derr)
			}
			for rows.Next() {
				var id, t string
				if serr := rows.Scan(&id, &t); serr != nil {
					rows.Close()
					return server.Item{}, mapErr("create item dedup scan", serr)
				}
				if artifactKey(t) == key {
					dupID = id
					break
				}
			}
			rows.Close()
		} else {
			err = tx.QueryRow(`SELECT id FROM items WHERE title = ? AND status NOT IN ('done','failed','split') LIMIT 1`, title).Scan(&dupID)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return server.Item{}, mapErr("create item dedup", err)
			}
		}
		if dupID != "" {
			_ = tx.Rollback()
			return s.GetItem(dupID)
		}
	}

	idSeq, err := nextCounterTx(tx, "t")
	if err != nil {
		return server.Item{}, err
	}
	order, err := nextCounterTx(tx, "seq")
	if err != nil {
		return server.Item{}, err
	}

	id := idFor("t", idSeq)
	createdAt := s.clock()
	if _, err := tx.Exec(`INSERT INTO items(id, parent_id, title, detail, kind, priority, status,
		created_by, created_at, done_at, not_before, seq)
		VALUES(?,?,?,?,?,?,?,?,?,NULL,NULL,?)`,
		id, parent, title, in.Detail, kind, in.Priority, StatusPending,
		strings.TrimSpace(in.Peer), createdAt, order); err != nil {
		return server.Item{}, mapErr("create item", err)
	}
	if err := tx.Commit(); err != nil {
		return server.Item{}, mapErr("create item", err)
	}
	return s.GetItem(id)
}

// setItemStatus updates an item's status, stamping done_at for terminal states.
func setItemStatus(tx *sql.Tx, id, status string, doneAt *int64) error {
	_, err := tx.Exec(`UPDATE items SET status = ?, done_at = ? WHERE id = ?`, status, doneAt, id)
	if err != nil {
		return mapErr("update item status", err)
	}
	return nil
}
