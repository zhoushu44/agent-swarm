package harness

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func userReq(text string) Request {
	return Request{Messages: []Message{{Role: RoleUser, Content: text}}}
}

func TestRequestValidate(t *testing.T) {
	tests := []struct {
		name string
		req  Request
		want error
	}{
		{"empty", Request{}, ErrEmptyConversation},
		{"bad role", Request{Messages: []Message{{Role: "tool", Content: "x"}}}, ErrBadRole},
		{"ok", userReq("hi"), nil},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := tc.req.Validate()
			if !errors.Is(err, tc.want) {
				t.Fatalf("Validate() = %v, want %v", err, tc.want)
			}
		})
	}
}

func TestRequestNormalize(t *testing.T) {
	if got := (Request{}).Normalize().MaxTokens; got != DefaultMaxTokens {
		t.Fatalf("MaxTokens = %d, want %d", got, DefaultMaxTokens)
	}
	if got := (Request{MaxTokens: 7}).Normalize().MaxTokens; got != 7 {
		t.Fatalf("MaxTokens = %d, want 7", got)
	}
}

func TestRequestLastUser(t *testing.T) {
	r := Request{Messages: []Message{
		{Role: RoleSystem, Content: "sys"},
		{Role: RoleUser, Content: "first"},
		{Role: RoleAssistant, Content: "answer"},
		{Role: RoleUser, Content: "second"},
	}}
	if got := r.LastUser(); got != "second" {
		t.Fatalf("LastUser() = %q, want second", got)
	}
	if got := (Request{}).LastUser(); got != "" {
		t.Fatalf("LastUser() = %q, want empty", got)
	}
}

func TestEchoHarness(t *testing.T) {
	h := NewEcho()
	resp, err := h.Complete(context.Background(), userReq("  hello world  "))
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if resp.Text != "echo: hello world" {
		t.Fatalf("Text = %q", resp.Text)
	}
	if resp.Model != "echo" {
		t.Fatalf("Model = %q", resp.Model)
	}
	if resp.Usage.PromptTokens != 2 || resp.Usage.CompletionTokens != 3 {
		t.Fatalf("Usage = %+v", resp.Usage)
	}
	if resp.Usage.TotalTokens != resp.Usage.PromptTokens+resp.Usage.CompletionTokens {
		t.Fatalf("TotalTokens = %d", resp.Usage.TotalTokens)
	}
	if resp.Attempts != 1 {
		t.Fatalf("Attempts = %d", resp.Attempts)
	}
}

func TestEchoHarnessErrors(t *testing.T) {
	h := NewEcho()
	if _, err := h.Complete(context.Background(), Request{}); !errors.Is(err, ErrEmptyConversation) {
		t.Fatalf("err = %v, want ErrEmptyConversation", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := h.Complete(ctx, userReq("x")); !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v, want context.Canceled", err)
	}
}

func TestHarnessFuncAdapts(t *testing.T) {
	h := HarnessFunc(func(ctx context.Context, req Request) (Response, error) {
		return Response{Text: "hi"}, nil
	})
	resp, err := h.Complete(context.Background(), userReq("x"))
	if err != nil || resp.Text != "hi" {
		t.Fatalf("resp = %+v, err = %v", resp, err)
	}
}

func TestSemaphoreBoundsConcurrency(t *testing.T) {
	sem := NewSemaphore(2)
	if sem.Cap() != 2 {
		t.Fatalf("Cap() = %d", sem.Cap())
	}
	var cur, peak int32
	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := sem.Acquire(context.Background()); err != nil {
				t.Errorf("Acquire: %v", err)
				return
			}
			defer sem.Release()
			n := atomic.AddInt32(&cur, 1)
			for {
				p := atomic.LoadInt32(&peak)
				if n <= p || atomic.CompareAndSwapInt32(&peak, p, n) {
					break
				}
			}
			time.Sleep(5 * time.Millisecond)
			atomic.AddInt32(&cur, -1)
		}()
	}
	wg.Wait()
	if peak > 2 {
		t.Fatalf("peak concurrency = %d, want <= 2", peak)
	}
	if sem.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", sem.Len())
	}
}

func TestSemaphoreContextCancel(t *testing.T) {
	sem := NewSemaphore(1)
	if !sem.TryAcquire() {
		t.Fatal("TryAcquire should succeed on a free semaphore")
	}
	if sem.TryAcquire() {
		t.Fatal("TryAcquire should fail when the only slot is held")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if err := sem.Acquire(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Acquire err = %v, want DeadlineExceeded", err)
	}
	sem.Release()
}

func TestSemaphoreClampsZeroCapacity(t *testing.T) {
	if got := NewSemaphore(0).Cap(); got != 1 {
		t.Fatalf("Cap() = %d, want 1", got)
	}
}

func TestSemaphoreDoReleasesOnPanic(t *testing.T) {
	sem := NewSemaphore(1)
	func() {
		defer func() { _ = recover() }()
		_ = sem.Do(context.Background(), func() error { panic("boom") })
	}()
	if sem.Len() != 0 {
		t.Fatalf("slot leaked after panic: Len() = %d", sem.Len())
	}
}
