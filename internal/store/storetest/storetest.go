// Package storetest is a reusable conformance suite for any implementation of
// the server.Store persistence contract.
//
// The SQLite-backed store in package store and the in-memory MemStore in
// package server must agree on the invariants the HTTP layer relies on
// (claim/release/renew/submit lifecycle, context history, digest shape).
// Rather than duplicate those tests, an implementation just calls:
//
//	func TestStoreConformance(t *testing.T) {
//		storetest.Run(t, func(tb testing.TB) server.Store {
//			s, err := store.Open(":memory:")
//			if err != nil {
//				tb.Fatalf("open: %v", err)
//			}
//			tb.Cleanup(func() { _ = s.Close() })
//			return s
//		})
//	}
//
// Contract an implementation must honour:
//   - missing item/token -> error wrapping server.ErrNotFound
//   - state violation (claiming a non-pending item, double claim, replayed
//     submit) -> error wrapping server.ErrConflict
//   - bad input (empty title/text/commitSha) -> error wrapping server.ErrInvalid
//   - CreateItem starts items in status "pending" and stamps CreatedBy with the
//     requesting peer
//   - Claim moves the item to "in_progress" and returns a non-empty token
//   - Release returns the item to "pending"; Renew keeps the claim alive
//   - Submit moves the item to "submitted" and consumes the token
//   - AddContext/SendMessage return the persisted record with ID and TS set
//   - Digest/ListItems/SearchContext/ItemHistory lists are never nil
package storetest

import (
	"testing"

	"swarm-go/internal/server"
)

// OpenFunc returns a fresh, empty Store for one test case.
// Implementations must be safe for concurrent use.
type OpenFunc func(tb testing.TB) server.Store

// Run executes the whole conformance suite against open.
func Run(t *testing.T, open OpenFunc) {
	t.Helper()
	runItemTests(t, open)
	runClaimTests(t, open)
	runContextTests(t, open)
	runDigestTests(t, open)
}

func mustCreate(t *testing.T, st server.Store, title, peer string) server.Item {
	t.Helper()
	it, err := st.CreateItem(server.CreateItemInput{Peer: peer, Title: title})
	if err != nil {
		t.Fatalf("CreateItem(%q): %v", title, err)
	}
	return it
}
