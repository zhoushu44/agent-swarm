// Package server wires the swarm HTTP API routes to a Store.
//
// It mirrors the route surface of src/server.js:
//
//	GET  /board            ?status=&limit=&offset=
//	GET  /item             ?id=
//	GET  /digest           ?peer=&since=
//	GET  /context/search   ?q=&limit=
//	GET  /messages         ?peer=&since=
//	POST /item             create item   {peer,title,detail,parentId,kind,priority}
//	POST /claim            claim item    {itemId,peer}
//	POST /release          release claim {token}
//	POST /renew            renew claim   {token}
//	POST /submit           submit work   {token,peer,commitSha}
//	POST /context          publish note  {peer,kind,text,itemId,tags}
//	POST /message          send message  {from,to,text}
//
// The package deliberately depends on nothing outside the standard library
// (this host has no C compiler, so cgo-backed SQLite drivers cannot build).
// Persistence is expressed through the Store interface so internal/store can
// satisfy it via a small adapter without creating an import cycle.
package server

import (
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"time"
)

// Sentinel errors returned by a Store. The HTTP layer maps them to status codes.
var (
	ErrNotFound = errors.New("not found")
	ErrConflict = errors.New("conflict")
	ErrInvalid  = errors.New("invalid request")
)

// Item is a unit of work on the board.
type Item struct {
	ID        string  `json:"id"`
	ParentID  *string `json:"parentId"`
	Title     string  `json:"title"`
	Detail    string  `json:"detail"`
	Kind      string  `json:"kind"`
	Priority  int     `json:"priority"`
	Status    string  `json:"status"`
	CreatedBy string  `json:"createdBy"`
	CreatedAt int64   `json:"createdAt"`
	DoneAt    *int64  `json:"doneAt"`
	NotBefore *int64  `json:"notBefore"`
}

// ContextEntry is a shared-memory note (FACT / OBSERVED / FAIL / PATCH_SUMMARY ...).
type ContextEntry struct {
	ID     int64    `json:"id,omitempty"`
	Peer   string   `json:"peer"`
	Kind   string   `json:"kind"`
	Text   string   `json:"text"`
	ItemID *string  `json:"itemId,omitempty"`
	Tags   []string `json:"tags,omitempty"`
	TS     int64    `json:"ts"`
}

// Claim is an active lease held by a peer.
type Claim struct {
	Token     string `json:"token"`
	ItemID    string `json:"itemId"`
	Title     string `json:"title"`
	ExpiresAt int64  `json:"expiresAt"`
}

// Message is a peer-to-peer note.
type Message struct {
	ID   int64  `json:"id,omitempty"`
	From string `json:"from"`
	To   string `json:"to"`
	Text string `json:"text"`
	TS   int64  `json:"ts"`
	Read bool   `json:"read"`
}

// Stats is the board census embedded in a digest.
type Stats struct {
	Total      int `json:"total"`
	Pending    int `json:"pending"`
	Submitted  int `json:"submitted"`
	Done       int `json:"done"`
	InProgress int `json:"inProgress"`
	Failed     int `json:"failed"`
}

// Digest is the single-call turn briefing returned by GET /digest.
type Digest struct {
	Now            int64          `json:"now"`
	Stats          Stats          `json:"stats"`
	PendingPreview []Item         `json:"pendingPreview"`
	Claims         []Claim        `json:"claims"`
	NewContext     []ContextEntry `json:"newContext"`
	Messages       []Message      `json:"messages"`
}

// ClaimResult is the task package handed back by POST /claim.
type ClaimResult struct {
	Token   string         `json:"token"`
	Item    Item           `json:"item"`
	History []ContextEntry `json:"history"`
}

// CreateItemInput describes a new item.
type CreateItemInput struct {
	Peer     string  `json:"peer"`
	Title    string  `json:"title"`
	Detail   string  `json:"detail"`
	ParentID *string `json:"parentId"`
	Kind     string  `json:"kind"`
	Priority int     `json:"priority"`
}

