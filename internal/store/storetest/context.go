package storetest

import (
	"errors"
	"testing"

	"swarm-go/internal/server"
)

func runContextTests(t *testing.T, open OpenFunc) {
	t.Helper()

	t.Run("Context", func(t *testing.T) {
		st := open(t)
		it := mustCreate(t, st, "task", "w1")
		itemID := it.ID

		if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "  ", ItemID: &itemID}); !errors.Is(err, server.ErrInvalid) {
			t.Fatalf("AddContext(empty text) err = %v, want ErrInvalid", err)
		}

		e1, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "sqlite driver is modernc", ItemID: &itemID})
		if err != nil {
			t.Fatalf("AddContext: %v", err)
		}
		if e1.ID == 0 {
			t.Fatal("AddContext did not assign an ID")
		}
		if e1.TS == 0 {
			t.Fatal("AddContext did not assign a timestamp")
		}

		if _, err := st.AddContext(server.ContextEntry{Peer: "w9", Kind: "FAIL", Text: "cgo sqlite cannot build here"}); err != nil {
			t.Fatalf("AddContext(no item): %v", err)
		}

		hist, err := st.ItemHistory(itemID)
		if err != nil {
			t.Fatalf("ItemHistory: %v", err)
		}
		if len(hist) != 1 || hist[0].Text != e1.Text {
			t.Fatalf("ItemHistory = %+v, want just the item-scoped entry", hist)
		}

		found, err := st.SearchContext("modernc", 10)
		if err != nil {
			t.Fatalf("SearchContext: %v", err)
		}
		if len(found) != 1 || found[0].Text != e1.Text {
			t.Fatalf("SearchContext = %+v, want 1 hit", found)
		}

		none, err := st.SearchContext("zzz-no-such-text", 10)
		if err != nil {
			t.Fatalf("SearchContext(no hit): %v", err)
		}
		if none == nil {
			t.Fatal("SearchContext returned nil slice, want empty slice")
		}
		if len(none) != 0 {
			t.Fatalf("SearchContext(no hit) = %d entries, want 0", len(none))
		}

		upper, err := st.SearchContext("MODERNC", 10)
		if err != nil {
			t.Fatalf("SearchContext(upper): %v", err)
		}
		if len(upper) != 1 {
			t.Fatalf("SearchContext is not case-insensitive: %d hits", len(upper))
		}

		if _, err := st.ItemHistory("missing"); err != nil {
			t.Fatalf("ItemHistory(missing) should be empty, not an error: %v", err)
		}

		// Search limit must be honoured when there are more hits than asked for.
		for i := 0; i < 5; i++ {
			if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "OBSERVED", Text: "needle entry"}); err != nil {
				t.Fatalf("AddContext(needle): %v", err)
			}
		}
		limited, err := st.SearchContext("needle", 2)
		if err != nil {
			t.Fatalf("SearchContext(needle): %v", err)
		}
		if len(limited) != 2 {
			t.Fatalf("SearchContext limit = %d entries, want 2", len(limited))
		}
	})

	t.Run("Messages", func(t *testing.T) {
		st := open(t)
		if _, err := st.SendMessage(server.Message{From: "w1", To: "", Text: "hi"}); !errors.Is(err, server.ErrInvalid) {
			t.Fatalf("SendMessage(no recipient) err = %v, want ErrInvalid", err)
		}
		if _, err := st.SendMessage(server.Message{From: "w1", To: "w2", Text: "  "}); !errors.Is(err, server.ErrInvalid) {
			t.Fatalf("SendMessage(empty text) err = %v, want ErrInvalid", err)
		}

		m, err := st.SendMessage(server.Message{From: "w1", To: "w2", Text: "ping"})
		if err != nil {
			t.Fatalf("SendMessage: %v", err)
		}
		if m.ID == 0 {
			t.Fatal("SendMessage did not assign an ID")
		}
		if m.TS == 0 {
			t.Fatal("SendMessage did not assign a timestamp")
		}
		if _, err := st.SendMessage(server.Message{From: "w1", To: "w3", Text: "other"}); err != nil {
			t.Fatalf("SendMessage(w3): %v", err)
		}

		inbox, err := st.Messages("w2", 0)
		if err != nil {
			t.Fatalf("Messages: %v", err)
		}
		if inbox == nil {
			t.Fatal("Messages returned nil slice, want empty slice")
		}
		if len(inbox) != 1 || inbox[0].Text != "ping" {
			t.Fatalf("Messages(w2) = %+v, want only the ping", inbox)
		}

		empty, err := st.Messages("w99", 0)
		if err != nil {
			t.Fatalf("Messages(no inbox): %v", err)
		}
		if len(empty) != 0 {
			t.Fatalf("Messages(w99) = %d messages, want 0", len(empty))
		}
	})
}
