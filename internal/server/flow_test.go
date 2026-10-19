package server

import (
	"net/http"
	"testing"
)

func TestClaimSubmitFlow(t *testing.T) {
	srv := newTestServer(t)
	_, created := do(t, srv, "POST", "/item", `{"peer":"w1","title":"work"}`)
	id := created["id"].(string)

	w, claimed := do(t, srv, "POST", "/claim", `{"itemId":"`+id+`","peer":"w2"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("claim status = %d body=%s", w.Code, w.Body.String())
	}
	token, _ := claimed["token"].(string)
	if token == "" {
		t.Fatalf("no token: %v", claimed)
	}
	if item, _ := claimed["item"].(map[string]any); item["status"] != "in_progress" {
		t.Fatalf("item status = %v", item["status"])
	}
	if hist, _ := claimed["history"].([]any); len(hist) != 0 {
		t.Fatalf("history = %v", claimed["history"])
	}

	w, _ = do(t, srv, "POST", "/claim", `{"itemId":"`+id+`","peer":"w3"}`)
	if w.Code != http.StatusConflict {
		t.Fatalf("second claim status = %d, want 409", w.Code)
	}

	if w, _ = do(t, srv, "POST", "/renew", `{"token":"`+token+`"}`); w.Code != http.StatusOK {
		t.Fatalf("renew status = %d", w.Code)
	}
	w, sub := do(t, srv, "POST", "/submit", `{"token":"`+token+`","peer":"w2","commitSha":"abc123"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("submit status = %d body=%s", w.Code, w.Body.String())
	}
	if item, _ := sub["item"].(map[string]any); item["status"] != "submitted" {
		t.Fatalf("submitted status = %v", item["status"])
	}

	if w, _ = do(t, srv, "POST", "/submit", `{"token":"`+token+`","peer":"w2","commitSha":"abc"}`); w.Code != http.StatusNotFound {
		t.Fatalf("reused token status = %d, want 404", w.Code)
	}
}

func TestClaimValidationAndRelease(t *testing.T) {
	srv := newTestServer(t)
	if w, _ := do(t, srv, "POST", "/claim", `{"itemId":""}`); w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
	if w, _ := do(t, srv, "POST", "/claim", `{"itemId":"nope","peer":"w1"}`); w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", w.Code)
	}
	_, created := do(t, srv, "POST", "/item", `{"peer":"w1","title":"x"}`)
	id := created["id"].(string)
	_, claimed := do(t, srv, "POST", "/claim", `{"itemId":"`+id+`","peer":"w1"}`)
	token := claimed["token"].(string)

	if w, _ := do(t, srv, "POST", "/release", `{"token":"`+token+`"}`); w.Code != http.StatusOK {
		t.Fatalf("release status = %d", w.Code)
	}
	_, got := do(t, srv, "GET", "/item?id="+id, "")
	if item, _ := got["item"].(map[string]any); item["status"] != "pending" {
		t.Fatalf("after release status = %v", item["status"])
	}
	if w, _ := do(t, srv, "POST", "/release", `{"token":"gone"}`); w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", w.Code)
	}
	if w, _ := do(t, srv, "POST", "/renew", `{}`); w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
}

func TestSubmitRequiresCommitSha(t *testing.T) {
	srv := newTestServer(t)
	_, created := do(t, srv, "POST", "/item", `{"peer":"w1","title":"s"}`)
	id := created["id"].(string)
	_, claimed := do(t, srv, "POST", "/claim", `{"itemId":"`+id+`","peer":"w1"}`)
	token := claimed["token"].(string)
	if w, _ := do(t, srv, "POST", "/submit", `{"token":"`+token+`","peer":"w1"}`); w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
}
