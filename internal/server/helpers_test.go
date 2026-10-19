package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// newTestServer returns a Server with a deterministic clock.
func newTestServer(t *testing.T) *Server {
	t.Helper()
	clock := int64(1000)
	tick := func() int64 { clock += 10; return clock }
	srv := New(NewMemStore(tick))
	srv.SetClock(tick)
	return srv
}

func do(t *testing.T, srv *Server, method, target, body string) (*httptest.ResponseRecorder, map[string]any) {
	t.Helper()
	var r *http.Request
	if body == "" {
		r = httptest.NewRequest(method, target, nil)
	} else {
		r = httptest.NewRequest(method, target, strings.NewReader(body))
	}
	w := httptest.NewRecorder()
	srv.ServeHTTP(w, r)
	var out map[string]any
	if w.Body.Len() > 0 {
		if err := json.Unmarshal(w.Body.Bytes(), &out); err != nil {
			t.Fatalf("%s %s: bad JSON %q: %v", method, target, w.Body.String(), err)
		}
	}
	return w, out
}

func TestHealthz(t *testing.T) {
	srv := newTestServer(t)
	w, out := do(t, srv, "GET", "/healthz", "")
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d", w.Code)
	}
	if out["ok"] != true {
		t.Fatalf("ok = %v", out["ok"])
	}
}

func TestMethodNotAllowed(t *testing.T) {
	srv := newTestServer(t)
	w, _ := do(t, srv, "GET", "/claim", "")
	if w.Code != http.StatusMethodNotAllowed {
		t.Fatalf("status = %d, want 405", w.Code)
	}
	if got := w.Header().Get("Allow"); got != "POST" {
		t.Fatalf("Allow = %q", got)
	}
}

func TestCreateItemAndBoard(t *testing.T) {
	srv := newTestServer(t)
	w, out := do(t, srv, "POST", "/item", `{"peer":"w1","title":"do a thing","detail":"d"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("create status = %d body=%s", w.Code, w.Body.String())
	}
	id, _ := out["id"].(string)
	if id == "" {
		t.Fatalf("no id in %v", out)
	}
	if out["status"] != "pending" || out["kind"] != "task" {
		t.Fatalf("unexpected item %v", out)
	}

	w, board := do(t, srv, "GET", "/board", "")
	if w.Code != http.StatusOK {
		t.Fatalf("board status = %d", w.Code)
	}
	if board["total"].(float64) != 1 {
		t.Fatalf("total = %v", board["total"])
	}
	items, _ := board["items"].([]any)
	if len(items) != 1 {
		t.Fatalf("items = %v", board["items"])
	}
	if board["hasMore"] != false {
		t.Fatalf("hasMore = %v", board["hasMore"])
	}
}

func TestCreateItemValidation(t *testing.T) {
	srv := newTestServer(t)
	w, _ := do(t, srv, "POST", "/item", `{"peer":"w1","title":"   "}`)
	if w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
	w, _ = do(t, srv, "POST", "/item", `not json`)
	if w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
	w, _ = do(t, srv, "POST", "/item", `{"peer":"w1","title":"child","parentId":"nope"}`)
	if w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", w.Code)
	}
}

func TestBoardFilterAndPaging(t *testing.T) {
	srv := newTestServer(t)
	for i := 0; i < 3; i++ {
		if w, _ := do(t, srv, "POST", "/item", `{"peer":"w1","title":"t"}`); w.Code != http.StatusOK {
			t.Fatalf("create %d status = %d", i, w.Code)
		}
	}
	_, board := do(t, srv, "GET", "/board?limit=2", "")
	if board["total"].(float64) != 3 || board["hasMore"] != true {
		t.Fatalf("board = %v", board)
	}
	_, page2 := do(t, srv, "GET", "/board?limit=2&offset=2", "")
	if items, _ := page2["items"].([]any); len(items) != 1 {
		t.Fatalf("page2 items = %v", page2["items"])
	}
	_, none := do(t, srv, "GET", "/board?status=done", "")
	if none["total"].(float64) != 0 {
		t.Fatalf("done total = %v", none["total"])
	}
}

func TestGetItemMissing(t *testing.T) {
	srv := newTestServer(t)
	if w, _ := do(t, srv, "GET", "/item", ""); w.Code != http.StatusBadRequest {
		t.Fatalf("no id status = %d, want 400", w.Code)
	}
	if w, _ := do(t, srv, "GET", "/item?id=zzz", ""); w.Code != http.StatusNotFound {
		t.Fatalf("unknown id status = %d, want 404", w.Code)
	}
}
