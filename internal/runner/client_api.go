package runner

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"swarm-go/internal/protocol"
)

// Board lists items, optionally filtered by status. limit <= 0 means the
// server default; offset < 0 is treated as 0.
func (c *Client) Board(ctx context.Context, status string, limit, offset int) (*protocol.BoardPage, error) {
	if limit <= 0 {
		limit = DefaultBoardLimit
	}
	if offset < 0 {
		offset = 0
	}
	q := url.Values{}
	q.Set("limit", strconv.Itoa(limit))
	q.Set("offset", strconv.Itoa(offset))
	if s := strings.TrimSpace(status); s != "" {
		q.Set("status", s)
	}
	var page protocol.BoardPage
	if err := c.do(ctx, http.MethodGet, "/board", q, nil, &page); err != nil {
		return nil, err
	}
	if page.Items == nil {
		page.Items = []*protocol.Item{}
	}
	return &page, nil
}

// PendingBoard is the convenience form of Board used by the claim loop: the
// first page of pending items.
func (c *Client) PendingBoard(ctx context.Context) ([]*protocol.Item, error) {
	page, err := c.Board(ctx, protocol.StatusPending, DefaultBoardLimit, 0)
	if err != nil {
		return nil, err
	}
	return page.Items, nil
}

// Item fetches one item together with its history (past findings and
// failures). Use it before claiming to see what you would get into.
func (c *Client) Item(ctx context.Context, id string) (*protocol.Item, []*protocol.ContextEntry, error) {
	id = strings.TrimSpace(id)
	if id == "" {
		return nil, nil, fmt.Errorf("%w: item id is required", ErrBadRequest)
	}
	q := url.Values{}
	q.Set("id", id)
	var resp struct {
		Item    *protocol.Item           `json:"item"`
		History []*protocol.ContextEntry `json:"history"`
	}
	if err := c.do(ctx, http.MethodGet, "/item", q, nil, &resp); err != nil {
		return nil, nil, err
	}
	if resp.History == nil {
		resp.History = []*protocol.ContextEntry{}
	}
	return resp.Item, resp.History, nil
}

// CreateItem adds an item to the board, optionally under req.ParentID.
func (c *Client) CreateItem(ctx context.Context, req protocol.CreateItemRequest) (*protocol.Item, error) {
	if strings.TrimSpace(req.Peer) == "" {
		return nil, fmt.Errorf("%w: peer is required", ErrBadRequest)
	}
	if strings.TrimSpace(req.Title) == "" {
		return nil, fmt.Errorf("%w: title is required", ErrBadRequest)
	}
	if req.Kind == "" {
		req.Kind = "task"
	}
	var item protocol.Item
	if err := c.do(ctx, http.MethodPost, "/item", nil, req, &item); err != nil {
		return nil, err
	}
	return &item, nil
}

// Claim attempts to take itemID for peer. A lost race returns an error
// satisfying errors.Is(err, ErrConflict); callers pick another item instead of
// treating it as a failure.
func (c *Client) Claim(ctx context.Context, itemID, peer string) (*ClaimResult, error) {
	itemID = strings.TrimSpace(itemID)
	peer = strings.TrimSpace(peer)
	if itemID == "" || peer == "" {
		return nil, fmt.Errorf("%w: itemId and peer are required", ErrBadRequest)
	}
	body := struct {
		ItemID string `json:"itemId"`
		Peer   string `json:"peer"`
	}{itemID, peer}
	var res ClaimResult
	if err := c.do(ctx, http.MethodPost, "/claim", nil, body, &res); err != nil {
		return nil, err
	}
	if strings.TrimSpace(res.Token) == "" {
		return nil, &APIError{Method: http.MethodPost, Path: "/claim", StatusCode: http.StatusOK,
			Err: fmt.Errorf("%w: claim returned an empty token", ErrUnavailable)}
	}
	if res.History == nil {
		res.History = []*protocol.ContextEntry{}
	}
	return &res, nil
}

// Submit hands the finished work (identified by commitSha) to the integrator.
func (c *Client) Submit(ctx context.Context, token, peer, commitSha string) (*protocol.Item, error) {
	token = strings.TrimSpace(token)
	peer = strings.TrimSpace(peer)
	commitSha = strings.TrimSpace(commitSha)
	if token == "" || peer == "" || commitSha == "" {
		return nil, fmt.Errorf("%w: token, peer and commitSha are required", ErrBadRequest)
	}
	body := struct {
		Token     string `json:"token"`
		Peer      string `json:"peer"`
		CommitSha string `json:"commitSha"`
	}{token, peer, commitSha}
	var resp struct {
		OK   bool           `json:"ok"`
		Item *protocol.Item `json:"item"`
	}
	if err := c.do(ctx, http.MethodPost, "/submit", nil, body, &resp); err != nil {
		return nil, err
	}
	return resp.Item, nil
}