// Store is the persistence contract the HTTP layer needs.
//
// Implementations must return the sentinel errors above so the handlers can
// produce the right status code (404 / 409 / 400).
type Store interface {
	ListItems(status string, limit, offset int) (items []Item, total int, err error)
	GetItem(id string) (Item, error)
	CreateItem(in CreateItemInput) (Item, error)

	Claim(itemID, peer string) (ClaimResult, error)
	Release(token string) error
	Renew(token string) error
	Submit(token, peer, commitSha string) (Item, error)

	AddContext(entry ContextEntry) (ContextEntry, error)
	SearchContext(q string, limit int) ([]ContextEntry, error)
	ItemHistory(itemID string) ([]ContextEntry, error)

	Digest(peer string, since int64) (Digest, error)
	SendMessage(m Message) (Message, error)
	Messages(peer string, since int64) ([]Message, error)
}

// Server is an http.Handler exposing the swarm API.
type Server struct {
	store Store
	mux   *http.ServeMux
	now   func() int64
}

// New returns a Server backed by store.
func New(store Store) *Server {
	if store == nil {
		panic("server: nil store")
	}
	s := &Server{store: store, mux: http.NewServeMux(), now: func() int64 { return time.Now().UnixMilli() }}
	s.routes()
	return s
}

// SetClock overrides the clock (used by tests).
func (s *Server) SetClock(fn func() int64) {
	if fn != nil {
		s.now = fn
	}
}

func (s *Server) routes() {
	s.mux.HandleFunc("/board", s.only("GET", s.handleBoard))
	s.mux.HandleFunc("/item", s.only2("GET", s.handleGetItem, "POST", s.handleCreateItem))
	s.mux.HandleFunc("/digest", s.only("GET", s.handleDigest))
	s.mux.HandleFunc("/context", s.only("POST", s.handleContext))
	s.mux.HandleFunc("/context/search", s.only("GET", s.handleSearch))
	s.mux.HandleFunc("/messages", s.only("GET", s.handleMessages))
	s.mux.HandleFunc("/message", s.only("POST", s.handleMessage))
	s.mux.HandleFunc("/claim", s.only("POST", s.handleClaim))
	s.mux.HandleFunc("/release", s.only("POST", s.handleRelease))
	s.mux.HandleFunc("/renew", s.only("POST", s.handleRenew))
	s.mux.HandleFunc("/submit", s.only("POST", s.handleSubmit))
	s.mux.HandleFunc("/healthz", s.only("GET", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "now": s.now()})
	}))
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	s.mux.ServeHTTP(w, r)
}

// ---- method guards -------------------------------------------------------

type handlerFunc func(http.ResponseWriter, *http.Request)

func (s *Server) only(method string, h handlerFunc) handlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if r.Method != method {
			w.Header().Set("Allow", method)
			writeError(w, http.StatusMethodNotAllowed, "method not allowed")
			return
		}
		h(w, r)
	}
}

func (s *Server) only2(m1 string, h1 handlerFunc, m2 string, h2 handlerFunc) handlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case m1:
			h1(w, r)
		case m2:
			h2(w, r)
		default:
			w.Header().Set("Allow", m1+", "+m2)
			writeError(w, http.StatusMethodNotAllowed, "method not allowed")
		}
	}
}

// ---- response helpers ----------------------------------------------------

func writeJSON(w http.ResponseWriter, status int, body any) {
	buf, err := json.Marshal(body)
	if err != nil {
		http.Error(w, "encode error", http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write(buf)
}

func writeError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]any{"error": msg})
}

// writeStoreError maps store sentinel errors onto HTTP status codes.
func writeStoreError(w http.ResponseWriter, err error) {
	switch {
	case errors.Is(err, ErrNotFound):
		writeError(w, http.StatusNotFound, err.Error())
	case errors.Is(err, ErrConflict):
		writeError(w, http.StatusConflict, err.Error())
	case errors.Is(err, ErrInvalid):
		writeError(w, http.StatusBadRequest, err.Error())
	default:
		writeError(w, http.StatusInternalServerError, err.Error())
	}
}

// decode reads a JSON body into dst, replying 400 on malformed input.
func decode(w http.ResponseWriter, r *http.Request, dst any) bool {
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20))
	dec.DisallowUnknownFields()
	if err := dec.Decode(dst); err != nil {
		writeError(w, http.StatusBadRequest, "invalid JSON body: "+err.Error())
		return false
	}
	return true
}

func atoiDefault(s string, def int) int {
	if s == "" {
		return def
	}
	n, err := strconv.Atoi(s)
	if err != nil {
		return def
	}
	return n
}
