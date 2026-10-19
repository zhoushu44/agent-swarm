package storetest

import (
	"errors"
	"sync"
	"testing"

	"swarm-go/internal/server"
)

func runClaimTests(t *testing.T, open OpenFunc) {
	t.Helper()

	t.Run("ClaimLifecycle", func(t *testing.T) {
		st := open(t)
		it := mustCreate(t, st, "task", "w1")

		res, err := st.Claim(it.ID, "w2")
		if err != nil {
			t.Fatalf("Claim: %v", err)
		}
		if res.Token == "" {
			t.Fatal("Claim returned empty token")
		}
		if res.Item.ID != it.ID {
			t.Fatalf("Claim item id = %q, want %q", res.Item.ID, it.ID)
		}
		if res.Item.Status != "in_progress" {
			t.Fatalf("claimed item status = %q, want in_progress", res.Item.Status)
		}
		if res.History == nil {
			t.Fatal("Claim history is nil, want empty slice")
		}

		if _, err := st.Claim(it.ID, "w3"); !errors.Is(err, server.ErrConflict) {
			t.Fatalf("double Claim err = %v, want ErrConflict", err)
		}
		if _, err := st.Claim("missing", "w3"); !errors.Is(err, server.ErrNotFound) {
			t.Fatalf("Claim(missing) err = %v, want ErrNotFound", err)
		}

		if err := st.Renew(res.Token); err != nil {
			t.Fatalf("Renew: %v", err)
		}
		if err := st.Renew("bogus"); !errors.Is(err, server.ErrNotFound) {
			t.Fatalf("Renew(bogus) err = %v, want ErrNotFound", err)
		}

		if err := st.Release(res.Token); err != nil {
			t.Fatalf("Release: %v", err)
		}
		if err := st.Release(res.Token); err == nil {
			t.Fatal("double Release succeeded, want error")
		}
		after, err := st.GetItem(it.ID)
		if err != nil {
			t.Fatalf("GetItem after release: %v", err)
		}
		if after.Status != "pending" {
			t.Fatalf("status after release = %q, want pending", after.Status)
		}
	})

	t.Run("Submit", func(t *testing.T) {
		st := open(t)
		it := mustCreate(t, st, "task", "w1")
		res, err := st.Claim(it.ID, "w2")
		if err != nil {
			t.Fatalf("Claim: %v", err)
		}

		if _, err := st.Submit(res.Token, "w2", "  "); !errors.Is(err, server.ErrInvalid) {
			t.Fatalf("Submit(empty sha) err = %v, want ErrInvalid", err)
		}

		done, err := st.Submit(res.Token, "w2", "abc1234")
		if err != nil {
			t.Fatalf("Submit: %v", err)
		}
		if done.Status != "submitted" {
			t.Fatalf("status after submit = %q, want submitted", done.Status)
		}
		if _, err := st.Submit(res.Token, "w2", "abc1234"); err == nil {
			t.Fatal("replayed Submit succeeded, want error (token consumed)")
		}

		// A submitted item can no longer be claimed.
		if _, err := st.Claim(it.ID, "w3"); !errors.Is(err, server.ErrConflict) {
			t.Fatalf("Claim(submitted item) err = %v, want ErrConflict", err)
		}
	})

	// Concurrent claims on the same item must yield exactly one winner.
	t.Run("ConcurrentClaim", func(t *testing.T) {
		st := open(t)
		it := mustCreate(t, st, "contended", "w1")

		const racers = 8
		var wg sync.WaitGroup
		var mu sync.Mutex
		wins := 0
		conflicts := 0
		for i := 0; i < racers; i++ {
			wg.Add(1)
			go func(n int) {
				defer wg.Done()
				_, err := st.Claim(it.ID, "peer")
				mu.Lock()
				defer mu.Unlock()
				switch {
				case err == nil:
					wins++
				case errors.Is(err, server.ErrConflict):
					conflicts++
				default:
					t.Errorf("unexpected Claim error: %v", err)
				}
			}(i)
		}
		wg.Wait()

		if wins != 1 {
			t.Fatalf("concurrent claims: %d winners, want exactly 1", wins)
		}
		if conflicts != racers-1 {
			t.Fatalf("concurrent claims: %d conflicts, want %d", conflicts, racers-1)
		}
	})
}
