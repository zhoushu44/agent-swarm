package harness

import (
	"context"
	"fmt"
	"sync"
	"time"
)

// Limiter is a token bucket: it admits at most Burst calls immediately and then
// refills at PerInterval calls per Interval. Workers use it to stay under a
// provider's requests-per-minute limit without any global coordination.
//
// It is safe for concurrent use. The zero value is not usable; use NewLimiter.
type Limiter struct {
	mu          sync.Mutex
	interval    time.Duration
	perInterval float64
	burst       float64
	tokens      float64
	last        time.Time
	now         func() time.Time
	sleep       func(context.Context, time.Duration) error
}

// NewLimiter returns a Limiter admitting perInterval calls every interval, with
// an initial burst allowance of burst (clamped to at least 1).
func NewLimiter(perInterval int, interval time.Duration, burst int) *Limiter {
	if perInterval <= 0 {
		perInterval = 1
	}
	if interval <= 0 {
		interval = time.Second
	}
	if burst <= 0 {
		burst = perInterval
	}
	if burst < 1 {
		burst = 1
	}
	return &Limiter{
		interval:    interval,
		perInterval: float64(perInterval),
		burst:       float64(burst),
		tokens:      float64(burst),
		now:         time.Now,
		sleep:       sleepCtx,
	}
}

// rate is how many tokens accrue per nanosecond.
func (l *Limiter) rate() float64 { return l.perInterval / float64(l.interval) }

// refill tops the bucket up according to the elapsed time. Caller holds mu.
func (l *Limiter) refillLocked(now time.Time) {
	if l.last.IsZero() {
		l.last = now
		return
	}
	elapsed := now.Sub(l.last)
	if elapsed <= 0 {
		return
	}
	l.tokens += float64(elapsed) * l.rate()
	if l.tokens > l.burst {
		l.tokens = l.burst
	}
	l.last = now
}

// Allow consumes a token if one is available, without blocking.
func (l *Limiter) Allow() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refillLocked(l.now())
	if l.tokens < 1 {
		return false
	}
	l.tokens--
	return true
}

// Reserve returns how long the caller must wait for the next token, or 0 when
// one is available right now. It does not consume the token; Wait does.
func (l *Limiter) Reserve() time.Duration {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refillLocked(l.now())
	if l.tokens >= 1 {
		return 0
	}
	missing := 1 - l.tokens
	wait := time.Duration(missing / l.rate())
	if wait < 0 {
		wait = 0
	}
	return wait
}

// Wait blocks until a token is available or ctx is cancelled.
func (l *Limiter) Wait(ctx context.Context) error {
	for {
		l.mu.Lock()
		l.refillLocked(l.now())
		if l.tokens >= 1 {
			l.tokens--
			l.mu.Unlock()
			return nil
		}
		missing := 1 - l.tokens
		wait := time.Duration(missing / l.rate())
		if wait <= 0 {
			wait = time.Millisecond
		}
		l.mu.Unlock()

		if err := l.sleep(ctx, wait); err != nil {
			return fmt.Errorf("harness: limiter wait: %w", err)
		}
	}
}

// Tokens reports the current token count (mostly for tests and metrics).
func (l *Limiter) Tokens() float64 {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.refillLocked(l.now())
	return l.tokens
}

// Limited wraps h so every call first passes the semaphore and then the
// limiter. The semaphore slot is held for the whole call, which is what keeps
// the number of in-flight provider requests bounded.
func Limited(h Harness, sem *Semaphore, lim *Limiter) Harness {
	if h == nil {
		return HarnessFunc(func(context.Context, Request) (Response, error) {
			return Response{}, ErrNilHarness
		})
	}
	return HarnessFunc(func(ctx context.Context, req Request) (Response, error) {
		if sem != nil {
			if err := sem.Acquire(ctx); err != nil {
				return Response{}, err
			}
			defer sem.Release()
		}
		if lim != nil {
			if err := lim.Wait(ctx); err != nil {
				return Response{}, err
			}
		}
		return h.Complete(ctx, req)
	})
}
