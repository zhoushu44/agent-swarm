package storetest_test

import (
	"sync/atomic"
	"testing"

	"swarm-go/internal/server"
	"swarm-go/internal/store/storetest"
)

// TestMemStoreConformance proves the suite itself is sound by running it
// against the in-memory reference store that ships with package server.
// The SQLite store must pass exactly the same suite.
func TestMemStoreConformance(t *testing.T) {
	storetest.Run(t, func(tb testing.TB) server.Store {
		tb.Helper()
		var tick int64 = 1_000_000
		clock := func() int64 { return atomic.AddInt64(&tick, 1) }
		return server.NewMemStore(clock)
	})
}
