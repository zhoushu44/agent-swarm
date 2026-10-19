package store_test

import (
	"fmt"
	"path/filepath"
	"sync/atomic"
	"testing"

	"swarm-go/internal/server"
	"swarm-go/internal/store"
	"swarm-go/internal/store/storetest"
)

// openConformant returns a fresh, empty SQLite store with a strictly
// increasing clock, exactly like the MemStore conformance test does. The
// conformance suite asserts that an entry written after a watermark has
// ts >= watermark+1, which only holds for a monotonic clock (two notes written
// in the same wall-clock millisecond would otherwise be indistinguishable).
func openConformant(tb testing.TB) server.Store {
	tb.Helper()
	var tick int64 = 1_000_000
	name := fmt.Sprintf("file:conf%d?mode=memory&cache=shared", atomic.AddInt64(&tick, 1))
	s, err := store.Open(name)
	if err != nil {
		tb.Fatalf("open store: %v", err)
	}
	s.SetClock(func() int64 { return atomic.AddInt64(&tick, 1) })
	tb.Cleanup(func() { _ = s.Close() })
	return s
}

// TestStoreConformance runs the shared server.Store conformance suite against
// the SQLite-backed implementation, proving it agrees with the in-memory
// reference store the HTTP layer ships with.
func TestStoreConformance(t *testing.T) {
	storetest.Run(t, openConformant)
}

// TestOpenFilePersists proves the store really persists to disk: data written
// through one handle is still readable after that handle is closed and the
// database is re-opened (an in-memory DSN would lose everything here).
func TestOpenFilePersists(t *testing.T) {
	path := filepath.Join(t.TempDir(), "swarm.db")

	st, err := store.Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	it, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "persist me"})
	if err != nil {
		t.Fatalf("CreateItem: %v", err)
	}
	if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "survives reopen"}); err != nil {
		t.Fatalf("AddContext: %v", err)
	}
	if err := st.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	reopened, err := store.Open(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer reopened.Close()

	got, err := reopened.GetItem(it.ID)
	if err != nil {
		t.Fatalf("GetItem after reopen: %v", err)
	}
	if got.Title != "persist me" {
		t.Fatalf("title after reopen = %q, want %q", got.Title, "persist me")
	}
	if got.Status != store.StatusPending {
		t.Fatalf("status after reopen = %q, want %q", got.Status, store.StatusPending)
	}

	notes, err := reopened.SearchContext("survives", 10)
	if err != nil {
		t.Fatalf("SearchContext after reopen: %v", err)
	}
	if len(notes) != 1 {
		t.Fatalf("context after reopen = %d entries, want 1", len(notes))
	}
}

// TestIDsAreUnique pins the id shape: items get a "t" prefix, claim tokens a
// "c" prefix, and no two ids ever collide.
func TestIDsAreUnique(t *testing.T) {
	var tick int64 = 1_000
	st, err := store.OpenMemory()
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer st.Close()
	st.SetClock(func() int64 { return atomic.AddInt64(&tick, 1) })

	seen := map[string]bool{}
	for i := 0; i < 20; i++ {
		// Distinct titles: CreateItem dedups worker-created items by exact
		// title, so identical titles would fold into one item by design.
		it, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: fmt.Sprintf("t%d", i)})
		if err != nil {
			t.Fatalf("CreateItem: %v", err)
		}
		if seen[it.ID] {
			t.Fatalf("duplicate item id %q", it.ID)
		}
		seen[it.ID] = true
		if it.ID == "" || it.ID[0] != 't' {
			t.Fatalf("item id %q does not start with 't'", it.ID)
		}
		res, err := st.Claim(it.ID, "w1")
		if err != nil {
			t.Fatalf("Claim: %v", err)
		}
		if seen[res.Token] {
			t.Fatalf("duplicate claim token %q", res.Token)
		}
		seen[res.Token] = true
		if res.Token == "" || res.Token[0] != 'c' {
			t.Fatalf("claim token %q does not start with 'c'", res.Token)
		}
	}
}
