package storetest

import (
	"testing"

	"swarm-go/internal/server"
)

func runDigestTests(t *testing.T, open OpenFunc) {
	t.Helper()

	t.Run("Digest", func(t *testing.T) {
		st := open(t)
		pending := mustCreate(t, st, "pending task", "w1")
		claimed := mustCreate(t, st, "claimed task", "w1")
		if _, err := st.Claim(claimed.ID, "w2"); err != nil {
			t.Fatalf("Claim: %v", err)
		}

		if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "note one"}); err != nil {
			t.Fatalf("AddContext: %v", err)
		}
		if _, err := st.SendMessage(server.Message{From: "w1", To: "w2", Text: "ping"}); err != nil {
			t.Fatalf("SendMessage: %v", err)
		}

		// peer == "" asks for the unfiltered view, which every store must
		// support: the runner uses it to build a global board digest.
		d, err := st.Digest("", 0)
		if err != nil {
			t.Fatalf("Digest: %v", err)
		}
		if d.Stats.Total != 2 || d.Stats.Pending != 1 || d.Stats.InProgress != 1 {
			t.Fatalf("Digest stats = %+v, want total 2 / pending 1 / inProgress 1", d.Stats)
		}
		if d.PendingPreview == nil || d.NewContext == nil || d.Messages == nil || d.Claims == nil {
			t.Fatalf("Digest slices must never be nil (JSON must encode [] not null): %+v", d)
		}
		if len(d.PendingPreview) != 1 || d.PendingPreview[0].ID != pending.ID {
			t.Fatalf("PendingPreview = %+v, want just %s", d.PendingPreview, pending.ID)
		}
		if len(d.NewContext) != 1 {
			t.Fatalf("NewContext = %d entries, want 1", len(d.NewContext))
		}
		if len(d.Messages) != 1 || d.Messages[0].Text != "ping" {
			t.Fatalf("Messages = %+v, want the w2 message", d.Messages)
		}
		if len(d.Claims) != 1 || d.Claims[0].ItemID != claimed.ID {
			t.Fatalf("Claims = %+v, want the claim on %s", d.Claims, claimed.ID)
		}
	})

	t.Run("DigestSince", func(t *testing.T) {
		st := open(t)
		if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "old note"}); err != nil {
			t.Fatalf("AddContext: %v", err)
		}

		d, err := st.Digest("", 0)
		if err != nil {
			t.Fatalf("Digest: %v", err)
		}
		if len(d.NewContext) != 1 {
			t.Fatalf("NewContext = %d entries, want 1", len(d.NewContext))
		}

		// A watermark past every entry must yield an empty (not nil) list.
		watermark := d.NewContext[0].TS + 1
		later, err := st.Digest("", watermark)
		if err != nil {
			t.Fatalf("Digest(since): %v", err)
		}
		if later.NewContext == nil {
			t.Fatal("Digest(since).NewContext is nil, want empty slice")
		}
		if len(later.NewContext) != 0 {
			t.Fatalf("Digest(since=%d).NewContext = %d entries, want 0", watermark, len(later.NewContext))
		}

		// A new entry after the watermark shows up exactly once.
		if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "fresh note"}); err != nil {
			t.Fatalf("AddContext(fresh): %v", err)
		}
		fresh, err := st.Digest("", watermark)
		if err != nil {
			t.Fatalf("Digest(fresh): %v", err)
		}
		if len(fresh.NewContext) != 1 || fresh.NewContext[0].Text != "fresh note" {
			t.Fatalf("Digest(since).NewContext = %+v, want just the fresh note", fresh.NewContext)
		}
	})
}
