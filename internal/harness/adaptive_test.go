package harness

import (
	"context"
	"sync"
	"testing"
	"time"
)

func TestAdaptiveBaseline(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	if s.Limit() != 64 || s.Ceiling() != 64 {
		t.Fatalf("baseline: limit=%d ceiling=%d, want 64/64", s.Limit(), s.Ceiling())
	}
}

func TestAdaptiveSlowStartDoubles(t *testing.T) {
	s := NewAdaptiveSemaphore(8)
	// 8 OKs with no rate-limit → window doubles 8→16 (and ceiling with it).
	for i := 0; i < 8; i++ {
		s.NoteOK()
	}
	if s.Limit() != 16 || s.Ceiling() != 16 {
		t.Fatalf("after 8 OKs: limit=%d ceiling=%d, want 16/16 (slow start double)", s.Limit(), s.Ceiling())
	}
	for i := 0; i < 8; i++ {
		s.NoteOK()
	}
	if s.Limit() != 32 || s.Ceiling() != 32 {
		t.Fatalf("after 16 OKs: limit=%d ceiling=%d, want 32/32", s.Limit(), s.Ceiling())
	}
	// First rate-limit ends slow start; subsequent OKs revert to additive AIMD.
	s.NoteRateLimit()
	lim := s.Limit()
	for i := 0; i < 8; i++ {
		s.NoteOK()
	}
	if s.Limit() != lim+1 {
		t.Fatalf("post-rate-limit: limit=%d, want %d (AIMD +1, slow start OFF)", s.Limit(), lim+1)
	}
}

func TestAdaptiveRateLimitPinsWatermark(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	// concurrency climbed to 20 when the relay started 503ing
	for i := 0; i < 20; i++ {
		if err := s.Acquire(context.Background()); err != nil {
			t.Fatal(err)
		}
	}
	s.NoteRateLimit()
	if s.Ceiling() != 20 {
		t.Fatalf("ceiling=%d, want pinned at 20", s.Ceiling())
	}
	if s.Limit() != 15 { // floor(min(64,20)*0.75)
		t.Fatalf("limit=%d, want 15 (below capacity)", s.Limit())
	}
}

func TestAdaptiveRecoveryNotPinnedForever(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	for i := 0; i < 20; i++ {
		_ = s.Acquire(context.Background())
	}
	s.NoteRateLimit() // ceiling=20, limit=15
	// server scaled up: sustained success
	for i := 0; i < 500; i++ {
		s.NoteOK()
	}
	if s.Ceiling() <= 40 {
		t.Fatalf("ceiling=%d should climb well past pinned 20 after recovery", s.Ceiling())
	}
}

func TestAdaptiveStormThrottle(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	for i := 0; i < 20; i++ {
		_ = s.Acquire(context.Background())
	}
	s.NoteRateLimit() // first: ceiling=20, limit=15
	for i := 0; i < 30; i++ {
		s.NoteRateLimit() // same storm window (<2s): throttled
	}
	if s.Limit() != 15 {
		t.Fatalf("limit=%d, storm should not stack back-offs (want 15)", s.Limit())
	}
	if s.Ceiling() != 20 {
		t.Fatalf("ceiling=%d, storm should not drag watermark down", s.Ceiling())
	}
}

func TestAdaptiveSecondHitLowersWatermark(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	for i := 0; i < 20; i++ {
		_ = s.Acquire(context.Background())
	}
	s.NoteRateLimit() // ceiling=20, limit=15
	// concurrency drops 20→12 (releases), storm window passes, server rejects again
	for i := 0; i < 8; i++ {
		s.Release()
	}
	s.lastBack = time.Time{} // simulate the 2s storm window having passed
	s.NoteRateLimit()
	if s.Ceiling() != 12 {
		t.Fatalf("ceiling=%d, want second hit pinned at 12", s.Ceiling())
	}
	if s.Limit() != 9 { // floor(min(15,12)*0.75)
		t.Fatalf("limit=%d, want 9", s.Limit())
	}
}

func TestAdaptiveNeverCollapsesToZero(t *testing.T) {
	s := NewAdaptiveSemaphore(64)
	for i := 0; i < 40; i++ {
		s.lastBack = time.Time{} // bypass storm throttle for the test
		s.NoteRateLimit()
	}
	if s.Limit() != 1 || s.Ceiling() != 1 {
		t.Fatalf("limit=%d ceiling=%d, want converged to 1/1", s.Limit(), s.Ceiling())
	}
}

func TestAdaptiveAsyncConverges(t *testing.T) {
	s := NewAdaptiveSemaphore(32)
	var mu sync.Mutex
	inflight := 0
	round := func() {
		_ = s.Acquire(context.Background())
		mu.Lock()
		inflight++
		ok := inflight <= 8 // server capacity 8
		mu.Unlock()
		time.Sleep(2 * time.Millisecond) // request in flight — must NOT hold mu, or everything serializes
		mu.Lock()
		inflight--
		mu.Unlock()
		if ok {
			s.NoteOK()
		} else {
			s.NoteRateLimit()
		}
		s.Release()
	}
	for wave := 0; wave < 3; wave++ {
		var wg sync.WaitGroup
		for i := 0; i < 32; i++ {
			wg.Add(1)
			go func() { defer wg.Done(); round() }()
		}
		wg.Wait()
		time.Sleep(2100 * time.Millisecond) // cross the storm window per wave
	}
	if s.Limit() >= 32 {
		t.Fatalf("limit=%d should have learned the capacity constraint (<32)", s.Limit())
	}
	if s.Ceiling() >= 32 {
		t.Fatalf("ceiling=%d should stay below the initial cap 32", s.Ceiling())
	}
	if s.Limit() < 1 {
		t.Fatalf("limit=%d must not collapse below 1", s.Limit())
	}
}
