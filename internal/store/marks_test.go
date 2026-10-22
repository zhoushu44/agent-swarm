package store

import (
	"testing"

	"swarm-go/internal/server"
)

// openMarkStore returns a store on a temp file DB plus a cleanup.
func openMarkStore(t *testing.T) *Store {
	t.Helper()
	st, err := Open(t.TempDir() + "/marks.db")
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { _ = st.Close() })
	return st
}

func strp(s string) *string { return &s }

// TestAddMarkRejectsUnknownKind: the guard layer must not silently accept a
// typo'd mark type — that would create a trace nobody can reason about.
func TestAddMarkRejectsUnknownKind(t *testing.T) {
	st := openMarkStore(t)
	if _, err := st.AddMark(server.ContextEntry{Kind: "INTENTX", Text: "x", Resource: "a.js"}); err == nil {
		t.Fatal("AddMark must reject an unknown mark type")
	}
	if _, err := st.AddMark(server.ContextEntry{Kind: "INTENT", Text: "", Resource: "a.js"}); err == nil {
		t.Fatal("AddMark must reject empty text")
	}
}

// TestIntentTTLExpiry: an intent is sticky only for its TTL.
func TestIntentTTLExpiry(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w1", Kind: "INTENT", Text: "editing pollVideo",
		Resource: "src/video.js#pollVideo", TTLMs: 1000, TS: base,
	}); err != nil {
		t.Fatalf("add mark: %v", err)
	}

	live, err := st.ActiveMarks("src/video.js#pollVideo", 10)
	if err != nil {
		t.Fatalf("active marks: %v", err)
	}
	if len(live) != 1 {
		t.Fatalf("intent should be live inside TTL, got %d", len(live))
	}

	// 1500ms later the TTL has elapsed.
	st.SetClock(func() int64 { return base + 1500 })
	live, err = st.ActiveMarks("src/video.js#pollVideo", 10)
	if err != nil {
		t.Fatalf("active marks: %v", err)
	}
	if len(live) != 0 {
		t.Fatalf("expired intent must be invisible, got %d", len(live))
	}
}

// TestActionsNeverExpire: ground truth outlives intents.
func TestActionsNeverExpire(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w1", Kind: "ACTION", Text: "added chat_ability", Resource: "src/video.js", TS: base,
	}); err != nil {
		t.Fatalf("add mark: %v", err)
	}
	st.SetClock(func() int64 { return base + 999999999 })
	live, err := st.ActiveMarks("src/video.js", 10)
	if err != nil {
		t.Fatalf("active marks: %v", err)
	}
	if len(live) != 1 {
		t.Fatalf("ACTION marks are permanent, got %d", len(live))
	}
}

