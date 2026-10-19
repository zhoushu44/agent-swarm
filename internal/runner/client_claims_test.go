package runner

import (
	"context"
	"errors"
	"net/http"
	"testing"

	"swarm-go/internal/protocol"
)

func TestClaimSendsBodyAndReturnsPackage(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"token":"tok","item":{"id":"t2"},"history":[]}`)
	res, err := c.Claim(context.Background(), "t2", "w2")
	if err != nil {
		t.Fatalf("Claim: %v", err)
	}
	if rec.method != http.MethodPost || rec.path != "/claim" {
		t.Fatalf("request = %s %s", rec.method, rec.path)
	}
	if rec.body["itemId"] != "t2" || rec.body["peer"] != "w2" {
		t.Fatalf("body = %+v", rec.body)
	}
	if res.Token != "tok" || res.Item == nil || res.Item.ID != "t2" {
		t.Fatalf("result = %+v", res)
	}
	if res.History == nil {
		t.Fatal("History must be non-nil")
	}
}

func TestClaimValidatesArguments(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.Claim(context.Background(), "", "w2"); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
	if _, err := c.Claim(context.Background(), "t1", "  "); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestClaimConflictIsTyped(t *testing.T) {
	c, _ := newTestClient(t, http.StatusConflict, `{"error":"item already claimed"}`)
	_, err := c.Claim(context.Background(), "t2", "w2")
	if !IsConflict(err) || !errors.Is(err, ErrConflict) {
		t.Fatalf("err = %v, want conflict", err)
	}
	if IsRetryable(err) {
		t.Fatalf("conflict must not be retryable: %v", err)
	}
	var apiErr *APIError
	if !errors.As(err, &apiErr) || apiErr.StatusCode != http.StatusConflict {
		t.Fatalf("apiErr = %+v", apiErr)
	}
	if apiErr.Message != "item already claimed" || apiErr.Method != http.MethodPost || apiErr.Path != "/claim" {
		t.Fatalf("apiErr = %+v", apiErr)
	}
}

func TestClaimEmptyTokenIsUnavailable(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{"item":{"id":"t2"}}`)
	if _, err := c.Claim(context.Background(), "t2", "w2"); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("err = %v, want ErrUnavailable", err)
	}
}

func TestSubmitSendsBodyAndDecodesItem(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"ok":true,"item":{"id":"t3","status":"submitted"}}`)
	item, err := c.Submit(context.Background(), "tok", "w2", "abc123")
	if err != nil {
		t.Fatalf("Submit: %v", err)
	}
	if rec.path != "/submit" || rec.body["token"] != "tok" || rec.body["peer"] != "w2" || rec.body["commitSha"] != "abc123" {
		t.Fatalf("request = %s %+v", rec.path, rec.body)
	}
	if item == nil || item.Status != protocol.StatusSubmitted {
		t.Fatalf("item = %+v", item)
	}
}

func TestSubmitRejectsMissingArguments(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.Submit(context.Background(), "tok", "w2", ""); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
	if _, err := c.Submit(context.Background(), "", "w2", "abc"); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestReleasePostsToken(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"ok":true}`)
	if err := c.Release(context.Background(), " tok "); err != nil {
		t.Fatalf("Release: %v", err)
	}
	if rec.path != "/release" || rec.body["token"] != "tok" {
		t.Fatalf("request = %s %+v", rec.path, rec.body)
	}
}

func TestReleaseRejectsEmptyToken(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if err := c.Release(context.Background(), "  "); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}

func TestRenewReturnsExpiry(t *testing.T) {
	c, rec := newTestClient(t, http.StatusOK, `{"ok":true,"expiresAt":1700000000000}`)
	exp, err := c.Renew(context.Background(), "tok")
	if err != nil {
		t.Fatalf("Renew: %v", err)
	}
	if rec.path != "/renew" || rec.body["token"] != "tok" {
		t.Fatalf("request = %s %+v", rec.path, rec.body)
	}
	if exp != 1700000000000 {
		t.Fatalf("expiresAt = %d", exp)
	}
}

func TestRenewRejectsEmptyToken(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{}`)
	if _, err := c.Renew(context.Background(), ""); !errors.Is(err, ErrBadRequest) {
		t.Fatalf("err = %v, want ErrBadRequest", err)
	}
}
