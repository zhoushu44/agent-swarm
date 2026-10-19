package store_test

import (
	"path/filepath"
	"sync/atomic"
	"testing"

	"swarm-go/internal/server"
	"swarm-go/internal/store"
	"swarm-go/internal/store/storetest"
)

// TestStoreConformance runs the shared server.Store conformance suite against
// the SQLite-backed implementation, proving it agrees with the in-memory
// reference store the HTTP layer ships with.
func TestStoreConformance(t *testing.T) {
	storetest.Run(t, func(tb testing.TB) server.Store {
		tb.Helper()
		s, err := store.Open("file::memory:?cache=shared")
		if err != nil {
			tb.Fatalf("open store: %v", err)
		}
		tb.Cleanup(func() { _ = s.Close() })
		return s
	})
}

// TestOpenFilePersists proves the store really persists to disk: data written
// through one handle is still readable after that handle is closed and the
// database is re-opened (an in-memory DSN would lose everything here).
func TestOpenFilePersists(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "swarm.db")

	st, err := store.Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	it, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "persist me"})
	if err != nil {
		t.Fatalf("CreateItem: %v", err)
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
}

// TestIDsAreUnique pins the id shape: items get a "t" prefix, claims a "c"
// prefix, and no two ids ever collide.
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
		it, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "t"})
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
