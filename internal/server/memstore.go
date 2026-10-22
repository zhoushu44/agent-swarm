package server

import (
	"fmt"
	"sort"
	"strings"
	"sync"
)

// MemStore is a concurrency-safe in-memory Store implementation.
//
// It exists so the HTTP layer is testable (and runnable) without SQLite: this
// host has no C compiler, so cgo-backed drivers cannot build. internal/store
// can later replace it behind the same interface.
type MemStore struct {
	mu       sync.Mutex
	seq      int64
	items    map[string]*Item
	order    []string
	claims   map[string]*Claim // token -> claim
	contexts []ContextEntry
	messages []Message
	now      func() int64
}

// NewMemStore returns an empty in-memory store.
func NewMemStore(now func() int64) *MemStore {
	if now == nil {
		now = func() int64 { return 0 }
	}
	return &MemStore{
		items:  map[string]*Item{},
		claims: map[string]*Claim{},
		now:    now,
	}
}

func (m *MemStore) nextID(prefix string) string {
	m.seq++
	return fmt.Sprintf("%s%d", prefix, m.seq)
}

// ListItems implements Store.
func (m *MemStore) ListItems(status string, limit, offset int) ([]Item, int, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	all := make([]Item, 0, len(m.order))
	for _, id := range m.order {
		it := m.items[id]
		if status != "" && it.Status != status {
			continue
		}
		all = append(all, *it)
	}
	sort.SliceStable(all, func(i, j int) bool {
		if all[i].Priority != all[j].Priority {
			return all[i].Priority > all[j].Priority
		}
		return all[i].CreatedAt < all[j].CreatedAt
	})
	total := len(all)
	if offset > total {
		offset = total
	}
	end := offset + limit
	if end > total {
		end = total
	}
	return all[offset:end], total, nil
}

// GetItem implements Store.
func (m *MemStore) GetItem(id string) (Item, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	it, ok := m.items[id]
	if !ok {
		return Item{}, fmt.Errorf("item %s: %w", id, ErrNotFound)
	}
	return *it, nil
}

// CreateItem implements Store.
func (m *MemStore) CreateItem(in CreateItemInput) (Item, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if strings.TrimSpace(in.Title) == "" {
		return Item{}, fmt.Errorf("title required: %w", ErrInvalid)
	}
	if in.ParentID != nil {
		if _, ok := m.items[*in.ParentID]; !ok {
			return Item{}, fmt.Errorf("parent %s: %w", *in.ParentID, ErrNotFound)
		}
	}
	kind := in.Kind
	if kind == "" {
		kind = "task"
	}
	it := &Item{
		ID:        m.nextID("t"),
		ParentID:  in.ParentID,
		Title:     in.Title,
		Detail:    in.Detail,
		Kind:      kind,
		Priority:  in.Priority,
		Status:    "pending",
		CreatedBy: in.Peer,
		CreatedAt: m.now(),
	}
	m.items[it.ID] = it
	m.order = append(m.order, it.ID)
	return *it, nil
}

// Claim implements Store.
func (m *MemStore) Claim(itemID, peer string) (ClaimResult, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	it, ok := m.items[itemID]
	if !ok {
		return ClaimResult{}, fmt.Errorf("item %s: %w", itemID, ErrNotFound)
	}
	if it.Status != "pending" {
		return ClaimResult{}, fmt.Errorf("item %s is %s: %w", itemID, it.Status, ErrConflict)
	}
	for _, c := range m.claims {
		if c.ItemID == itemID {
			return ClaimResult{}, fmt.Errorf("item %s already claimed: %w", itemID, ErrConflict)
		}
	}
	token := m.nextID("c")
	claim := &Claim{Token: token, ItemID: itemID, Title: it.Title, ExpiresAt: m.now() + 300000}
	m.claims[token] = claim
	it.Status = "in_progress"
	return ClaimResult{Token: token, Item: *it, History: m.historyLocked(itemID)}, nil
}

// Release implements Store.
func (m *MemStore) Release(token string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.claims[token]
	if !ok {
		return fmt.Errorf("claim %s: %w", token, ErrNotFound)
	}
	delete(m.claims, token)
	if it, ok := m.items[c.ItemID]; ok && it.Status == "in_progress" {
		it.Status = "pending"
	}
	return nil
}

// Renew implements Store.
func (m *MemStore) Renew(token string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.claims[token]
	if !ok {
		return fmt.Errorf("claim %s: %w", token, ErrNotFound)
	}
	c.ExpiresAt = m.now() + 300000
	return nil
}

