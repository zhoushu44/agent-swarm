package runner

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"testing"

	"swarm-go/internal/protocol"
)

func TestAddContextDefaultsKindAndSendsBody(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"peer":"w2","kind":"OBSERVED","text":"hi"}`)
	id := "t4"
	entry, err := c.AddContext(context.Background(), protocol.AddContextRequest{Peer: "w2", Text: "hi", ItemID: &id})
	if err != nil {
		t.Fatalf("AddContext: %v", err)
	}
	if rec.path != "/context" {
		t.Fatalf("path = %s", rec.path)
	}
	if rec.body["kind"] != protocol.KindObserved || rec.body["text"] != "hi" || rec.body["itemId"] != "t4" {
		t.Fatalf("body = %+v", rec.body)
	}
	if entry == nil || entry.Peer != "w2" {
		t.Fatalf("entry = %+v", entry)
	}
}

func TestAddContextKeepsExplicitKind(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"peer":"w2","kind":"FAIL","text":"nope"}`)
	if _, err := c.AddContext(context.Background(), protocol.AddContextRequest{Peer: "w2", Kind: protocol.KindFail, Text: "nope"}); err != nil {
		t.Fatalf("AddContext: %v", err)
	}
	if rec.body["kind"] != protocol.KindFail {
		t.Fatalf("body = %+v", rec.body)
	}
}

func TestAddContextRequiresPeerAndText(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.AddContext(context.Background(), protocol.AddContextRequest{Text: "x"}); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
	if _, err := c.AddContext(context.Background(), protocol.AddContextRequest{Peer: "w2"}); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestSearchContextBuildsQuery(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"entries":[{"kind":"FACT","text":"a"}]}`)
	entries, err := c.SearchContext(context.Background(), "client.go", 7)
	if err != nil {
		t.Fatalf("SearchContext: %v", err)
	}
	if rec.path != "/context/search" || !strings.Contains(rec.query, "q=client.go") || !strings.Contains(rec.query, "limit=7") {
		t.Fatalf("request = %s %s", rec.path, rec.query)
	}
	if len(entries) != 1 || entries[0].Kind != protocol.KindFact {
		t.Fatalf("entries = %+v", entries)
	}
}

func TestSearchContextOmitsNonPositiveLimit(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"entries":[]}`)
	if _, err := c.SearchContext(context.Background(), "x", 0); err != nil {
		t.Fatalf("SearchContext: %v", err)
	}
	if strings.Contains(rec.query, "limit=") {
		t.Fatalf("query = %q", rec.query)
	}
}

func TestMessageUsesDefaultPeerAsSender(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"from":"w2","to":"w1","text":"hey"}`)
	c.SetPeer("w2")
	msg, err := c.Message(context.Background(), "", "w1", "hey")
	if err != nil {
		t.Fatalf("Message: %v", err)
	}
	if rec.path != "/message" || rec.body["from"] != "w2" || rec.body["to"] != "w1" || rec.body["text"] != "hey" {
		t.Fatalf("request = %s %+v", rec.path, rec.body)
	}
	if msg == nil || msg.To != "w1" {
		t.Fatalf("msg = %+v", msg)
	}
}

func TestMessageRejectsMissingRecipientOrText(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	c.SetPeer("w2")
	if _, err := c.Message(context.Background(), "w2", "  ", "hey"); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
	if _, err := c.Message(context.Background(), "w2", "w1", "  "); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestMessagesUsesPeerDefaultAndSince(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"messages":[{"from":"w1","to":"w2","text":"yo"}]}`)
	c.SetPeer("w2")
	msgs, err := c.Messages(context.Background(), "", 42)
	if err != nil {
		t.Fatalf("Messages: %v", err)
	}
	if rec.path != "/messages" || !strings.Contains(rec.query, "peer=w2") || !strings.Contains(rec.query, "since=42") {
		t.Fatalf("request = %s %s", rec.path, rec.query)
	}
	if len(msgs) != 1 || msgs[0].Text != "yo" {
		t.Fatalf("msgs = %+v", msgs)
	}
}

func TestMessagesRequiresAPeer(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.Messages(context.Background(), "", 0); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestDigestUsesPeerDefaultAndNormalises(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"now":123,"stats":{"pending":2}}`)
	c.SetPeer("w2")
	d, err := c.Digest(context.Background(), "", 0)
	if err != nil {
		t.Fatalf("Digest: %v", err)
	}
	if rec.path != "/digest" || !strings.Contains(rec.query, "peer=w2") || !strings.Contains(rec.query, "since=0") {
		t.Fatalf("request = %s %s", rec.path, rec.query)
	}
	if d.Now != 123 || d.Stats.Pending != 2 {
		t.Fatalf("digest = %+v", d)
	}
	if d.PendingPreview == nil || d.Claims == nil || d.NewContext == nil || d.Messages == nil {
		t.Fatal("digest slices must be non-nil")
	}
}

func TestDigestRequiresAPeer(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.Digest(context.Background(), "", 0); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}
