package server

import (
	"errors"
	"net/http"
	"strconv"
	"strings"
)

// handleBoard serves GET /board?status=&limit=&offset=.
func (s *Server) handleBoard(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	limit := atoiDefault(q.Get("limit"), 200)
	if limit <= 0 || limit > 1000 {
		limit = 200
	}
	offset := atoiDefault(q.Get("offset"), 0)
	if offset < 0 {
		offset = 0
	}
	items, total, err := s.store.ListItems(strings.TrimSpace(q.Get("status")), limit, offset)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	if items == nil {
		items = []Item{}
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"items":   items,
		"total":   total,
		"limit":   limit,
		"offset":  offset,
		"hasMore": offset+len(items) < total,
	})
}

// handleGetItem serves GET /item?id=... including the item's history.
func (s *Server) handleGetItem(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimSpace(r.URL.Query().Get("id"))
	if id == "" {
		writeError(w, http.StatusBadRequest, "missing id")
		return
	}
	item, err := s.store.GetItem(id)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	history, err := s.store.ItemHistory(id)
	if err != nil && !errors.Is(err, ErrNotFound) {
		writeStoreError(w, err)
		return
	}
	if history == nil {
		history = []ContextEntry{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"item": item, "history": history})
}

// handleCreateItem serves POST /item.
func (s *Server) handleCreateItem(w http.ResponseWriter, r *http.Request) {
	var in CreateItemInput
	if !decode(w, r, &in) {
		return
	}
	in.Title = strings.TrimSpace(in.Title)
	if in.Title == "" {
		writeError(w, http.StatusBadRequest, "missing title")
		return
	}
	if in.Kind == "" {
		in.Kind = "task"
	}
	if in.ParentID != nil {
		p := strings.TrimSpace(*in.ParentID)
		if p == "" {
			in.ParentID = nil
		} else {
			in.ParentID = &p
		}
	}
	item, err := s.store.CreateItem(in)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, item)
}

// handleClaim serves POST /claim, returning the task package.
func (s *Server) handleClaim(w http.ResponseWriter, r *http.Request) {
	var in struct {
		ItemID string `json:"itemId"`
		Peer   string `json:"peer"`
	}
	if !decode(w, r, &in) {
		return
	}
	in.ItemID = strings.TrimSpace(in.ItemID)
	in.Peer = strings.TrimSpace(in.Peer)
	if in.ItemID == "" || in.Peer == "" {
		writeError(w, http.StatusBadRequest, "missing itemId or peer")
		return
	}
	res, err := s.store.Claim(in.ItemID, in.Peer)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	if res.History == nil {
		res.History = []ContextEntry{}
	}
	writeJSON(w, http.StatusOK, res)
}

// handleRelease serves POST /release {token}.
func (s *Server) handleRelease(w http.ResponseWriter, r *http.Request) {
	token, ok := s.tokenRequest(w, r)
	if !ok {
		return
	}
	if err := s.store.Release(token); err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

// handleRenew serves POST /renew {token}.
func (s *Server) handleRenew(w http.ResponseWriter, r *http.Request) {
	token, ok := s.tokenRequest(w, r)
	if !ok {
		return
	}
	if err := s.store.Renew(token); err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

// handleSubmit serves POST /submit {token,peer,commitSha}.
func (s *Server) handleSubmit(w http.ResponseWriter, r *http.Request) {
	var in struct {
		Token     string `json:"token"`
		Peer      string `json:"peer"`
		CommitSha string `json:"commitSha"`
	}
	if !decode(w, r, &in) {
		return
	}
	in.Token = strings.TrimSpace(in.Token)
	in.Peer = strings.TrimSpace(in.Peer)
	if in.Token == "" || in.Peer == "" {
		writeError(w, http.StatusBadRequest, "missing token or peer")
		return
	}
	item, err := s.store.Submit(in.Token, in.Peer, strings.TrimSpace(in.CommitSha))
	if err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "item": item})
}