// Submit implements Store.
func (m *MemStore) Submit(token, peer, commitSha string) (Item, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.claims[token]
	if !ok {
		return Item{}, fmt.Errorf("claim %s: %w", token, ErrNotFound)
	}
	it, ok := m.items[c.ItemID]
	if !ok {
		return Item{}, fmt.Errorf("item %s: %w", c.ItemID, ErrNotFound)
	}
	if strings.TrimSpace(commitSha) == "" {
		return Item{}, fmt.Errorf("commitSha required: %w", ErrInvalid)
	}
	delete(m.claims, token)
	it.Status = "submitted"
	done := m.now()
	it.DoneAt = &done
	return *it, nil
}

// NeedsSplit implements Store. Mirrors the SQLite rule: a seeded top-level item
// with no children yet should be decomposed by its first worker.
func (m *MemStore) NeedsSplit(itemID string) (bool, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	it, ok := m.items[strings.TrimSpace(itemID)]
	if !ok {
		return false, fmt.Errorf("item %s: %w", itemID, ErrNotFound)
	}
	if strings.TrimSpace(it.CreatedBy) != "seed" {
		return false, nil
	}
	if it.ParentID != nil {
		return false, nil
	}
	switch it.Status {
	case "split", "done", "failed":
		return false, nil
	}
	for _, child := range m.items {
		if child.ParentID != nil && *child.ParentID == it.ID {
			return false, nil
		}
	}
	return true, nil
}

// AddContext implements Store.
func (m *MemStore) AddContext(entry ContextEntry) (ContextEntry, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if strings.TrimSpace(entry.Text) == "" {
		return ContextEntry{}, fmt.Errorf("text required: %w", ErrInvalid)
	}
	m.seq++
	entry.ID = m.seq
	if entry.TS == 0 {
		entry.TS = m.now()
	}
	m.contexts = append(m.contexts, entry)
	return entry, nil
}

// SearchContext implements Store (case-insensitive substring match).
func (m *MemStore) SearchContext(q string, limit int) ([]ContextEntry, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	q = strings.ToLower(strings.TrimSpace(q))
	out := []ContextEntry{}
	for _, e := range m.contexts {
		if q == "" || strings.Contains(strings.ToLower(e.Text), q) {
			out = append(out, e)
		}
	}
	if limit > 0 && len(out) > limit {
		out = out[len(out)-limit:]
	}
	return out, nil
}

// ItemHistory implements Store.
func (m *MemStore) ItemHistory(itemID string) ([]ContextEntry, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.historyLocked(itemID), nil
}

func (m *MemStore) historyLocked(itemID string) []ContextEntry {
	out := []ContextEntry{}
	for _, e := range m.contexts {
		if e.ItemID != nil && *e.ItemID == itemID {
			out = append(out, e)
		}
	}
	return out
}

// Digest implements Store.
func (m *MemStore) Digest(peer string, since int64) (Digest, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	d := Digest{Now: m.now(), Stats: Stats{Total: len(m.order)}}
	for _, id := range m.order {
		switch m.items[id].Status {
		case "pending":
			d.Stats.Pending++
		case "in_progress":
			d.Stats.InProgress++
		case "submitted":
			d.Stats.Submitted++
		case "done":
			d.Stats.Done++
		case "failed":
			d.Stats.Failed++
		}
	}
	d.PendingPreview = []Item{}
	d.Claims = []Claim{}
	d.NewContext = []ContextEntry{}
	d.Messages = []Message{}
	for _, id := range m.order {
		it := m.items[id]
		if it.Status == "pending" && len(d.PendingPreview) < 5 {
			d.PendingPreview = append(d.PendingPreview, *it)
		}
	}
	tokens := make([]string, 0, len(m.claims))
	for tok := range m.claims {
		tokens = append(tokens, tok)
	}
	sort.Strings(tokens)
	for _, tok := range tokens {
		c := m.claims[tok]
		if peer != "" && m.items[c.ItemID].CreatedBy != peer {
			continue
		}
		d.Claims = append(d.Claims, *c)
	}
	for _, e := range m.contexts {
		if e.TS >= since {
			d.NewContext = append(d.NewContext, e)
		}
	}
	for _, msg := range m.messages {
		if peer == "" || msg.To == peer {
			if msg.TS >= since {
				d.Messages = append(d.Messages, msg)
			}
		}
	}
	return d, nil
}

// SendMessage implements Store.
func (m *MemStore) SendMessage(msg Message) (Message, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if strings.TrimSpace(msg.To) == "" || strings.TrimSpace(msg.Text) == "" {
		return Message{}, fmt.Errorf("to and text required: %w", ErrInvalid)
	}
	m.seq++
	msg.ID = m.seq
	if msg.TS == 0 {
		msg.TS = m.now()
	}
	m.messages = append(m.messages, msg)
	return msg, nil
}

// Messages implements Store.
func (m *MemStore) Messages(peer string, since int64) ([]Message, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := []Message{}
	for _, msg := range m.messages {
		if peer != "" && msg.To != peer {
			continue
		}
		if msg.TS >= since {
			out = append(out, msg)
		}
	}
	return out, nil
}
