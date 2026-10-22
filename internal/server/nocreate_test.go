package server

import (
	"net/http"
	"testing"
)

// TestForbidItemCreationBlocksWorkerCreations is the hard counterpart of the
// --no-create prompt rule: the board must not grow from worker invention.
func TestForbidItemCreationBlocksWorkerCreations(t *testing.T) {
	srv := newTestServer(t)
	srv.ForbidItemCreation = true

	// A worker creating new top-level work must be refused.
	w, out := do(t, srv, "POST", "/item", `{"title":"worker invented this","peer":"w1"}`)
	if w.Code != http.StatusForbidden {
		t.Fatalf("worker top-level create = %d, want 403", w.Code)
	}
	// The refusal body names the reason (writeError returns a bare string).
	msg, _ := out["error"].(string)
	if !contains(msg, "disabled") {
		t.Fatalf("403 body should explain the refusal, got %q", msg)
	}

	// The seeder (no peer) must still work.
	if w, _ := do(t, srv, "POST", "/item", `{"title":"seeded item","peer":""}`); w.Code != http.StatusOK {
		t.Fatalf("seed create = %d, want 200", w.Code)
	}

	// A worker attaching under an existing parent is a SPLIT, not new work.
	if w, _ := do(t, srv, "POST", "/item", `{"title":"split child","peer":"w1","parentId":"t1"}`); w.Code != http.StatusOK {
		t.Fatalf("split create = %d, want 200", w.Code)
	}

	// auto-split must still work.
	if w, _ := do(t, srv, "POST", "/item", `{"title":"auto split child","peer":"auto-split"}`); w.Code != http.StatusOK {
		t.Fatalf("auto-split create = %d, want 200", w.Code)
	}
}

// TestDefaultAllowsCreation: the flag off keeps the original behaviour.
func TestDefaultAllowsCreation(t *testing.T) {
	srv := newTestServer(t)
	if w, _ := do(t, srv, "POST", "/item", `{"title":"anything goes","peer":"w1"}`); w.Code != http.StatusOK {
		t.Fatalf("default create = %d, want 200", w.Code)
	}
}

func contains(s, sub string) bool {
	return len(s) >= len(sub) && (func() bool {
		for i := 0; i+len(sub) <= len(s); i++ {
			if s[i:i+len(sub)] == sub {
				return true
			}
		}
		return false
	})()
}