// handleContext serves POST /context (publish shared knowledge).
func (s *Server) handleContext(w http.ResponseWriter, r *http.Request) {
	var in struct {
		Peer   string   `json:"peer"`
		Kind   string   `json:"kind"`
		Text   string   `json:"text"`
		ItemID *string  `json:"itemId"`
		Tags   []string `json:"tags"`
	}
	if !decode(w, r, &in) {
		return
	}
	if strings.TrimSpace(in.Text) == "" {
		writeError(w, http.StatusBadRequest, "missing text")
		return
	}
	if strings.TrimSpace(in.Kind) == "" {
		in.Kind = "OBSERVED"
	}
	entry, err := s.store.AddContext(ContextEntry{
		Peer:   strings.TrimSpace(in.Peer),
		Kind:   strings.ToUpper(strings.TrimSpace(in.Kind)),
		Text:   in.Text,
		ItemID: in.ItemID,
		Tags:   in.Tags,
		TS:     s.now(),
	})
	if err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, entry)
}

// handleSearch serves GET /context/search?q=&limit=.
func (s *Server) handleSearch(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	query := strings.TrimSpace(q.Get("q"))
	limit := atoiDefault(q.Get("limit"), 50)
	if limit <= 0 || limit > 500 {
		limit = 50
	}
	entries, err := s.store.SearchContext(query, limit)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	if entries == nil {
		entries = []ContextEntry{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"entries": entries, "count": len(entries)})
}

// handleDigest serves GET /digest?peer=&since=.
func (s *Server) handleDigest(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	peer := strings.TrimSpace(q.Get("peer"))
	since := int64(atoiDefault(q.Get("since"), 0))
	d, err := s.store.Digest(peer, since)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	if d.Now == 0 {
		d.Now = s.now()
	}
	if d.PendingPreview == nil {
		d.PendingPreview = []Item{}
	}
	if d.Claims == nil {
		d.Claims = []Claim{}
	}
	if d.NewContext == nil {
		d.NewContext = []ContextEntry{}
	}
	if d.Messages == nil {
		d.Messages = []Message{}
	}
	writeJSON(w, http.StatusOK, d)
}

// handleMessage serves POST /message {from,to,text}.
func (s *Server) handleMessage(w http.ResponseWriter, r *http.Request) {
	var in struct {
		From string `json:"from"`
		To   string `json:"to"`
		Text string `json:"text"`
	}
	if !decode(w, r, &in) {
		return
	}
	in.From = strings.TrimSpace(in.From)
	in.To = strings.TrimSpace(in.To)
	if in.To == "" || strings.TrimSpace(in.Text) == "" {
		writeError(w, http.StatusBadRequest, "missing to or text")
		return
	}
	msg, err := s.store.SendMessage(Message{From: in.From, To: in.To, Text: in.Text, TS: s.now()})
	if err != nil {
		writeStoreError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, msg)
}

// handleMessages serves GET /messages?peer=&since=.
func (s *Server) handleMessages(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	peer := strings.TrimSpace(q.Get("peer"))
	since := int64(atoiDefault(q.Get("since"), 0))
	msgs, err := s.store.Messages(peer, since)
	if err != nil {
		writeStoreError(w, err)
		return
	}
	if msgs == nil {
		msgs = []Message{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"messages": msgs, "count": len(msgs)})
}

// tokenRequest decodes a {"token":...} body and validates it is present.
func (s *Server) tokenRequest(w http.ResponseWriter, r *http.Request) (string, bool) {
	var in struct {
		Token string `json:"token"`
	}
	if !decode(w, r, &in) {
		return "", false
	}
	token := strings.TrimSpace(in.Token)
	if token == "" {
		writeError(w, http.StatusBadRequest, "missing token")
		return "", false
	}
	return token, true
}

// parseID is a small helper kept for symmetry with the JS implementation.
func parseID(s string) (int64, bool) {
	n, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
	return n, err == nil
}
