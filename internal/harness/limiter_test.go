package harness

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// fakeClock drives a Limiter deterministically so refill math can be asserted
// without sleeping.
type fakeClock struct{ now time.Time }

func newFakeClock() *fakeClock { return &fakeClock{now: time.Unix(1700000000, 0)} }

func (c *fakeClock) attach(l *Limiter) {
	l.now = func() time.Time { return c.now }
	l.sleep = func(ctx context.Context, d time.Duration) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		c.now = c.now.Add(d)
		return nil
	}
}

func TestLimiterBurstThenRefill(t *testing.T) {
	clk := newFakeClock()
	l := NewLimiter(1, time.Second, 2)
	clk.attach(l)

	for i := 0; i < 2; i++ {
		if !l.Allow() {
			t.Fatalf("call %d should be allowed from the burst", i)
		}
	}
	if l.Allow() {
		t.Fatal("third immediate call must be rejected")
	}
	if wait := l.Reserve(); wait <= 0 {
		t.Fatalf("Reserve() = %v, want > 0", wait)
	}

	clk.now = clk.now.Add(500 * time.Millisecond)
	if l.Allow() {
		t.Fatal("half a token should not admit a call")
	}
	clk.now = clk.now.Add(500 * time.Millisecond)
	if !l.Allow() {
		t.Fatal("a full second at 1/s should refill exactly one token")
	}
	if l.Allow() {
		t.Fatal("bucket must be empty again")
	}
}

func TestLimiterCapsAtBurst(t *testing.T) {
	clk := newFakeClock()
	l := NewLimiter(1, time.Second, 2)
	clk.attach(l)
	if got := l.Tokens(); got != 2 {
		t.Fatalf("initial tokens = %v, want 2", got)
	}
	clk.now = clk.now.Add(10 * time.Second)
	if got := l.Tokens(); got != 2 {
		t.Fatalf("tokens after idle = %v, want burst cap 2", got)
	}
}

func TestLimiterWaitAdvancesClock(t *testing.T) {
	clk := newFakeClock()
	l := NewLimiter(1, time.Second, 1)
	clk.attach(l)

	if err := l.Wait(context.Background()); err != nil {
		t.Fatalf("first Wait: %v", err)
	}
	start := clk.now
	if err := l.Wait(context.Background()); err != nil {
		t.Fatalf("second Wait: %v", err)
	}
	if elapsed := clk.now.Sub(start); elapsed < 900*time.Millisecond {
		t.Fatalf("second Wait waited %v, want ~1s", elapsed)
	}
}

func TestLimiterWaitHonoursContext(t *testing.T) {
	l := NewLimiter(1, time.Hour, 1)
	if !l.Allow() {
		t.Fatal("burst token should be available")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if err := l.Wait(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Wait err = %v, want DeadlineExceeded", err)
	}
}

func TestNewLimiterDefaults(t *testing.T) {
	l := NewLimiter(0, 0, 0)
	if l.perInterval != 1 || l.interval != time.Second || l.burst != 1 {
		t.Fatalf("defaults = %+v", l)
	}
}

func TestLimitedWrapsHarness(t *testing.T) {
	sem := NewSemaphore(1)
	lim := NewLimiter(100, time.Second, 100)
	var calls int32
	inner := HarnessFunc(func(ctx context.Context, req Request) (Response, error) {
		atomic.AddInt32(&calls, 1)
		return Response{Text: "ok"}, nil
	})
	h := Limited(inner, sem, lim)
	if _, err := h.Complete(context.Background(), userReq("a")); err != nil {
		t.Fatalf("Complete: %v", err)
	}
	if atomic.LoadInt32(&calls) != 1 {
		t.Fatalf("calls = %d", calls)
	}
	if sem.Len() != 0 {
		t.Fatalf("semaphore slot leaked: Len() = %d", sem.Len())
	}
}

func TestLimitedBoundsConcurrency(t *testing.T) {
	sem := NewSemaphore(2)
	var cur, peak int32
	inner := HarnessFunc(func(ctx context.Context, req Request) (Response, error) {
		n := atomic.AddInt32(&cur, 1)
		for {
			p := atomic.LoadInt32(&peak)
			if n <= p || atomic.CompareAndSwapInt32(&peak, p, n) {
				break
			}
		}
		time.Sleep(5 * time.Millisecond)
		atomic.AddInt32(&cur, -1)
		return Response{Text: "ok"}, nil
	})
	h := Limited(inner, sem, nil)
	var wg sync.WaitGroup
	for i := 0; i < 10; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, err := h.Complete(context.Background(), userReq("x")); err != nil {
				t.Errorf("Complete: %v", err)
			}
		}()
	}
	wg.Wait()
	if peak > 2 {
		t.Fatalf("peak in-flight = %d, want <= 2", peak)
	}
	if sem.Len() != 0 {
		t.Fatalf("semaphore slot leaked: Len() = %d", sem.Len())
	}
}

func TestLimitedNilHarness(t *testing.T) {
	h := Limited(nil, nil, nil)
	if _, err := h.Complete(context.Background(), userReq("a")); !errors.Is(err, ErrNilHarness) {
		t.Fatalf("err = %v, want ErrNilHarness", err)
	}
}

func TestLimitedReleasesSlotOnPanic(t *testing.T) {
	sem := NewSemaphore(1)
	inner := HarnessFunc(func(context.Context, Request) (Response, error) { panic("boom") })
	h := Limited(inner, sem, nil)
	func() {
		defer func() { _ = recover() }()
		_, _ = h.Complete(context.Background(), userReq("a"))
	}()
	if sem.Len() != 0 {
		t.Fatalf("slot leaked after panic: Len() = %d", sem.Len())
	}
}

func TestLimitedContextCancelWhileWaiting(t *testing.T) {
	sem := NewSemaphore(1)
	if !sem.TryAcquire() {
		t.Fatal("TryAcquire should succeed")
	}
	defer sem.Release()
	h := Limited(NewEcho(), sem, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if _, err := h.Complete(ctx, userReq("a")); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("err = %v, want DeadlineExceeded", err)
	}
}
