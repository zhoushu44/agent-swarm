// Package protocol implements the swarm HTTP API: the wire types and the route
// handlers for /board /item /claim /submit /context /context/search /release
// /renew /message /digest.
//
// The package is transport-only. All persistence and business rules are
// delegated to a Backend, which internal/store is expected to implement; that
// keeps the handlers testable with an in-memory fake and lets the HTTP layer
// stay a thin mirror of the original src/server.js routes.
package protocol

import (
	"errors"
	"net/http"
	"strings"
)

// Item lifecycle states.
const (
	StatusPending    = "pending"
	StatusInProgress = "in_progress"
	StatusSubmitted  = "submitted"
	StatusDone       = "done"
	StatusFailed     = "failed"
)

// Shared-memory entry kinds.
const (
	KindObserved = "OBSERVED"
	KindFact     = "FACT"
	KindFail     = "FAIL"
)

// Sentinel errors a Backend may return; the handlers map them to HTTP statuses.
var (
	ErrBadRequest = errors.New("bad request")
	ErrConflict   = errors.New("conflict")
	ErrNotFound   = errors.New("not found")
	ErrForbidden  = errors.New("forbidden")
)

// Item is one unit of work on the board.
type Item struct {
	ID        string  `json:"id"`
	ParentID  *string `json:"parent_id"`
	Title     string  `json:"title"`
	Detail    string  `json:"detail"`
	Kind      string  `json:"kind"`
	Priority  int     `json:"priority"`
	Status    string  `json:"status"`
	CreatedBy string  `json:"created_by"`
	CreatedAt int64   `json:"created_at"`
	DoneAt    *int64  `json:"done_at"`
	NotBefore *int64  `json:"not_before"`
}

// Claim is a lease held by a peer over an item.
type Claim struct {
	Token     string `json:"token"`
	ItemID    string `json:"itemId"`
	Peer      string `json:"peer"`
	Title     string `json:"title,omitempty"`
	ExpiresAt int64  `json:"expiresAt"`
}

// ContextEntry is a shared-memory note (FACT / OBSERVED / FAIL / PATCH_SUMMARY /
// CONFLICT). It doubles as an item history record.
type ContextEntry struct {
	Peer   string   `json:"peer"`
	Kind   string   `json:"kind"`
	ItemID *string  `json:"item_id"`
	Text   string   `json:"text"`
	Tags   []string `json:"tags,omitempty"`
	TS     int64    `json:"ts"`
}

// Message is a peer-to-peer note.
type Message struct {
	From string `json:"from"`
	To   string `json:"to"`
	Text string `json:"text"`
	TS   int64  `json:"ts"`
}

// Stats is the board summary embedded in a digest.
type Stats struct {
	Total      int `json:"total"`
	Pending    int `json:"pending"`
	Submitted  int `json:"submitted"`
	Done       int `json:"done"`
	InProgress int `json:"inProgress"`
	Failed     int `json:"failed"`
}

// BoardPage is the paginated result of GET /board.
type BoardPage struct {
	Items   []*Item `json:"items"`
	Total   int     `json:"total"`
	Limit   int     `json:"limit"`
	Offset  int     `json:"offset"`
	HasMore bool    `json:"hasMore"`
}

// Digest is the single-call turn briefing returned by GET /digest.
type Digest struct {
	Now            int64           `json:"now"`
	Stats          Stats           `json:"stats"`
	PendingPreview []*Item         `json:"pendingPreview"`
	Claims         []*Claim        `json:"claims"`
	NewContext     []*ContextEntry `json:"newContext"`
	Messages       []*Message      `json:"messages"`
}

// CreateItemRequest is the POST /item body.
type CreateItemRequest struct {
	Peer     string  `json:"peer"`
	Title    string  `json:"title"`
	Detail   string  `json:"detail"`
	ParentID *string `json:"parentId"`
	Kind     string  `json:"kind"`
	Priority int     `json:"priority"`
}

// AddContextRequest is the POST /context body.
type AddContextRequest struct {
	Peer   string   `json:"peer"`
	Kind   string   `json:"kind"`
	Text   string   `json:"text"`
	ItemID *string  `json:"itemId"`
	Tags   []string `json:"tags"`
}

// Backend is everything the HTTP layer needs from persistence. Implementations
// return the package sentinel errors (wrapped is fine) for non-2xx responses.
type Backend interface {
	Board(status string, limit, offset int) (BoardPage, error)
	Item(id string) (*Item, []*ContextEntry, error)
	CreateItem(req CreateItemRequest) (*Item, error)

	// Claim returns ErrConflict when the item is already taken or terminal.
	Claim(itemID, peer string) (*Claim, *Item, []*ContextEntry, error)
	Submit(token, peer, commitSha string) (*Item, error)
	Release(token, peer string) error
	Renew(token, peer string) (int64, error)

	AddContext(req AddContextRequest) (*ContextEntry, error)
	SearchContext(query string) ([]*ContextEntry, error)
	Message(from, to, text string) (*Message, error)
	Digest(peer string, since int64) (*Digest, error)
}

// Handler serves the swarm API on top of a Backend.
type Handler struct {
	Backend Backend
}

// New returns an http.Handler with every swarm route registered.
func New(b Backend) http.Handler {
	mux := http.NewServeMux()
	Register(mux, b)
	return mux
}

// Register wires the swarm routes onto an existing mux so a server can add its
// own endpoints (health, static, ...) around them.
func Register(mux *http.ServeMux, b Backend) {
	h := &Handler{Backend: b}
	mux.HandleFunc("/board", h.handleBoard)
	mux.HandleFunc("/item", h.handleItem)
	mux.HandleFunc("/claim", h.handleClaim)
	mux.HandleFunc("/submit", h.handleSubmit)
	mux.HandleFunc("/release", h.handleRelease)
	mux.HandleFunc("/renew", h.handleRenew)
	mux.HandleFunc("/context", h.handleContext)
	mux.HandleFunc("/context/search", h.handleContextSearch)
	mux.HandleFunc("/message", h.handleMessage)
	mux.HandleFunc("/digest", h.handleDigest)
}

// ValidKind reports whether k is a shared-memory kind accepted by /context.
func ValidKind(k string) bool {
	switch strings.ToUpper(strings.TrimSpace(k)) {
	case KindObserved, KindFact, KindFail:
		return true
	}
	return false
}
