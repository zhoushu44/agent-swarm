package harness

import (
	"context"
	"fmt"
)

// Semaphore is a counting semaphore that also honours context cancellation, so
// a worker whose turn was cancelled while waiting for an LLM slot unblocks
// immediately instead of pinning a goroutine.
//
// The zero value is NOT usable; build one with NewSemaphore. A Semaphore is
// safe for concurrent use.
type Semaphore struct {
	slots chan struct{}
	cap   int
}

// NewSemaphore returns a Semaphore allowing at most n concurrent holders.
// n <= 0 is clamped to 1 so a misconfigured fleet still makes progress.
func NewSemaphore(n int) *Semaphore {
	if n <= 0 {
		n = 1
	}
	return &Semaphore{slots: make(chan struct{}, n), cap: n}
}

// Cap returns the maximum number of concurrent holders.
func (s *Semaphore) Cap() int { return s.cap }

// Len returns how many slots are currently held.
func (s *Semaphore) Len() int { return len(s.slots) }

// Acquire blocks until a slot is free or ctx is cancelled.
func (s *Semaphore) Acquire(ctx context.Context) error {
	select {
	case s.slots <- struct{}{}:
		return nil
	default:
	}
	select {
	case s.slots <- struct{}{}:
		return nil
	case <-ctx.Done():
		return fmt.Errorf("harness: acquire semaphore: %w", ctx.Err())
	}
}

// TryAcquire takes a slot without blocking. It reports false when every slot is
// busy.
func (s *Semaphore) TryAcquire() bool {
	select {
	case s.slots <- struct{}{}:
		return true
	default:
		return false
	}
}

// Release frees one slot. Releasing an unheld Semaphore panics, which surfaces
// double-release bugs immediately instead of silently over-admitting work.
func (s *Semaphore) Release() {
	select {
	case <-s.slots:
	default:
		panic("harness: semaphore released more times than acquired")
	}
}

// Do acquires a slot, runs fn and releases the slot even when fn panics.
func (s *Semaphore) Do(ctx context.Context, fn func() error) error {
	if err := s.Acquire(ctx); err != nil {
		return err
	}
	defer s.Release()
	return fn()
}
