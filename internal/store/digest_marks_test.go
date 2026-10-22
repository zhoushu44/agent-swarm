package store

import (
	"testing"

	"swarm-go/internal/server"
)

// TestDigestFiltersContextByClaimedResources is the incremental-loading
// guarantee: a worker only sees traces that can affect its own decisions.
func TestDigestFiltersContextByClaimedResources(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	// Two items naming two different files. B exists so the unrelated trace
	// has something to attach to.
	itA, err := st.CreateItem(server.CreateItemInput{Title: "src/a.js"})
	if err != nil {
		t.Fatalf("create A: %v", err)
	}
	if _, err := st.CreateItem(server.CreateItemInput{Title: "src/b.js"}); err != nil {
		t.Fatalf("create B: %v", err)
	}

	// w1 holds a claim on A only.
	if _, err := st.Claim(itA.ID, "w1"); err != nil {
		t.Fatalf("claim A: %v", err)
	}

	// Another peer leaves traces on BOTH files.
	for _, res := range []string{"src/a.js", "src/b.js"} {
		if _, err := st.AddMark(server.ContextEntry{
			Peer: "w2", Kind: "OBSERVATION", Text: "note on " + res,
			Resource: res, TS: base,
		}); err != nil {
			t.Fatalf("add mark %s: %v", res, err)
		}
	}

	d, err := st.Digest("w1", base-1)
	if err != nil {
		t.Fatalf("digest: %v", err)
	}
	if len(d.NewContext) != 1 {
		t.Fatalf("w1 should only see the trace on its own resource, got %d: %+v", len(d.NewContext), d.NewContext)
	}
	if d.NewContext[0].Resource != "src/a.js" {
		t.Fatalf("wrong resource surfaced: %q", d.NewContext[0].Resource)
	}
}

// TestDigestAlwaysIncludesOwnItemNotes: notes attached to the worker's own
// item are never filtered out, regardless of resource.
func TestDigestAlwaysIncludesOwnItemNotes(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	it, err := st.CreateItem(server.CreateItemInput{Title: "src/c.js"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if _, err := st.Claim(it.ID, "w1"); err != nil {
		t.Fatalf("claim: %v", err)
	}
	// A peer posts a FAIL attached to THIS item (no resource at all).
	if _, err := st.AddContext(server.ContextEntry{
		Peer: "w2", Kind: "FAIL", Text: "your approach hit a wall",
		ItemID: &it.ID, TS: base,
	}); err != nil {
		t.Fatalf("add context: %v", err)
	}

	d, err := st.Digest("w1", base-1)
	if err != nil {
		t.Fatalf("digest: %v", err)
	}
	if len(d.NewContext) != 1 || d.NewContext[0].Kind != "FAIL" {
		t.Fatalf("own-item notes must always surface, got %+v", d.NewContext)
	}
}

// TestDigestNoClaimsFallsBackToFullFeed: a freshly spawned worker with no
// claim still sees recent history rather than nothing.
func TestDigestNoClaimsFallsBackToFullFeed(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	for _, res := range []string{"src/x.js", "src/y.js"} {
		if _, err := st.AddContext(server.ContextEntry{
			Peer: "w2", Kind: "FACT", Text: "fact about " + res, TS: base,
		}); err != nil {
			t.Fatalf("add: %v", err)
		}
	}
	d, err := st.Digest("w9", base-1)
	if err != nil {
		t.Fatalf("digest: %v", err)
	}
	if len(d.NewContext) != 2 {
		t.Fatalf("no-claim worker should get the full feed, got %d", len(d.NewContext))
	}
}

// TestDigestSizeDropsWithScope is the token-savings assertion: the same event
// stream yields a far smaller brief once the worker has a scoped claim.
func TestDigestSizeDropsWithScope(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	it, err := st.CreateItem(server.CreateItemInput{Title: "src/target.js"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	// 40 unrelated traces + 1 relevant one.
	for i := 0; i < 40; i += 1 {
		if _, err := st.AddContext(server.ContextEntry{
			Peer: "w2", Kind: "FACT", Text: "unrelated noise", TS: base,
		}); err != nil {
			t.Fatalf("add noise: %v", err)
		}
	}
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w2", Kind: "ACTION", Text: "the one that matters",
		Resource: "src/target.js", TS: base,
	}); err != nil {
		t.Fatalf("add relevant: %v", err)
	}

	unscoped, err := st.Digest("w9", base-1)
	if err != nil {
		t.Fatalf("digest unscoped: %v", err)
	}
	if _, err := st.Claim(it.ID, "w1"); err != nil {
		t.Fatalf("claim: %v", err)
	}
	scoped, err := st.Digest("w1", base-1)
	if err != nil {
		t.Fatalf("digest scoped: %v", err)
	}
	if len(scoped.NewContext) >= len(unscoped.NewContext) {
		t.Fatalf("scoped brief (%d) should be smaller than unscoped (%d)",
			len(scoped.NewContext), len(unscoped.NewContext))
	}
	if len(scoped.NewContext) != 1 {
		t.Fatalf("scoped brief should carry exactly the relevant trace, got %d", len(scoped.NewContext))
	}
}
