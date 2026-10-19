package harness

import (
	"context"
	"fmt"
	"sync"
	"time"
)

// AdaptiveSemaphore is a counting semaphore with AIMD flow control, the Go
// counterpart of the JS apiGate. Behaviour mirrors src/harness.js:
//
//   - rate-limit (429/53): pin the ceiling to the peak concurrency seen in the
//     current round (the level at which the server started rejecting IS its
//     real capacity), then back off multiplicatively (0.75x) BELOW that level.
//   - sustained success: additively raise the limit by 1 every 8 OKs, and once
//     at the ceiling keep probing upward (+1, accelerating after long healthy
//     streaks) so a recovered/upgraded server is not pinned forever.
//   - 503 storm throttling: concurrent rejections within a 2s window only
//     trigger ONE back-off, otherwise N simultaneous failures would multiply
//     the limit down to 1.
//
// The zero value is NOT usable; build one with NewAdaptiveSemaphore. It is
// safe for concurrent use.
type AdaptiveSemaphore struct {
	mu       sync.Mutex
	limit    int
	ceiling  int
	active   int
	hiWater  int       // peak concurrency since last pin (rejections lag releases)
	okStreak int       // consecutive successes since last rate-limit
	lastBack time.Time // storm throttle: only first 503 in a window backs off

	waitq []chan struct{}
}

// NewAdaptiveSemaphore returns an AdaptiveSemaphore starting at limit n (n<=0
// is clamped to 1 so a misconfigured fleet still makes progress).
func NewAdaptiveSemaphore(n int) *AdaptiveSemaphore {
	if n <= 0 {
		n = 1
	}
	return &AdaptiveSemaphore{limit: n, ceiling: n}
}

// Limit reports the current concurrency limit (the AIMD-controlled value).
func (s *AdaptiveSemaphore) Limit() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.limit
}

// Ceiling reports the current capacity estimate.
func (s *AdaptiveSemaphore) Ceiling() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.ceiling
}

// Acquire blocks until a slot is free or ctx is cancelled.
func (s *AdaptiveSemaphore) Acquire(ctx context.Context) error {
	s.mu.Lock()
	if s.active < s.limit {
		s.active++
		if s.active > s.hiWater {
			s.hiWater = s.active
		}
		s.mu.Unlock()
		return nil
	}
	ch := make(chan struct{})
	s.waitq = append(s.waitq, ch)
	s.mu.Unlock()

	select {
	case <-ch:
		// slot handed to us by Release / SetLimit; active already incremented there
		return nil
	case <-ctx.Done():
		// drop ourselves from the queue if still queued
		s.mu.Lock()
		for i, c := range s.waitq {
			if c == ch {
				s.waitq = append(s.waitq[:i], s.waitq[i+1:]...)
				break
			}
		}
		s.mu.Unlock()
		return fmt.Errorf("harness: acquire adaptive semaphore: %w", ctx.Err())
	}
}

// Release frees one slot and hands it to the next waiter.
func (s *AdaptiveSemaphore) Release() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.active > 0 {
		s.active--
	}
	for len(s.waitq) > 0 && s.active < s.limit {
		ch := s.waitq[0]
		s.waitq = s.waitq[1:]
		s.active++
		if s.active > s.hiWater {
			s.hiWater = s.active
		}
		close(ch)
	}
}

// SetLimit explicitly sets the limit (e.g. CLI --max-inflight). Explicit raises
// are allowed to raise the ceiling too; internal AIMD back-offs are not (they
// go through backOffLocked so a pin cannot be immediately undone).
func (s *AdaptiveSemaphore) SetLimit(n int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.setLimitLocked(n, false)
}

func (s *AdaptiveSemaphore) setLimitLocked(n int, aimd bool) {
	if n < 1 {
		n = 1
	}
	if n > s.ceiling && !aimd {
		s.ceiling = n
	}
	s.limit = n
	for len(s.waitq) > 0 && s.active < s.limit {
		ch := s.waitq[0]
		s.waitq = s.waitq[1:]
		s.active++
		if s.active > s.hiWater {
			s.hiWater = s.active
		}
		close(ch)
	}
}

// NoteRateLimit records a rate-limit (429/503) event: pins the ceiling at the
// peak concurrency of the round and backs off below it. Concurrent failures
// inside a 2s storm window are throttled to a single back-off.
func (s *AdaptiveSemaphore) NoteRateLimit() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.limit <= 0 {
		return
	}
	s.okStreak = 0
	now := time.Now()
	if !s.lastBack.IsZero() && now.Sub(s.lastBack) < 2*time.Second {
		return // 503 storm: first rejection already backed off
	}
	s.lastBack = now

	hit := s.hiWater
	if hit < 1 {
		hit = s.active
	}
	if hit < 1 {
		hit = 1
	}
	if hit < s.ceiling {
		s.ceiling = hit
	}
	s.hiWater = 0 // consumed: next round re-accumulates the true peak

	base := s.limit
	if s.ceiling < base {
		base = s.ceiling
	}
	backed := base * 3 / 4 // 0.75x below the capacity watermark
	if backed < 1 {
		backed = 1
	}
	s.setLimitLocked(backed, true)
}

// NoteOK records a successful request: additive recovery (+1 every 8 OKs)
// inside the pinned capacity, then accelerated ceiling probing once at the
// ceiling so a recovered server is not pinned forever.
func (s *AdaptiveSemaphore) NoteOK() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.limit <= 0 {
		return
	}
	s.okStreak++
	if s.okStreak%8 != 0 {
		return
	}
	if s.limit < s.ceiling {
		s.setLimitLocked(s.limit+1, false)
		return
	}
	// At the ceiling with sustained success: probe upward. Step grows with
	// streak (>=64 → +2, >=256 → +4); any new rate-limit resets to +1.
	probe := 1
	if s.okStreak >= 64 {
		probe = 2
	}
	if s.okStreak >= 256 {
		probe = 4
	}
	s.ceiling += probe
	s.setLimitLocked(s.limit+1, false)
}

// Do acquires a slot, runs fn and releases the slot even when fn panics.
func (s *AdaptiveSemaphore) Do(ctx context.Context, fn func() error) error {
	if err := s.Acquire(ctx); err != nil {
		return err
	}
	defer s.Release()
	return fn()
}
