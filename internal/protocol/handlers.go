package protocol

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
)

// maxBody caps request bodies so a runaway client cannot exhaust memory.
const maxBody = 8 << 20 // 8 MiB

// ------------------------------------------------------------------ helpers

// reply writes v as JSON with the given status. Marshalling happens before any
// header is sent, so a marshal error still produces a clean 500.
func reply(w http.ResponseWriter, status int, v any) {
	body, err := json.Marshal(v)
	if err != nil {
		w.Header().Set("Content-Type", "application/json; charset=utf-8")
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = w.Write([]byte(`{"error":"internal marshal error"}`))
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write(body)
}

// fail writes a plain {"error": "..."} body.
func fail(w http.ResponseWriter, status int, format string, args ...any) {
	reply(w, status, map[string]string{"error": fmt.Sprintf(format, args...)})
}

// backendError maps a Backend error onto an HTTP status.
func backendError(w http.ResponseWriter, err error) {
	switch {
	case err == nil:
		return
	case errors.Is(err, ErrBadRequest):
		fail(w, http.StatusBadRequest, "%s", err.Error())
	case errors.Is(err, ErrConflict):
		fail(w, http.StatusConflict, "%s", err.Error())
	case errors.Is(err, ErrNotFound):
		fail(w, http.StatusNotFound, "%s", err.Error())
	case errors.Is(err, ErrForbidden):
		fail(w, http.StatusForbidden, "%s", err.Error())
	default:
		fail(w, http.StatusInternalServerError, "%s", err.Error())
	}
}

// requireMethod answers 405 (with Allow) unless r uses the given method.
func requireMethod(w http.ResponseWriter, r *http.Request, method string) bool {
	if r.Method == method || (method == http.MethodGet && r.Method == http.MethodHead) {
		return true
	}
	w.Header().Set("Allow", method)
	fail(w, http.StatusMethodNotAllowed, "method %s not allowed", r.Method)
	return false
}

// decodeJSON reads exactly one JSON value from the request body into dst.
func decodeJSON(r *http.Request, dst any) error {
	if r.Body == nil {
		return fmt.Errorf("%w: empty body", ErrBadRequest)
	}
	dec := json.NewDecoder(io.LimitReader(r.Body, maxBody))
	if err := dec.Decode(dst); err != nil {
		if errors.Is(err, io.EOF) {
			return fmt.Errorf("%w: empty body", ErrBadRequest)
		}
		return fmt.Errorf("%w: invalid JSON: %v", ErrBadRequest, err)
	}
	return nil
}

// intQuery parses an optional integer query parameter.
func intQuery(s string, def int) int {
	if strings.TrimSpace(s) == "" {
		return def
	}
	n, err := strconv.Atoi(strings.TrimSpace(s))
	if err != nil {
		return def
	}
	return n
}

// int64Query parses an optional int64 query parameter.
func int64Query(s string, def int64) int64 {
	if strings.TrimSpace(s) == "" {
		return def
	}
	n, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
	if err != nil {
		return def
	}
	return n
}

// requireQuery fails the request when a mandatory query parameter is absent.
func requireQuery(w http.ResponseWriter, r *http.Request, names ...string) bool {
	for _, n := range names {
		if strings.TrimSpace(r.URL.Query().Get(n)) == "" {
			fail(w, http.StatusBadRequest, "missing required query parameter %q", n)
			return false
		}
	}
	return true
}

// -------------------------------------------------------------------- /board

func (h *Handler) handleBoard(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodGet) {
		return
	}
	q := r.URL.Query()
	limit := intQuery(q.Get("limit"), 200)
	if limit <= 0 || limit > 1000 {
		limit = 200
	}
	offset := intQuery(q.Get("offset"), 0)
	if offset < 0 {
		offset = 0
	}
	page, err := h.Backend.Board(strings.TrimSpace(q.Get("status")), limit, offset)
	if err != nil {
		backendError(w, err)
		return
	}
	if page.Items == nil {
		page.Items = []*Item{}
	}
	page.Limit = limit
	page.Offset = offset
	reply(w, http.StatusOK, page)
}

// --------------------------------------------------------------------- /item

func (h *Handler) handleItem(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet, http.MethodHead:
		if !requireQuery(w, r, "id") {
			return
		}
		item, history, err := h.Backend.Item(strings.TrimSpace(r.URL.Query().Get("id")))
		if err != nil {
			backendError(w, err)
			return
		}
		if history == nil {
			history = []*ContextEntry{}
		}
		reply(w, http.StatusOK, map[string]any{"item": item, "history": history})
	case http.MethodPost:
		var req CreateItemRequest
		if err := decodeJSON(r, &req); err != nil {
			backendError(w, err)
			return
		}
		req.Title = strings.TrimSpace(req.Title)
		if req.Peer == "" {
			backendError(w, fmt.Errorf("%w: peer is required", ErrBadRequest))
			return
		}
		if req.Title == "" {
			backendError(w, fmt.Errorf("%w: title is required", ErrBadRequest))
			return
		}
		if req.Kind == "" {
			req.Kind = "task"
		}
		item, err := h.Backend.CreateItem(req)
		if err != nil {
			backendError(w, err)
			return
		}
		reply(w, http.StatusOK, item)
	default:
		w.Header().Set("Allow", "GET, POST")
		fail(w, http.StatusMethodNotAllowed, "method %s not allowed", r.Method)
	}
}

// -------------------------------------------------------------------- /claim

