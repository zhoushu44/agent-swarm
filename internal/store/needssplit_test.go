package store

import (
	"testing"

	"swarm-go/internal/server"
)

// TestNeedsSplit: the flag that makes a worker decompose a big item on its
// first turn, so the operator does not have to write a detailed spec.
func TestNeedsSplit(t *testing.T) {
	st := openMarkStore(t)

	// A seeded top-level item needs splitting.
	seed, err := st.CreateItem(server.CreateItemInput{Peer: "seed", Title: "big thing", Kind: "task"})
	if err != nil {
		t.Fatalf("create seed: %v", err)
	}
	if ok, _ := st.NeedsSplit(seed.ID); !ok {
		t.Fatal("a fresh seeded top-level item must need splitting")
	}

	// Once it has a child, it does not (already decomposed).
	child, err := st.CreateItem(server.CreateItemInput{
		Peer: "w1", Title: "src/a.js", ParentID: &seed.ID, Kind: "task",
	})
	if err != nil {
		t.Fatalf("create child: %v", err)
	}
	_ = child
	if ok, _ := st.NeedsSplit(seed.ID); ok {
		t.Fatal("an item with children must not need splitting again")
	}

	// A child never needs splitting (it is already one file).
	if ok, _ := st.NeedsSplit(child.ID); ok {
		t.Fatal("a child item must not need splitting")
	}

	// Terminal items do not either.
	if err := st.SetStatus(seed.ID, StatusDone); err != nil {
		t.Fatal(err)
	}
	if ok, _ := st.NeedsSplit(seed.ID); ok {
		t.Fatal("a done item must not need splitting")
	}

	// Worker-created items are not the operator's spec: leave them alone.
	mine, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: "my own idea", Kind: "task"})
	if err != nil {
		t.Fatalf("create worker item: %v", err)
	}
	if ok, _ := st.NeedsSplit(mine.ID); ok {
		t.Fatal("a worker-created item must not be force-split")
	}

	// Unknown id reports an error rather than silently "no".
	if _, err := st.NeedsSplit("t_nope"); err == nil {
		t.Fatal("unknown item should error")
	}
}

// TestChildCount: the guard behind NeedsSplit.
func TestChildCount(t *testing.T) {
	st := openMarkStore(t)
	parent, err := st.CreateItem(server.CreateItemInput{Peer: "seed", Title: "parent", Kind: "task"})
	if err != nil {
		t.Fatal(err)
	}
	if n, _ := st.ChildCount(parent.ID); n != 0 {
		t.Fatalf("fresh parent has %d children, want 0", n)
	}
	for _, title := range []string{"src/a.js", "src/b.js"} {
		if _, err := st.CreateItem(server.CreateItemInput{
			Peer: "w1", Title: title, ParentID: &parent.ID, Kind: "task",
		}); err != nil {
			t.Fatal(err)
		}
	}
	if n, _ := st.ChildCount(parent.ID); n != 2 {
		t.Fatalf("parent has %d children, want 2", n)
	}
}
