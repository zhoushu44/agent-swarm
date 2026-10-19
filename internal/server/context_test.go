package server

import (
	"net/http"
	"testing"
)

func TestContextPublishSearchAndHistory(t *testing.T) {
	srv := newTestServer(t)
	_, created := do(t, srv, "POST", "/item", `{"peer":"w1","title":"ctx"}`)
	id := created["id"].(string)

	w, entry := do(t, srv, "POST", "/context", `{"peer":"w1","kind":"FAIL","text":"sqlite cgo stub failed","itemId":"`+id+`","tags":["build"]}`)
	if w.Code != http.StatusOK {
		t.Fatalf("context status = %d body=%s", w.Code, w.Body.String())
	}
	if entry["kind"] != "FAIL" || entry["ts"].(float64) == 0 {
		t.Fatalf("entry = %v", entry)
	}

	if w, _ = do(t, srv, "POST", "/context", `{"peer":"w1","text":"  "}`); w.Code != http.StatusBadRequest {
		t.Fatalf("empty text status = %d, want 400", w.Code)
	}

	_, found := do(t, srv, "GET", "/context/search?q=CGO", "")
	if found["count"].(float64) != 1 {
		t.Fatalf("search = %v", found)
	}
	_, empty := do(t, srv, "GET", "/context/search?q=zzz", "")
	if entries, _ := empty["entries"].([]any); len(entries) != 0 {
		t.Fatalf("empty search = %v", empty["entries"])
	}

	_, got := do(t, srv, "GET", "/item?id="+id, "")
	if hist, _ := got["history"].([]any); len(hist) != 1 {
		t.Fatalf("history = %v", got["history"])
	}
}

func TestDigestAndMessages(t *testing.T) {
	srv := newTestServer(t)
	_, created := do(t, srv, "POST", "/item", `{"peer":"w1","title":"d"}`)
	id := created["id"].(string)
	do(t, srv, "POST", "/claim", `{"itemId":"`+id+`","peer":"w1"}`)
	do(t, srv, "POST", "/context", `{"peer":"w2","kind":"FACT","text":"a fact"}`)

	if w, _ := do(t, srv, "POST", "/message", `{"from":"w2","to":"w1","text":"hi"}`); w.Code != http.StatusOK {
		t.Fatalf("message status = %d", w.Code)
	}
	if w, _ := do(t, srv, "POST", "/message", `{"from":"w2","text":"hi"}`); w.Code != http.StatusBadRequest {
		t.Fatalf("message validation status = %d", w.Code)
	}

	w, digest := do(t, srv, "GET", "/digest?peer=w1&since=0", "")
	if w.Code != http.StatusOK {
		t.Fatalf("digest status = %d", w.Code)
	}
	stats, _ := digest["stats"].(map[string]any)
	if stats["total"].(float64) != 1 || stats["inProgress"].(float64) != 1 {
		t.Fatalf("stats = %v", stats)
	}
	if claims, _ := digest["claims"].([]any); len(claims) != 1 {
		t.Fatalf("claims = %v", digest["claims"])
	}
	if nc, _ := digest["newContext"].([]any); len(nc) != 1 {
		t.Fatalf("newContext = %v", digest["newContext"])
	}
	if msgs, _ := digest["messages"].([]any); len(msgs) != 1 {
		t.Fatalf("messages = %v", digest["messages"])
	}
	if digest["now"].(float64) == 0 {
		t.Fatalf("now missing")
	}

	_, msgs := do(t, srv, "GET", "/messages?peer=w1&since=0", "")
	if msgs["count"].(float64) != 1 {
		t.Fatalf("messages = %v", msgs)
	}
	_, other := do(t, srv, "GET", "/messages?peer=w9&since=0", "")
	if other["count"].(float64) != 0 {
		t.Fatalf("other peer messages = %v", other)
	}
}

func TestSubItemParentLink(t *testing.T) {
	srv := newTestServer(t)
	_, parent := do(t, srv, "POST", "/item", `{"peer":"w1","title":"parent"}`)
	pid := parent["id"].(string)
	w, child := do(t, srv, "POST", "/item", `{"peer":"w2","title":"child","parentId":"`+pid+`","kind":"task"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("child status = %d body=%s", w.Code, w.Body.String())
	}
	if child["parentId"] != pid {
		t.Fatalf("parentId = %v, want %s", child["parentId"], pid)
	}
}
