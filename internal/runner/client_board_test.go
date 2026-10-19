package runner

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"testing"

	"swarm-go/internal/protocol"
)

func TestBoardBuildsQueryAndDecodesPage(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"items":[{"id":"t1","title":"x","status":"pending"}],"total":1,"limit":5,"offset":10,"hasMore":true}`)
	page, err := c.Board(context.Background(), "pending", 5, 10)
	if err != nil {
		t.Fatalf("Board: %v", err)
	}
	if rec.method != http.MethodGet || rec.path != "/board" {
		t.Fatalf("request = %s %s", rec.method, rec.path)
	}
	for _, want := range []string{"status=pending", "limit=5", "offset=10"} {
		if !strings.Contains(rec.query, want) {
			t.Fatalf("query = %q, missing %q", rec.query, want)
		}
	}
	if len(page.Items) != 1 || page.Items[0].ID != "t1" || page.Total != 1 || !page.HasMore {
		t.Fatalf("page = %+v", page)
	}
}

func TestBoardDefaultsClampAndOmitBlankStatus(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{}`)
	page, err := c.Board(context.Background(), "   ", 0, -3)
	if err != nil {
		t.Fatalf("Board: %v", err)
	}
	if strings.Contains(rec.query, "status=") {
		t.Fatalf("blank status should be omitted: %q", rec.query)
	}
	if !strings.Contains(rec.query, "offset=0") {
		t.Fatalf("negative offset should clamp to 0: %q", rec.query)
	}
	if !strings.Contains(rec.query, "limit=200") {
		t.Fatalf("limit should fall back to the default: %q", rec.query)
	}
	if page.Items == nil {
		t.Fatal("Items must be non-nil")
	}
}

func TestPendingBoardUsesPendingStatus(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"items":[]}`)
	if _, err := c.PendingBoard(context.Background()); err != nil {
		t.Fatalf("PendingBoard: %v", err)
	}
	if !strings.Contains(rec.query, "status="+protocol.StatusPending) {
		t.Fatalf("query = %q", rec.query)
	}
}

func TestItemReturnsItemAndHistory(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"item":{"id":"t9"},"history":[{"peer":"w1","kind":"FAIL","text":"nope"}]}`)
	item, history, err := c.Item(context.Background(), " t9 ")
	if err != nil {
		t.Fatalf("Item: %v", err)
	}
	if rec.path != "/item" || !strings.Contains(rec.query, "id=t9") {
		t.Fatalf("request = %s %s", rec.path, rec.query)
	}
	if item == nil || item.ID != "t9" {
		t.Fatalf("item = %+v", item)
	}
	if len(history) != 1 || history[0].Kind != protocol.KindFail {
		t.Fatalf("history = %+v", history)
	}
}

func TestItemRejectsEmptyID(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, _, err := c.Item(context.Background(), "   "); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestCreateItemValidatesAndPosts(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"id":"t5","title":"do it","kind":"task"}`)
	parent := "t0"
	item, err := c.CreateItem(context.Background(), protocol.CreateItemRequest{Peer: "w2", Title: "do it", ParentID: &parent})
	if err != nil {
		t.Fatalf("CreateItem: %v", err)
	}
	if rec.method != http.MethodPost || rec.path != "/item" {
		t.Fatalf("request = %s %s", rec.method, rec.path)
	}
	if rec.body["peer"] != "w2" || rec.body["title"] != "do it" || rec.body["parentId"] != "t0" || rec.body["kind"] != "task" {
		t.Fatalf("body = %+v", rec.body)
	}
	if item == nil || item.ID != "t5" {
		t.Fatalf("item = %+v", item)
	}
}

func TestCreateItemRequiresPeerAndTitle(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.CreateItem(context.Background(), protocol.CreateItemRequest{Title: "x"}); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
	if _, err := c.CreateItem(context.Background(), protocol.CreateItemRequest{Peer: "w2"}); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}
