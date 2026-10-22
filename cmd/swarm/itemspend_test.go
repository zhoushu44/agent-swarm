package main

import (
	"testing"
)

// newTestRunner builds the minimal runner shape these ledger tests need. The
// real runner is built by newSwarmRunner with a live store and repo; the
// accounting paths under test only touch mu + the two maps.
func newTestRunner(t *testing.T) *swarmRunner {
	t.Helper()
	return &swarmRunner{
		workers:    make(map[string]*workerState),
		itemHoldBy: make(map[string]string),
		itemSpend:  make(map[string]int64),
		stopCh:     make(chan struct{}),
		submitCh:   make(chan struct{}, 1),
	}
}

// TestItemTokensStartsEmpty: the per-item ledger must begin at zero, not at
// some sentinel that makes the breaker look already-fired.
func TestItemTokensStartsEmpty(t *testing.T) {
	r := newTestRunner(t)
	if got := r.itemTokens("it_nope"); got != 0 {
		t.Fatalf("unknown item should cost 0, got %d", got)
	}
}

// TestItemSpendAccumulates: spend attributed to an item must survive the
// worker accounting path (this is what makes maxItemTokens meaningful).
func TestItemSpendAccumulates(t *testing.T) {
	r := newTestRunner(t)
	r.mu.Lock()
	r.itemSpend["it_1"] += 1200
	r.itemSpend["it_1"] += 800
	r.mu.Unlock()

	if got := r.itemTokens("it_1"); got != 2000 {
		t.Fatalf("item_1 spend = %d, want 2000", got)
	}
	// Snapshot must be a copy: mutating it must not affect the ledger.
	snap := r.itemSpendSnapshot()
	snap["it_1"] = 999999
	if got := r.itemTokens("it_1"); got != 2000 {
		t.Fatalf("snapshot leaked into the ledger: %d", got)
	}
	if len(snap) != 1 {
		t.Fatalf("snapshot has %d entries, want 1", len(snap))
	}
}

// TestItemHoldByTracking: the progress line needs to know which worker holds
// which item to report a stall instead of a bare "inflight" number.
func TestItemHoldByTracking(t *testing.T) {
	r := newTestRunner(t)
	r.mu.Lock()
	r.itemHoldBy["w1"] = "it_1"
	r.itemHoldBy["w2"] = "it_2"
	r.mu.Unlock()

	r.mu.Lock()
	held, ok := r.itemHoldBy["w1"]
	r.mu.Unlock()
	if !ok || held != "it_1" {
		t.Fatalf("itemHoldBy[w1] = (%q,%v), want (it_1,true)", held, ok)
	}

	r.mu.Lock()
	delete(r.itemHoldBy, "w1")
	r.mu.Unlock()
	if _, ok := r.itemHoldBy["w1"]; ok {
		t.Fatal("delete must remove the hold")
	}
}
