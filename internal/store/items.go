package store

import (
	"database/sql"
	"errors"
	"regexp"
	"strings"

	"swarm-go/internal/server"
)

// resourceRE pulls resource tokens (path, optionally "#symbol") out of a
// title. It is the single source of truth shared by the item dedup key and
// the stigmergy conflict scan, so both agree on granularity.
//
// The optional "#symbol" suffix is what makes symbol-level tasks possible:
// "implement src/util/compact.js#compress" and
// "test src/util/compact.js#compress" are two items that BOTH touch the same
// symbol (so they conflict), while "#compress" and "#decompress" variants do
// NOT conflict — and, crucially, are no longer folded into one item by dedup.
//
// The regex is deliberately loose; looksLikeResource decides whether a
// candidate is a real path. A bare word.ext shape is NOT enough: prose like
// "error.message" otherwise becomes a "resource", and a real run split one
// item into "error.messag", "error.msg", "error.error", "error.detail" —
// four junk items that burned a worker each.
var resourceRE = regexp.MustCompile(`(?:[A-Za-z0-9_.@-]+/)*[A-Za-z0-9_@.-]+\.[A-Za-z][A-Za-z0-9]{0,6}(?:#[A-Za-z_][A-Za-z0-9_]*)?`)

// knownExt lists file extensions accepted without requiring a slash in the
// token. Keeping this explicit is what stops "error.message" from becoming a
// resource: "message" is not a known extension, so the token is rejected.
var knownExt = map[string]bool{
	"js": true, "mjs": true, "cjs": true, "ts": true, "tsx": true, "jsx": true,
	"go": true, "py": true, "rs": true, "java": true, "kt": true, "rb": true,
	"php": true, "c": true, "h": true, "cc": true, "cpp": true, "hpp": true,
	"cs": true, "swift": true, "scala": true, "sh": true, "bash": true, "ps1": true,
	"json": true, "yaml": true, "yml": true, "toml": true, "ini": true, "env": true,
	"md": true, "txt": true, "rst": true, "html": true, "htm": true, "css": true,
	"scss": true, "less": true, "vue": true, "svelte": true, "xml": true, "svg": true,
	"sql": true, "db": true, "csv": true, "tsv": true,
	"png": true, "jpg": true, "jpeg": true, "gif": true, "webp": true, "ico": true,
	"pdf": true, "zip": true, "tar": true, "gz": true, "lock": true, "log": true,
	"test": true, "spec": true, "mod": true, "sum": true, "work": true,
}

// looksLikeResource applies the path heuristic to one candidate token.
func looksLikeResource(tok string) bool {
	t := strings.TrimSpace(strings.ReplaceAll(tok, `\`, "/"))
	if t == "" {
		return false
	}
	dot := strings.LastIndex(t, ".")
	if dot <= 0 || dot == len(t)-1 {
		return false
	}
	// A slash anywhere means it is a path: "src/util.js", "./a.js", "a/b/c.d".
	if strings.Contains(t, "/") {
		return true
	}
	// Otherwise the extension must be a known file extension.
	ext := strings.ToLower(t[dot+1:])
	if h := strings.IndexByte(ext, '#'); h >= 0 {
		ext = ext[:h]
	}
	return knownExt[ext]
}

// artifactKey returns the normalized dedup key for a title: "f:<resource>"
// (resource = path or path#symbol, lowercased, sorted+joined), else "".
func artifactKey(title string) string {
	hits := resourceRE.FindAllString(title, -1)
	if len(hits) == 0 {
		return ""
	}
	seen := map[string]bool{}
	var uniq []string
	for _, h := range hits {
		if !looksLikeResource(h) {
			continue
		}
		n := strings.ToLower(strings.TrimPrefix(strings.ReplaceAll(h, `\`, "/"), "./"))
		n = strings.TrimRight(n, ".,;:)")
		if !seen[n] {
			seen[n] = true
			uniq = append(uniq, n)
		}
	}
	if len(uniq) == 0 {
		return ""
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

// SeedProgress counts the SEED items (created_by = "seed") by terminal state.
//
// Used by --exit-when-done: the operator's own items are what "done" means.
// Worker-created items and auto-split children are excluded, so a swarm that
// keeps exploring after finishing its assignment still reports completion
// promptly instead of never exiting.
func (s *Store) SeedProgress() (open, total int, err error) {
	rows, err := s.db.Query(`SELECT status FROM items WHERE created_by = 'seed'`)
	if err != nil {
		return 0, 0, mapErr("seed progress", err)
	}
	defer rows.Close()
	for rows.Next() {
		var status string
		if serr := rows.Scan(&status); serr != nil {
			return 0, 0, mapErr("seed progress scan", serr)
		}
		total++
		switch status {
		case StatusDone, StatusFailed:
			// terminal
		default:
			open++
		}
	}
	return open, total, rows.Err()
}

// TerminalSeedItems returns the seed items that reached done/failed, for the
// run report. Ordered by id so the report is stable across calls.
func (s *Store) TerminalSeedItems() ([]server.Item, error) {
	rows, err := s.db.Query(
		`SELECT ` + itemColumns + ` FROM items
		 WHERE created_by = 'seed' AND status IN ('done','failed')
		 ORDER BY id ASC`)
	if err != nil {
		return nil, mapErr("terminal seeds", err)
	}
	defer rows.Close()
	out := []server.Item{}
	for rows.Next() {
		it, serr := scanItem(rows)
		if serr != nil {
			return nil, serr
		}
		out = append(out, it)
	}
	return out, rows.Err()
}

// OpenItemSummary lists what is still open (any creator), so the report can
// say "these were left running" instead of silently dropping them.
func (s *Store) OpenItemSummary() ([]server.Item, error) {
	rows, err := s.db.Query(
		`SELECT ` + itemColumns + ` FROM items
		 WHERE status NOT IN ('done','failed','split')
		 ORDER BY id ASC`)
	if err != nil {
		return nil, mapErr("open items", err)
	}
	defer rows.Close()
	out := []server.Item{}
	for rows.Next() {
		it, serr := scanItem(rows)
		if serr != nil {
			return nil, serr
		}
		out = append(out, it)
	}
	return out, rows.Err()
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
