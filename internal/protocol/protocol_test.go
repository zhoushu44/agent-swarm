package protocol

import (
	"fmt"
	"net/http"
	"testing"
)

func TestBoardEmptyIsJSONArray(t *testing.T) {
	h := New(newFake())
	w := do(t, h, http.MethodGet, "/board", "")
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", w.Code)
	}
	m := decodeMap(t, w)
	items, ok := m["items"].([]any)
	if !ok || len(items) != 0 {
		t.Fatalf("items = %v, want empty array", m["items"])
	}
	if m["limit"].(float64) != 200 {
		t.Fatalf("limit = %v, want 200", m["limit"])
	}
}

func TestBoardRejectsPost(t *testing.T) {
	h := New(newFake())
	w := do(t, h, http.MethodPost, "/board", "{}")
	if w.Code != http.StatusMethodNotAllowed {
		t.Fatalf("status = %d, want 405", w.Code)
	}
	if w.Header().Get("Allow") != http.MethodGet {
		t.Fatalf("Allow = %q, want GET", w.Header().Get("Allow"))
	}
}

func TestBoardClampsLimitAndFiltersStatus(t *testing.T) {
	f := newFake()
	h := New(f)
	for i := 0; i < 3; i++ {
		if _, err := f.CreateItem(CreateItemRequest{Peer: "w1", Title: fmt.Sprintf("t%d", i)}); err != nil {
			t.Fatal(err)
		}
	}
	w := do(t, h, http.MethodGet, "/board?status=pending&limit=99999", "")
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d", w.Code)
	}
	m := decodeMap(t, w)
	if m["limit"].(float64) != 200 {
		t.Fatalf("limit = %v, want clamp to 200", m["limit"])
	}
	if n := len(m["items"].([]any)); n != 3 {
		t.Fatalf("items = %d, want 3", n)
	}
	w = do(t, h, http.MethodGet, "/board?status=done", "")
	if n := len(decodeMap(t, w)["items"].([]any)); n != 0 {
		t.Fatalf("done items = %d, want 0", n)
	}
}

func TestCreateThenGetItem(t *testing.T) {
	f := newFake()
	h := New(f)
	w := do(t, h, http.MethodPost, "/item", `{"peer":"w1","title":"do thing","detail":"d","parentId":"t0"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("create status = %d body=%s", w.Code, w.Body.String())
	}
	m := decodeMap(t, w)
	id, _ := m["id"].(string)
	if id == "" {
		t.Fatalf("no id in %v", m)
	}
	if m["kind"] != "task" {
		t.Fatalf("kind = %v, want default task", m["kind"])
	}
	if m["parent_id"] != "t0" {
		t.Fatalf("parent_id = %v, want t0", m["parent_id"])
	}

	w = do(t, h, http.MethodGet, "/item?id="+id, "")
	if w.Code != http.StatusOK {
		t.Fatalf("get status = %d", w.Code)
	}
	m = decodeMap(t, w)
	if _, ok := m["item"].(map[string]any); !ok {
		t.Fatalf("item missing in %v", m)
	}
	if hist, ok := m["history"].([]any); !ok || len(hist) != 0 {
		t.Fatalf("history = %v, want empty array", m["history"])
	}
}

func TestCreateItemValidation(t *testing.T) {
	h := New(newFake())
	for _, body := range []string{
		`{"title":"x"}`,
		`{"peer":"w1"}`,
		`{"peer":"w1","title":"   "}`,
		`not json`,
		``,
	} {
		w := do(t, h, http.MethodPost, "/item", body)
		if w.Code != http.StatusBadRequest {
			t.Fatalf("body %q -> status %d, want 400", body, w.Code)
		}
		if decodeMap(t, w)["error"] == nil {
			t.Fatalf("body %q -> missing error field", body)
		}
	}
}

func TestGetItemMissingIDAndNotFound(t *testing.T) {
	h := New(newFake())
	if w := do(t, h, http.MethodGet, "/item", ""); w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
	if w := do(t, h, http.MethodGet, "/item?id=nope", ""); w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", w.Code)
	}
}

func TestClaimReturnsTokenItemHistory(t *testing.T) {
	f := newFake()
	h := New(f)
	it, _ := f.CreateItem(CreateItemRequest{Peer: "w1", Title: "t", Kind: "task"})
	itemID := it.ID
	f.contexts = append(f.contexts, &ContextEntry{Peer: "w9", Kind: KindFail, ItemID: &itemID, Text: "boom"})

	w := do(t, h, http.MethodPost, "/claim", `{"itemId":"`+itemID+`","peer":"w4"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d body=%s", w.Code, w.Body.String())
	}
	m := decodeMap(t, w)
	if m["token"] == nil || m["token"] == "" {
		t.Fatalf("no token in %v", m)
	}
	if hist, ok := m["history"].([]any); !ok || len(hist) != 1 {
		t.Fatalf("history = %v, want 1 entry", m["history"])
	}
}

func TestClaimConflictAndNotFound(t *testing.T) {
	f := newFake()
	h := New(f)
	f.claimErr = fmt.Errorf("%w: taken", ErrConflict)
	if w := do(t, h, http.MethodPost, "/claim", `{"itemId":"t1","peer":"w4"}`); w.Code != http.StatusConflict {
		t.Fatalf("status = %d, want 409", w.Code)
	}
	f.claimErr = nil
	if w := do(t, h, http.MethodPost, "/claim", `{"itemId":"t1","peer":"w4"}`); w.Code != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", w.Code)
	}
	if w := do(t, h, http.MethodPost, "/claim", `{"itemId":"","peer":"w4"}`); w.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", w.Code)
	}
}