// Release gives a claim back without submitting, so another worker can take
// the item. Use it whenever a turn ends without a commit.
func (c *Client) Release(ctx context.Context, token string) error {
	token = strings.TrimSpace(token)
	if token == "" {
		return fmt.Errorf("%w: token is required", ErrBadRequest)
	}
	body := struct {
		Token string `json:"token"`
	}{token}
	return c.do(ctx, http.MethodPost, "/release", nil, body, nil)
}

// Renew extends the lease on token. Call it before the lease expires when a
// turn runs long, otherwise the item is handed to another worker.
func (c *Client) Renew(ctx context.Context, token string) (int64, error) {
	token = strings.TrimSpace(token)
	if token == "" {
		return 0, fmt.Errorf("%w: token is required", ErrBadRequest)
	}
	body := struct {
		Token string `json:"token"`
	}{token}
	var resp struct {
		OK        bool  `json:"ok"`
		ExpiresAt int64 `json:"expiresAt"`
	}
	if err := c.do(ctx, http.MethodPost, "/renew", nil, body, &resp); err != nil {
		return 0, err
	}
	return resp.ExpiresAt, nil
}

// AddContext publishes a shared-memory note (OBSERVED / FACT / FAIL).
func (c *Client) AddContext(ctx context.Context, req protocol.AddContextRequest) (*protocol.ContextEntry, error) {
	if strings.TrimSpace(req.Peer) == "" {
		return nil, fmt.Errorf("%w: peer is required", ErrBadRequest)
	}
	if strings.TrimSpace(req.Text) == "" {
		return nil, fmt.Errorf("%w: text is required", ErrBadRequest)
	}
	if strings.TrimSpace(req.Kind) == "" {
		req.Kind = protocol.KindObserved
	}
	var entry protocol.ContextEntry
	if err := c.do(ctx, http.MethodPost, "/context", nil, req, &entry); err != nil {
		return nil, err
	}
	return &entry, nil
}

// SearchContext greps the shared memory for older findings and failures.
// limit <= 0 means the server default.
func (c *Client) SearchContext(ctx context.Context, query string, limit int) ([]*protocol.ContextEntry, error) {
	q := url.Values{}
	q.Set("q", query)
	if limit > 0 {
		q.Set("limit", strconv.Itoa(limit))
	}
	var resp struct {
		Entries []*protocol.ContextEntry `json:"entries"`
	}
	if err := c.do(ctx, http.MethodGet, "/context/search", q, nil, &resp); err != nil {
		return nil, err
	}
	if resp.Entries == nil {
		resp.Entries = []*protocol.ContextEntry{}
	}
	return resp.Entries, nil
}

// Message sends a peer-to-peer note.
func (c *Client) Message(ctx context.Context, from, to, text string) (*protocol.Message, error) {
	to = strings.TrimSpace(to)
	if to == "" || strings.TrimSpace(text) == "" {
		return nil, fmt.Errorf("%w: to and text are required", ErrBadRequest)
	}
	if strings.TrimSpace(from) == "" {
		from = c.peer
	}
	body := protocol.Message{From: from, To: to, Text: text}
	var msg protocol.Message
	if err := c.do(ctx, http.MethodPost, "/message", nil, body, &msg); err != nil {
		return nil, err
	}
	return &msg, nil
}

// Messages lists notes addressed to peer received after since (unix millis).
func (c *Client) Messages(ctx context.Context, peer string, since int64) ([]*protocol.Message, error) {
	peer = strings.TrimSpace(peer)
	if peer == "" {
		peer = c.peer
	}
	if peer == "" {
		return nil, fmt.Errorf("%w: peer is required", ErrBadRequest)
	}
	q := url.Values{}
	q.Set("peer", peer)
	q.Set("since", strconv.FormatInt(since, 10))
	var resp struct {
		Messages []*protocol.Message `json:"messages"`
	}
	if err := c.do(ctx, http.MethodGet, "/messages", q, nil, &resp); err != nil {
		return nil, err
	}
	if resp.Messages == nil {
		resp.Messages = []*protocol.Message{}
	}
	return resp.Messages, nil
}

// Digest is the single-call turn briefing: stats, pending preview, this peer's
// claims, new shared context and new messages since the given timestamp.
// since = 0 returns everything.
func (c *Client) Digest(ctx context.Context, peer string, since int64) (*protocol.Digest, error) {
	peer = strings.TrimSpace(peer)
	if peer == "" {
		peer = c.peer
	}
	if peer == "" {
		return nil, fmt.Errorf("%w: peer is required", ErrBadRequest)
	}
	q := url.Values{}
	q.Set("peer", peer)
	q.Set("since", strconv.FormatInt(since, 10))
	var d protocol.Digest
	if err := c.do(ctx, http.MethodGet, "/digest", q, nil, &d); err != nil {
		return nil, err
	}
	if d.PendingPreview == nil {
		d.PendingPreview = []*protocol.Item{}
	}
	if d.Claims == nil {
		d.Claims = []*protocol.Claim{}
	}
	if d.NewContext == nil {
		d.NewContext = []*protocol.ContextEntry{}
	}
	if d.Messages == nil {
		d.Messages = []*protocol.Message{}
	}
	return &d, nil
}
