package harness

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

func TestDirectHarnessSuccess(t *testing.T) {
	var gotAuth, gotPath string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		gotPath = r.URL.Path
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"model":"test-model","choices":[{"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}`)
	}))
	defer srv.Close()

	h, err := NewDirect(DirectConfig{BaseURL: srv.URL, APIKey: "k", Model: "test-model"})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	resp, err := h.Complete(context.Background(), userReq("ping"))
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if resp.Text != "pong" || resp.StopReason != "stop" || resp.Model != "test-model" {
		t.Fatalf("resp = %+v", resp)
	}
	if resp.Usage.TotalTokens != 4 {
		t.Fatalf("TotalTokens = %d, want 4 (derived)", resp.Usage.TotalTokens)
	}
	if resp.Attempts != 1 {
		t.Fatalf("Attempts = %d", resp.Attempts)
	}
	if gotAuth != "Bearer k" {
		t.Fatalf("Authorization = %q", gotAuth)
	}
	if gotPath != DefaultChatPath {
		t.Fatalf("path = %q", gotPath)
	}
}

func TestDirectHarnessRetriesThenSucceeds(t *testing.T) {
	var n int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if atomic.AddInt32(&n, 1) == 1 {
			http.Error(w, "slow down", http.StatusTooManyRequests)
			return
		}
		fmt.Fprint(w, `{"choices":[{"message":{"content":"second try"}}]}`)
	}))
	defer srv.Close()

	h, err := NewDirect(DirectConfig{BaseURL: srv.URL, Model: "m", RetryBackoff: time.Millisecond, MaxRetries: 2})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	resp, err := h.Complete(context.Background(), userReq("hi"))
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if resp.Text != "second try" || resp.Attempts != 2 {
		t.Fatalf("resp = %+v", resp)
	}
}

func TestDirectHarnessDoesNotRetry4xx(t *testing.T) {
	var n int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&n, 1)
		http.Error(w, "bad request", http.StatusBadRequest)
	}))
	defer srv.Close()

	h, _ := NewDirect(DirectConfig{BaseURL: srv.URL, Model: "m", MaxRetries: 3, RetryBackoff: time.Millisecond})
	_, err := h.Complete(context.Background(), userReq("hi"))
	if err == nil {
		t.Fatal("expected an error")
	}
	var httpErr *HTTPError
	if !errors.As(err, &httpErr) || httpErr.StatusCode != http.StatusBadRequest {
		t.Fatalf("err = %v, want HTTPError 400", err)
	}
	if got := atomic.LoadInt32(&n); got != 1 {
		t.Fatalf("server hit %d times, want 1 (no retry on 4xx)", got)
	}
}

func TestDirectHarnessGivesUpAfterRetries(t *testing.T) {
	var n int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&n, 1)
		http.Error(w, "boom", http.StatusInternalServerError)
	}))
	defer srv.Close()

	h, _ := NewDirect(DirectConfig{BaseURL: srv.URL, Model: "m", MaxRetries: 2, RetryBackoff: time.Millisecond})
	if _, err := h.Complete(context.Background(), userReq("hi")); err == nil {
		t.Fatal("expected an error")
	}
	if got := atomic.LoadInt32(&n); got != 3 {
		t.Fatalf("server hit %d times, want 3 (1 try + 2 retries)", got)
	}
}

func TestDirectHarnessEmptyChoices(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, `{"choices":[]}`)
	}))
	defer srv.Close()

	h, _ := NewDirect(DirectConfig{BaseURL: srv.URL, Model: "m"})
	if _, err := h.Complete(context.Background(), userReq("hi")); !errors.Is(err, ErrEmptyCompletion) {
		t.Fatalf("err = %v, want ErrEmptyCompletion", err)
	}
}

func TestDirectHarnessRequiresModel(t *testing.T) {
	h, err := NewDirect(DirectConfig{BaseURL: "http://127.0.0.1:1"})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	if _, err := h.Complete(context.Background(), userReq("hi")); !errors.Is(err, ErrNoModel) {
		t.Fatalf("err = %v, want ErrNoModel", err)
	}
}

func TestDirectHarnessRequestModelOverridesConfig(t *testing.T) {
	var gotModel string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Model string `json:"model"`
		}
		_ = decodeJSON(r, &body)
		gotModel = body.Model
		fmt.Fprint(w, `{"choices":[{"message":{"content":"ok"}}]}`)
	}))
	defer srv.Close()

	h, _ := NewDirect(DirectConfig{BaseURL: srv.URL, Model: "cfg-model"})
	req := userReq("hi")
	req.Model = "req-model"
	if _, err := h.Complete(context.Background(), req); err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if gotModel != "req-model" {
		t.Fatalf("model sent = %q, want req-model", gotModel)
	}
}

func TestDirectHarnessRejectsBadRequest(t *testing.T) {
	h, err := NewDirect(DirectConfig{BaseURL: "http://127.0.0.1:1", Model: "m"})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	if _, err := h.Complete(context.Background(), Request{}); !errors.Is(err, ErrEmptyConversation) {
		t.Fatalf("err = %v, want ErrEmptyConversation", err)
	}
}

func TestDirectConfigNormalize(t *testing.T) {
	cfg := DirectConfig{BaseURL: "https://x.test/"}.Normalize()
	if cfg.BaseURL != "https://x.test" {
		t.Fatalf("BaseURL = %q", cfg.BaseURL)
	}
	if cfg.ChatPath != DefaultChatPath {
		t.Fatalf("ChatPath = %q", cfg.ChatPath)
	}
	if cfg.Timeout != DefaultTimeout {
		t.Fatalf("Timeout = %v", cfg.Timeout)
	}
	if cfg.RetryBackoff <= 0 || cfg.Client == nil {
		t.Fatalf("cfg = %+v", cfg)
	}
	if got := (DirectConfig{ChatPath: "v1/chat"}).Normalize().ChatPath; got != "/v1/chat" {
		t.Fatalf("ChatPath = %q, want /v1/chat", got)
	}
}

func TestHTTPErrorRetryable(t *testing.T) {
	cases := map[int]bool{
		http.StatusBadRequest:          false,
		http.StatusUnauthorized:        false,
		http.StatusTooManyRequests:     true,
		http.StatusInternalServerError: true,
		http.StatusBadGateway:          true,
	}
	for code, want := range cases {
		e := &HTTPError{StatusCode: code, Status: fmt.Sprint(code)}
		if got := e.Retryable(); got != want {
			t.Fatalf("status %d Retryable() = %v, want %v", code, got, want)
		}
	}
}