func (h *Handler) handleClaim(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req struct {
		ItemID string `json:"itemId"`
		Peer   string `json:"peer"`
	}
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	req.ItemID = strings.TrimSpace(req.ItemID)
	if req.ItemID == "" || req.Peer == "" {
		backendError(w, fmt.Errorf("%w: itemId and peer are required", ErrBadRequest))
		return
	}
	claim, item, history, err := h.Backend.Claim(req.ItemID, req.Peer)
	if err != nil {
		backendError(w, err)
		return
	}
	if history == nil {
		history = []*ContextEntry{}
	}
	reply(w, http.StatusOK, map[string]any{
		"token":   claim.Token,
		"item":    item,
		"history": history,
	})
}

// ------------------------------------------------------------------- /submit

func (h *Handler) handleSubmit(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req struct {
		Token     string `json:"token"`
		Peer      string `json:"peer"`
		CommitSha string `json:"commitSha"`
	}
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	req.Token = strings.TrimSpace(req.Token)
	req.CommitSha = strings.TrimSpace(req.CommitSha)
	if req.Token == "" || req.Peer == "" || req.CommitSha == "" {
		backendError(w, fmt.Errorf("%w: token, peer and commitSha are required", ErrBadRequest))
		return
	}
	item, err := h.Backend.Submit(req.Token, req.Peer, req.CommitSha)
	if err != nil {
		backendError(w, err)
		return
	}
	reply(w, http.StatusOK, map[string]any{"ok": true, "item": item})
}

// ------------------------------------------------------------------ /release

func (h *Handler) handleRelease(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req struct {
		Token string `json:"token"`
		Peer  string `json:"peer"`
	}
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	if strings.TrimSpace(req.Token) == "" {
		backendError(w, fmt.Errorf("%w: token is required", ErrBadRequest))
		return
	}
	if err := h.Backend.Release(strings.TrimSpace(req.Token), req.Peer); err != nil {
		backendError(w, err)
		return
	}
	reply(w, http.StatusOK, map[string]any{"ok": true})
}

// -------------------------------------------------------------------- /renew

func (h *Handler) handleRenew(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req struct {
		Token string `json:"token"`
		Peer  string `json:"peer"`
	}
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	if strings.TrimSpace(req.Token) == "" {
		backendError(w, fmt.Errorf("%w: token is required", ErrBadRequest))
		return
	}
	expiresAt, err := h.Backend.Renew(strings.TrimSpace(req.Token), req.Peer)
	if err != nil {
		backendError(w, err)
		return
	}
	reply(w, http.StatusOK, map[string]any{"ok": true, "expiresAt": expiresAt})
}

// ------------------------------------------------------------------ /context

func (h *Handler) handleContext(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req AddContextRequest
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	req.Kind = strings.ToUpper(strings.TrimSpace(req.Kind))
	req.Text = strings.TrimSpace(req.Text)
	if req.Peer == "" {
		backendError(w, fmt.Errorf("%w: peer is required", ErrBadRequest))
		return
	}
	if !ValidKind(req.Kind) {
		backendError(w, fmt.Errorf("%w: kind must be one of OBSERVED, FACT, FAIL", ErrBadRequest))
		return
	}
	if req.Text == "" {
		backendError(w, fmt.Errorf("%w: text is required", ErrBadRequest))
		return
	}
	entry, err := h.Backend.AddContext(req)
	if err != nil {
		backendError(w, err)
		return
	}
	reply(w, http.StatusOK, map[string]any{"ok": true, "entry": entry})
}

func (h *Handler) handleContextSearch(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodGet) {
		return
	}
	if !requireQuery(w, r, "q") {
		return
	}
	entries, err := h.Backend.SearchContext(strings.TrimSpace(r.URL.Query().Get("q")))
	if err != nil {
		backendError(w, err)
		return
	}
	if entries == nil {
		entries = []*ContextEntry{}
	}
	reply(w, http.StatusOK, map[string]any{"entries": entries})
}

// ------------------------------------------------------------------ /message

func (h *Handler) handleMessage(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodPost) {
		return
	}
	var req struct {
		From string `json:"from"`
		To   string `json:"to"`
		Text string `json:"text"`
	}
	if err := decodeJSON(r, &req); err != nil {
		backendError(w, err)
		return
	}
	req.Text = strings.TrimSpace(req.Text)
	if req.From == "" || req.To == "" || req.Text == "" {
		backendError(w, fmt.Errorf("%w: from, to and text are required", ErrBadRequest))
		return
	}
	msg, err := h.Backend.Message(req.From, req.To, req.Text)
	if err != nil {
		backendError(w, err)
		return
	}
	reply(w, http.StatusOK, map[string]any{"ok": true, "message": msg})
}

// ------------------------------------------------------------------- /digest

func (h *Handler) handleDigest(w http.ResponseWriter, r *http.Request) {
	if !requireMethod(w, r, http.MethodGet) {
		return
	}
	q := r.URL.Query()
	d, err := h.Backend.Digest(strings.TrimSpace(q.Get("peer")), int64Query(q.Get("since"), 0))
	if err != nil {
		backendError(w, err)
		return
	}
	if d.PendingPreview == nil {
		d.PendingPreview = []*Item{}
	}
	if d.Claims == nil {
		d.Claims = []*Claim{}
	}
	if d.NewContext == nil {
		d.NewContext = []*ContextEntry{}
	}
	if d.Messages == nil {
		d.Messages = []*Message{}
	}
	reply(w, http.StatusOK, d)
}
