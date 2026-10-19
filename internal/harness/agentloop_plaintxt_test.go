package harness

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
)

// plainTextResponse builds a loop-chat response whose message has content but
// NO tool_calls — the shape a non-compliant endpoint returns when it ignores
// tool_choice:"required".
func plainTextResponse(text string) string {
	return fmt.Sprintf(`{"model":"m","choices":[{"message":{"role":"assistant","content":%q},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`, text)
}

// finishToolResponse builds a loop-chat response that calls the finish tool.
func finishToolResponse(summary string) string {
	arg, _ := json.Marshal(map[string]string{"summary": summary})
	argStr, _ := json.Marshal(string(arg)) // arguments is a string field
	return fmt.Sprintf(`{"model":"m","choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"finish","arguments":%s}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`, argStr)
}

// TestAgentLoopPlainTextRetriesThenFinishes: endpoint returns plain text while
// nothing is submitted, then complies. The loop must re-prime and continue,
// NOT treat the first plain-text reply as completion.
func TestAgentLoopPlainTextRetriesThenFinishes(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		n := atomic.AddInt64(&calls, 1)
		w.Header().Set("Content-Type", "application/json")
		if n <= 2 {
			fmt.Fprint(w, plainTextResponse("I'll help you!"))
			return
		}
		fmt.Fprint(w, finishToolResponse("done"))
	}))
	defer srv.Close()

	h, err := NewDirect(DirectConfig{BaseURL: srv.URL, APIKey: "k", Model: "m"})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	res, err := h.RunAgentLoop(context.Background(), "m", "do work", AgentLoopConfig{WorkDir: t.TempDir(), MaxSteps: 10})
	if err != nil {
		t.Fatalf("RunAgentLoop: %v", err)
	}
	if got := atomic.LoadInt64(&calls); got != 3 {
		t.Fatalf("server calls = %d, want 3 (2 plain-text retries + 1 finish)", got)
	}
	if res.Steps != 3 {
		t.Fatalf("Steps = %d, want 3", res.Steps)
	}
}

// TestAgentLoopPlainTextGivesUp: endpoint NEVER returns tool_calls. After
// maxPlainTextRetries re-primes the loop must stop instead of spinning to
// MaxSteps and burning tokens.
func TestAgentLoopPlainTextGivesUp(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt64(&calls, 1)
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, plainTextResponse("no tools for me"))
	}))
	defer srv.Close()

	h, err := NewDirect(DirectConfig{BaseURL: srv.URL, APIKey: "k", Model: "m"})
	if err != nil {
		t.Fatalf("NewDirect: %v", err)
	}
	res, err := h.RunAgentLoop(context.Background(), "m", "do work", AgentLoopConfig{WorkDir: t.TempDir(), MaxSteps: 40})
	if err != nil {
		t.Fatalf("RunAgentLoop: %v", err)
	}
	// 1 initial + 3 retries = 4 total, well under MaxSteps(40).
	if got := atomic.LoadInt64(&calls); got != 4 {
		t.Fatalf("server calls = %d, want 4 (initial + maxPlainTextRetries)", got)
	}
	if res.Steps != 4 {
		t.Fatalf("Steps = %d, want 4", res.Steps)
	}
	if res.Submitted {
		t.Fatalf("Submitted = true, want false (nothing was submitted)")
	}
}
