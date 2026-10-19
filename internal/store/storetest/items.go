package storetest

import (
	"errors"
	"testing"

	"swarm-go/internal/server"
)

func runItemTests(t *testing.T, open OpenFunc) {
	t.Helper()

	t.Run("CreateAndGet", func(t *testing.T) {
		st := open(t)
		it := mustCreate(t, st, "write docs", "w1")
		if it.ID == "" {
			t.Fatal("CreateItem returned empty id")
		}
		if it.Status != "pending" {
			t.Fatalf("new item status = %q, want pending", it.Status)
		}
		if it.CreatedBy != "w1" {
			t.Fatalf("CreatedBy = %q, want w1", it.CreatedBy)
		}
		if it.Title != "write docs" {
			t.Fatalf("Title = %q", it.Title)
		}

		got, err := st.GetItem(it.ID)
		if err != nil {
			t.Fatalf("GetItem: %v", err)
		}
		if got.ID != it.ID || got.Title != it.Title || got.Status != it.Status {
			t.Fatalf("GetItem roundtrip mismatch: %+v vs %+v", got, it)
		}

		if _, err := st.GetItem("does-not-exist"); !errors.Is(err, server.ErrNotFound) {
			t.Fatalf("GetItem(missing) err = %v, want ErrNotFound", err)
		}
	})

	t.Run("CreateValidation", func(t *testing.T) {
		st := open(t)
		if _, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "   "}); !errors.Is(err, server.ErrInvalid) {
			t.Fatalf("CreateItem(empty title) err = %v, want ErrInvalid", err)
		}
		missing := "nope"
		if _, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "child", ParentID: &missing}); !errors.Is(err, server.ErrNotFound) {
			t.Fatalf("CreateItem(bad parent) err = %v, want ErrNotFound", err)
		}
	})

	t.Run("ParentChild", func(t *testing.T) {
		st := open(t)
		parent := mustCreate(t, st, "parent", "w1")
		child, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "child", ParentID: &parent.ID})
		if err != nil {
			t.Fatalf("CreateItem(child): %v", err)
		}
		if child.ParentID == nil || *child.ParentID != parent.ID {
			t.Fatalf("child.ParentID = %v, want %q", child.ParentID, parent.ID)
		}
	})

	t.Run("ListItems", func(t *testing.T) {
		st := open(t)
		mustCreate(t, st, "a", "w1")
		mustCreate(t, st, "b", "w1")
		mustCreate(t, st, "c", "w1")

		items, total, err := st.ListItems("", 10, 0)
		if err != nil {
			t.Fatalf("ListItems: %v", err)
		}
		if items == nil {
			t.Fatal("ListItems returned nil slice, want empty slice not nil")
		}
		if total != 3 || len(items) != 3 {
			t.Fatalf("ListItems = %d items / total %d, want 3/3", len(items), total)
		}

		page, total, err := st.ListItems("", 2, 1)
		if err != nil {
			t.Fatalf("ListItems page: %v", err)
		}
		if total != 3 || len(page) != 2 {
			t.Fatalf("ListItems page = %d items / total %d, want 2/3", len(page), total)
		}

		if _, _, err := st.ListItems("failed", 10, 0); err != nil {
			t.Fatalf("ListItems(failed): %v", err)
		}

		// A status filter must not leak items of other statuses.
		none, total, err := st.ListItems("done", 10, 0)
		if err != nil {
			t.Fatalf("ListItems(done): %v", err)
		}
		if none == nil {
			t.Fatal("ListItems(done) returned nil slice, want empty slice")
		}
		if len(none) != 0 || total != 0 {
			t.Fatalf("ListItems(done) = %d items / total %d, want 0/0", len(none), total)
		}
	})
}