func TestSubmitRenewReleaseRoundTrip(t *testing.T) {
	f := newFake()
	h := New(f)
	it, _ := f.CreateItem(CreateItemRequest{Peer: "w1", Title: "t"})
	tok := "tok-" + it.ID
	if w := do(t, h, http.MethodPost, "/claim", `{"itemId":"`+it.ID+`","peer":"w4"}`); w.Code != http.StatusOK {
		t.Fatalf("claim status = %d", w.Code)
	}

	w := do(t, h, http.MethodPost, "/renew", `{"token":"`+tok+`","peer":"w4"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("renew status = %d body=%s", w.Code, w.Body.String())
	}
	if decodeMap(t, w)["expiresAt"] == nil {
		t.Fatalf("renew missing expiresAt")
	}

	w = do(t, h, http.MethodPost, "/submit", `{"token":"`+tok+`","peer":"w4","commitSha":"abc123"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("submit status = %d body=%s", w.Code, w.Body.String())
	}
	if f.items[it.ID].Status != StatusSubmitted {
		t.Fatalf("status = %s, want submitted", f.items[it.ID].Status)
	}

	if w := do(t, h, http.MethodPost, "/release", `{"token":"`+tok+`","peer":"w4"}`); w.Code != http.StatusOK {
		t.Fatalf("release status = %d", w.Code)
	}
	if w := do(t, h, http.MethodPost, "/release", `{"token":"nope","peer":"w4"}`); w.Code != http.StatusNotFound {
		t.Fatalf("release unknown token status = %d, want 404", w.Code)
	}
}

func TestSubmitRequiresAllFields(t *testing.T) {
	h := New(newFake())
	for _, body := range []string{
		`{"peer":"w4","commitSha":"abc"}`,
		`{"token":"t","peer":"w4"}`,
		`{"token":"t","commitSha":"abc"}`,
	} {
		if w := do(t, h, http.MethodPost, "/submit", body); w.Code != http.StatusBadRequest {
			t.Fatalf("body %q -> status %d, want 400", body, w.Code)
		}
	}
}

func TestContextValidationAndSearch(t *testing.T) {
	h := New(newFake())
	w := do(t, h, http.MethodPost, "/context", `{"peer":"w4","kind":"fact","text":"sqlite needs modernc","tags":["go"]}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d body=%s", w.Code, w.Body.String())
	}
	entry := decodeMap(t, w)["entry"].(map[string]any)
	if entry["kind"] != KindFact {
		t.Fatalf("kind = %v, want FACT (upper-cased)", entry["kind"])
	}

	for _, body := range []string{
		`{"kind":"FACT","text":"x"}`,
		`{"peer":"w4","kind":"NOPE","text":"x"}`,
		`{"peer":"w4","kind":"FACT","text":"  "}`,
	} {
		if w := do(t, h, http.MethodPost, "/context", body); w.Code != http.StatusBadRequest {
			t.Fatalf("body %q -> status %d, want 400", body, w.Code)
		}
	}

	w = do(t, h, http.MethodGet, "/context/search?q=modernc", "")
	if w.Code != http.StatusOK {
		t.Fatalf("search status = %d", w.Code)
	}
	if n := len(decodeMap(t, w)["entries"].([]any)); n != 1 {
		t.Fatalf("entries = %d, want 1", n)
	}
	if w := do(t, h, http.MethodGet, "/context/search", ""); w.Code != http.StatusBadRequest {
		t.Fatalf("missing q -> %d, want 400", w.Code)
	}
	if w := do(t, h, http.MethodGet, "/context/search?q=nomatch", ""); w.Code != http.StatusOK {
		t.Fatalf("empty result -> %d, want 200", w.Code)
	}
}

func TestMessageEndpoint(t *testing.T) {
	h := New(newFake())
	w := do(t, h, http.MethodPost, "/message", `{"from":"w1","to":"w4","text":"hi"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d body=%s", w.Code, w.Body.String())
	}
	if decodeMap(t, w)["message"] == nil {
		t.Fatalf("missing message")
	}
	if w := do(t, h, http.MethodPost, "/message", `{"from":"w1","to":"w4"}`); w.Code != http.StatusBadRequest {
		t.Fatalf("missing text -> %d, want 400", w.Code)
	}
}

func TestDigestShapeAndClaimFilter(t *testing.T) {
	f := newFake()
	h := New(f)
	it, _ := f.CreateItem(CreateItemRequest{Peer: "w1", Title: "t"})
	if w := do(t, h, http.MethodPost, "/claim", `{"itemId":"`+it.ID+`","peer":"w4"}`); w.Code != http.StatusOK {
		t.Fatalf("claim status = %d", w.Code)
	}
	w := do(t, h, http.MethodGet, "/digest?peer=w4&since=0", "")
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d", w.Code)
	}
	m := decodeMap(t, w)
	for _, k := range []string{"now", "stats", "pendingPreview", "claims", "newContext", "messages"} {
		if _, ok := m[k]; !ok {
			t.Fatalf("digest missing %q: %v", k, m)
		}
	}
	if n := len(m["claims"].([]any)); n != 1 {
		t.Fatalf("claims = %d, want 1", n)
	}
	if n := len(m["messages"].([]any)); n != 0 {
		t.Fatalf("messages = %d, want empty array", n)
	}
}

func TestValidKind(t *testing.T) {
	for _, k := range []string{"FACT", "observed", " Fail "} {
		if !ValidKind(k) {
			t.Fatalf("ValidKind(%q) = false", k)
		}
	}
	if ValidKind("PATCH_SUMMARY") {
		t.Fatal("PATCH_SUMMARY must not be client-postable")
	}
}
