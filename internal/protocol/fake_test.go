package protocol

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// fakeBackend is a minimal in-memory Backend used to exercise the HTTP layer.
type fakeBackend struct {
	items    map[string]*Item
	claims   map[string]*Claim
	contexts []*ContextEntry
	messages []*Message
	nextID   int
	claimErr error
}

func newFake() *fakeBackend {
	return &fakeBackend{items: map[string]*Item{}, claims: map[string]*Claim{}}
}

func (f *fakeBackend) Board(status string, limit, offset int) (BoardPage, error) {
	all := make([]*Item, 0, len(f.items))
	for _, it := range f.items {
		if status != "" && it.Status != status {
			continue
		}
		all = append(all, it)
	}
	page := BoardPage{Items: all, Total: len(all)}
	if offset < len(all) {
		end := offset + limit
		if end > len(all) {
			end = len(all)
		}
		page.Items = all[offset:end]
		page.HasMore = end < len(all)
	} else {
		page.Items = nil
	}
	return page, nil
}

func (f *fakeBackend) Item(id string) (*Item, []*ContextEntry, error) {
	it, ok := f.items[id]
	if !ok {
		return nil, nil, fmt.Errorf("%w: item %s", ErrNotFound, id)
	}
	var hist []*ContextEntry
	for _, c := range f.contexts {
		if c.ItemID != nil && *c.ItemID == id {
			hist = append(hist, c)
		}
	}
	return it, hist, nil
}

func (f *fakeBackend) CreateItem(req CreateItemRequest) (*Item, error) {
	f.nextID++
	it := &Item{
		ID:        fmt.Sprintf("t%d", f.nextID),
		ParentID:  req.ParentID,
		Title:     req.Title,
		Detail:    req.Detail,
		Kind:      req.Kind,
		Priority:  req.Priority,
		Status:    StatusPending,
		CreatedBy: req.Peer,
		CreatedAt: 1000,
	}
	f.items[it.ID] = it
	return it, nil
}

func (f *fakeBackend) Claim(itemID, peer string) (*Claim, *Item, []*ContextEntry, error) {
	if f.claimErr != nil {
		return nil, nil, nil, f.claimErr
	}
	it, ok := f.items[itemID]
	if !ok {
		return nil, nil, nil, fmt.Errorf("%w: item %s", ErrNotFound, itemID)
	}
	if it.Status == StatusFailed {
		return nil, nil, nil, fmt.Errorf("%w: item %s failed", ErrConflict, itemID)
	}
	c := &Claim{Token: "tok-" + itemID, ItemID: itemID, Peer: peer, Title: it.Title, ExpiresAt: 9999}
	f.claims[c.Token] = c
	it.Status = StatusInProgress
	hist := make([]*ContextEntry, 0, 1)
	for _, e := range f.contexts {
		if e.ItemID != nil && *e.ItemID == itemID {
			hist = append(hist, e)
		}
	}
	return c, it, hist, nil
}

func (f *fakeBackend) Submit(token, peer, commitSha string) (*Item, error) {
	c, ok := f.claims[token]
	if !ok {
		return nil, fmt.Errorf("%w: token", ErrNotFound)
	}
	it := f.items[c.ItemID]
	it.Status = StatusSubmitted
	return it, nil
}

func (f *fakeBackend) Release(token, peer string) error {
	if _, ok := f.claims[token]; !ok {
		return fmt.Errorf("%w: token", ErrNotFound)
	}
	delete(f.claims, token)
	return nil
}

func (f *fakeBackend) Renew(token, peer string) (int64, error) {
	c, ok := f.claims[token]
	if !ok {
		return 0, fmt.Errorf("%w: token", ErrNotFound)
	}
	c.ExpiresAt += 300
	return c.ExpiresAt, nil
}

func (f *fakeBackend) AddContext(req AddContextRequest) (*ContextEntry, error) {
	e := &ContextEntry{Peer: req.Peer, Kind: req.Kind, ItemID: req.ItemID, Text: req.Text, Tags: req.Tags, TS: 2000}
	f.contexts = append(f.contexts, e)
	return e, nil
}

func (f *fakeBackend) SearchContext(query string) ([]*ContextEntry, error) {
	var out []*ContextEntry
	for _, c := range f.contexts {
		if strings.Contains(c.Text, query) {
			out = append(out, c)
		}
	}
	return out, nil
}

func (f *fakeBackend) Message(from, to, text string) (*Message, error) {
	m := &Message{From: from, To: to, Text: text, TS: 3000}
	f.messages = append(f.messages, m)
	return m, nil
}

func (f *fakeBackend) Digest(peer string, since int64) (*Digest, error) {
	page, _ := f.Board(StatusPending, 10, 0)
	var claims []*Claim
	for _, c := range f.claims {
		if c.Peer == peer {
			claims = append(claims, c)
		}
	}
	return &Digest{
		Now:            5000,
		Stats:          Stats{Total: len(f.items), Pending: len(page.Items)},
		PendingPreview: page.Items,
		Claims:         claims,
		NewContext:     f.contexts,
		Messages:       f.messages,
	}, nil
}

func do(t *testing.T, h http.Handler, method, target, body string) *httptest.ResponseRecorder {
	t.Helper()
	var r *http.Request
	if body == "" {
		r = httptest.NewRequest(method, target, nil)
	} else {
		r = httptest.NewRequest(method, target, strings.NewReader(body))
	}
	w := httptest.NewRecorder()
	h.ServeHTTP(w, r)
	return w
}

func decodeMap(t *testing.T, w *httptest.ResponseRecorder) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &m); err != nil {
		t.Fatalf("body is not JSON: %v (%s)", err, w.Body.String())
	}
	return m
}