// TestConflictsForExcludesSelfAndHonoursTTL is the core of the coordination
// guarantee: a worker never conflicts with itself, and a stale intent does not
// block anybody.
func TestConflictsForExcludesSelfAndHonoursTTL(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	must := func(e server.ContextEntry) {
		t.Helper()
		if _, err := st.AddMark(e); err != nil {
			t.Fatalf("add mark: %v", err)
		}
	}
	must(server.ContextEntry{Peer: "w2", Kind: "INTENT", Text: "editing fn A", Resource: "src/a.js#fnA", TTLMs: 600000, TS: base, ItemID: strp("t1")})
	must(server.ContextEntry{Peer: "w3", Kind: "INTENT", Text: "editing fn B", Resource: "src/a.js#fnB", TTLMs: 600000, TS: base})
	must(server.ContextEntry{Peer: "w2", Kind: "INTENT", Text: "editing fn C", Resource: "src/a.js#fnC", TTLMs: 600000, TS: base})

	// w1 asks about fnA: it must see w2's intent, never its own.
	c, err := st.ConflictsFor([]string{"src/a.js#fnA"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 1 {
		t.Fatalf("want 1 conflict on fnA, got %d", len(c))
	}
	if c[0].Peer != "w2" || c[0].ItemID != "t1" {
		t.Fatalf("unexpected conflict: %+v", c[0])
	}
	if c[0].RemainingMs <= 0 {
		t.Fatalf("remaining should be positive, got %d", c[0].RemainingMs)
	}

	// w2 asks about fnA: its own intent must not be reported back.
	c, err = st.ConflictsFor([]string{"src/a.js#fnA"}, "w2")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 0 {
		t.Fatalf("a peer must never conflict with itself, got %+v", c)
	}

	// Batch query returns both foreign intents.
	c, err = st.ConflictsFor([]string{"src/a.js#fnA", "src/a.js#fnB"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 2 {
		t.Fatalf("want 2 conflicts across two resources, got %d", len(c))
	}
}

// TestReleaseMarksDropsOnlyThatPeersIntents: releasing w2 must not disturb w3.
func TestReleaseMarksDropsOnlyThatPeersIntents(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	for _, p := range []string{"w2", "w3"} {
		if _, err := st.AddMark(server.ContextEntry{
			Peer: p, Kind: "INTENT", Text: "editing", Resource: "src/a.js#fn", TTLMs: 600000, TS: base,
		}); err != nil {
			t.Fatalf("add: %v", err)
		}
	}
	n, err := st.ReleaseMarks("w2")
	if err != nil {
		t.Fatalf("release marks: %v", err)
	}
	if n != 1 {
		t.Fatalf("want 1 released mark, got %d", n)
	}
	c, err := st.ConflictsFor([]string{"src/a.js#fn"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 1 || c[0].Peer != "w3" {
		t.Fatalf("only w3's intent should survive, got %+v", c)
	}
	// Idempotent: releasing again is a no-op, not an error.
	if n, err = st.ReleaseMarks("w2"); err != nil || n != 0 {
		t.Fatalf("second release = (%d,%v), want (0,nil)", n, err)
	}
}

// TestReapExpiredMarks: TTL-based cleanup independent of any claim.
func TestReapExpiredMarks(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w9", Kind: "INTENT", Text: "orphan", Resource: "src/x.js", TTLMs: 100, TS: base,
	}); err != nil {
		t.Fatalf("add: %v", err)
	}
	st.SetClock(func() int64 { return base + 500 })
	n, err := st.ReapExpiredMarks()
	if err != nil {
		t.Fatalf("reap: %v", err)
	}
	if n != 1 {
		t.Fatalf("want 1 reaped, got %d", n)
	}
	if c, _ := st.ConflictsFor([]string{"src/x.js"}, ""); len(c) != 0 {
		t.Fatalf("reaped intent must not conflict, got %+v", c)
	}
}

// TestMarksForFiltersByResource: the turn brief must be able to pull only the
// traces relevant to this worker's item.
func TestMarksForFiltersByResource(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	for _, r := range []string{"src/a.js", "src/b.js"} {
		if _, err := st.AddMark(server.ContextEntry{
			Peer: "w1", Kind: "OBSERVATION", Text: "note", Resource: r, TS: base,
		}); err != nil {
			t.Fatalf("add: %v", err)
		}
	}
	got, err := st.MarksFor([]string{"src/a.js"}, 10)
	if err != nil {
		t.Fatalf("marks for: %v", err)
	}
	if len(got) != 1 || got[0].Resource != "src/a.js" {
		t.Fatalf("want only src/a.js, got %+v", got)
	}
}

// TestClaimReturnsConflicts wires the whole loop: w2 claims an item naming a
// file, w1 claims an item naming the same file, and w1 is told about w2.
func TestClaimReturnsConflicts(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	seed := func(title string) server.Item {
		t.Helper()
		// Peer "" bypasses the dedup fold: both items legitimately name the
		// same symbol (one implements it, one tests it), which is exactly the
		// contention case the conflict scan must report.
		it, err := st.CreateItem(server.CreateItemInput{Title: title, Detail: "touch " + title})
		if err != nil {
			t.Fatalf("create item: %v", err)
		}
		return it
	}
	itemA := seed("implement src/shared.js#fnA")
	itemB := seed("test src/shared.js#fnA")

	resA, err := st.Claim(itemA.ID, "w2")
	if err != nil {
		t.Fatalf("claim A: %v", err)
	}
	// w2 declares its intent on the resource it is about to edit.
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w2", Kind: "INTENT", Text: "editing fnA",
		Resource: "src/shared.js#fna", ItemID: strp(itemA.ID), TTLMs: 600000, TS: base,
	}); err != nil {
		t.Fatalf("add mark: %v", err)
	}

	resB, err := st.Claim(itemB.ID, "w1")
	if err != nil {
		t.Fatalf("claim B: %v", err)
	}
	if len(resB.Conflicts) != 1 {
		t.Fatalf("w1 should be warned about w2's intent, got %+v", resB.Conflicts)
	}
	if resB.Conflicts[0].Peer != "w2" {
		t.Fatalf("conflict peer = %q, want w2", resB.Conflicts[0].Peer)
	}
	// Same symbol claimed by the same peer: no self-conflict on its own claim.
	if len(resA.Conflicts) != 0 {
		t.Fatalf("w2 must not conflict with itself, got %+v", resA.Conflicts)
	}

	// Releasing w2's claim clears its traces. Release B first (w1 still holds
	// it from the claim above), then re-claim B to observe a clean board.
	if err := st.Release(resB.Token); err != nil {
		t.Fatalf("release B: %v", err)
	}
	if err := st.Release(resA.Token); err != nil {
		t.Fatalf("release A: %v", err)
	}
	resB2, err := st.Claim(itemB.ID, "w1")
	if err != nil {
		t.Fatalf("re-claim B: %v", err)
	}
	if len(resB2.Conflicts) != 0 {
		t.Fatalf("after release there should be no conflict, got %+v", resB2.Conflicts)
	}
}

// TestSubmitClearsMarks: submitting hands work to the integrator, so the
// resources must stop looking contended.
func TestSubmitClearsMarks(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	it, err := st.CreateItem(server.CreateItemInput{Title: "src/y.js", Peer: "seed"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	res, err := st.Claim(it.ID, "w2")
	if err != nil {
		t.Fatalf("claim: %v", err)
	}
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w2", Kind: "INTENT", Text: "editing", Resource: "src/y.js", TTLMs: 600000, TS: base,
	}); err != nil {
		t.Fatalf("add mark: %v", err)
	}
	if _, err := st.Submit(res.Token, "w2", "abc123"); err != nil {
		t.Fatalf("submit: %v", err)
	}
	c, err := st.ConflictsFor([]string{"src/y.js"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 0 {
		t.Fatalf("submit must clear the peer's intents, got %+v", c)
	}
}

// TestOldRowsAndPlainContextStillWork: the migration is additive.
func TestOldRowsAndPlainContextStillWork(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	// A plain note (no mark columns) must round-trip.
	if _, err := st.AddContext(server.ContextEntry{Peer: "w1", Kind: "FACT", Text: "hello", TS: base}); err != nil {
		t.Fatalf("add context: %v", err)
	}
	got, err := st.SearchContext("hello", 10)
	if err != nil {
		t.Fatalf("search: %v", err)
	}
	if len(got) != 1 || got[0].Text != "hello" {
		t.Fatalf("plain context broken: %+v", got)
	}
	// And the mark scan over a table containing non-mark rows must not choke.
	live, err := st.ActiveMarks("", 10)
	if err != nil {
		t.Fatalf("active marks: %v", err)
	}
	if len(live) != 0 {
		t.Fatalf("non-mark rows must not surface as marks, got %+v", live)
	}
}

// TestConflictsAcrossGranularities is the containment rule: a file-level
// intent must warn a symbol-level claim, and vice versa. Exact-match IN (...)
// silently missed both, which is the whole point of symbol-level parallelism.
func TestConflictsAcrossGranularities(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })

	// w2 owns the WHOLE file.
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w2", Kind: "INTENT", Text: "rewriting the file",
		Resource: "src/shared.js", TTLMs: 600000, TS: base,
	}); err != nil {
		t.Fatalf("add: %v", err)
	}

	// A worker about to touch one symbol inside it must be warned.
	c, err := st.ConflictsFor([]string{"src/shared.js#fna"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c) != 1 || c[0].Peer != "w2" {
		t.Fatalf("file-level intent must warn the symbol worker, got %+v", c)
	}

	// And the reverse: a symbol-level intent warns a whole-file rewriter.
	st2 := openMarkStore(t)
	st2.SetClock(func() int64 { return base })
	if _, err := st2.AddMark(server.ContextEntry{
		Peer: "w3", Kind: "INTENT", Text: "editing one fn",
		Resource: "src/shared.js#fnb", TTLMs: 600000, TS: base,
	}); err != nil {
		t.Fatalf("add: %v", err)
	}
	c2, err := st2.ConflictsFor([]string{"src/shared.js"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c2) != 1 || c2[0].Peer != "w3" {
		t.Fatalf("symbol intent must warn the file rewriter, got %+v", c2)
	}

	// Unrelated resources must NOT match (no false positives from prefixing).
	c3, err := st.ConflictsFor([]string{"src/other.js", "src/shared.jsx"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c3) != 0 {
		t.Fatalf("unrelated resources must not conflict, got %+v", c3)
	}

	// A sibling symbol in the SAME file is a different resource and must not
	// be reported: that is what makes symbol-level parallelism work.
	st3 := openMarkStore(t)
	st3.SetClock(func() int64 { return base })
	if _, err := st3.AddMark(server.ContextEntry{
		Peer: "w4", Kind: "INTENT", Text: "editing fna",
		Resource: "src/shared.js#fna", TTLMs: 600000, TS: base,
	}); err != nil {
		t.Fatalf("add: %v", err)
	}
	c4, err := st3.ConflictsFor([]string{"src/shared.js#fnb"}, "w1")
	if err != nil {
		t.Fatalf("conflicts: %v", err)
	}
	if len(c4) != 0 {
		t.Fatalf("sibling symbols must not conflict, got %+v", c4)
	}
}

// TestMarksForContainment: sniffing a whole file must also surface symbol
// marks inside it.
func TestMarksForContainment(t *testing.T) {
	st := openMarkStore(t)
	base := st.clock()
	st.SetClock(func() int64 { return base })
	if _, err := st.AddMark(server.ContextEntry{
		Peer: "w2", Kind: "WARNING", Text: "this fn is stale",
		Resource: "src/shared.js#fna", TS: base,
	}); err != nil {
		t.Fatalf("add: %v", err)
	}
	got, err := st.MarksFor([]string{"src/shared.js"}, 10)
	if err != nil {
		t.Fatalf("marks for: %v", err)
	}
	if len(got) != 1 || got[0].Resource != "src/shared.js#fna" {
		t.Fatalf("file query must surface the symbol mark, got %+v", got)
	}
}

// TestItemResourcesExtractsPaths: the conflict scan is only as good as the
// resource extraction.
func TestItemResourcesExtractsPaths(t *testing.T) {
	got := itemResources(server.Item{
		Title:  "create src/openai/video.js with tests",
		Detail: "see docs/dola-os-research.md and src/dola/client.js",
	})
	want := map[string]bool{
		"src/openai/video.js": true,
		"docs/dola-os-research.md": true,
		"src/dola/client.js": true,
	}
	if len(got) != len(want) {
		t.Fatalf("resources = %v, want keys of %v", got, want)
	}
	for _, r := range got {
		if !want[r] {
			t.Fatalf("unexpected resource %q in %v", r, got)
		}
	}
}
