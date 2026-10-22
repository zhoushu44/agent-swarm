package store

import (
	"strings"
	"testing"
	"time"

	"swarm-go/internal/server"
)

// TestRunLogRoundTrip: the log is the durable record a later agent reads, so
// every field must survive the round trip and the filters must work.
func TestRunLogRoundTrip(t *testing.T) {
	st := openMarkStore(t)

	must := func(ev RunEvent) {
		t.Helper()
		if err := st.LogRunEvent(ev); err != nil {
			t.Fatalf("LogRunEvent(%s): %v", ev.Kind, err)
		}
	}
	must(RunEvent{Kind: EvRunStart, Actor: "system", Detail: "workers=4"})
	must(RunEvent{Kind: EvItemClaimed, Actor: "w1", ItemID: "t1", Title: "src/util.js#alpha", Tokens: 1200})
	must(RunEvent{Kind: EvMerge, Actor: "integrator", ItemID: "t1", Detail: "merged abc123 — 3 files", Files: 3})
	must(RunEvent{Kind: EvWorkerPark, Actor: "w2", Detail: "no new output"})
	must(RunEvent{Kind: EvItemDone, Actor: "w1", ItemID: "t1", Detail: "integrated and gated green"})

	all, err := st.RunLog("", 100)
	if err != nil {
		t.Fatalf("RunLog: %v", err)
	}
	if len(all) != 5 {
		t.Fatalf("want 5 events, got %d", len(all))
	}
	// newest first
	if all[0].Kind != EvItemDone {
		t.Fatalf("expected newest-first ordering, got %s", all[0].Kind)
	}

	// filter by kind
	merges, err := st.RunLog(EvMerge, 100)
	if err != nil {
		t.Fatalf("RunLog(MERGE): %v", err)
	}
	if len(merges) != 1 || merges[0].Files != 3 || merges[0].ItemID != "t1" {
		t.Fatalf("merge event lost its fields: %+v", mergges(merges))
	}

	// timeline for one item
	tl, err := st.ItemTimeline("t1")
	if err != nil {
		t.Fatalf("ItemTimeline: %v", err)
	}
	if len(tl) != 3 {
		t.Fatalf("want 3 events for t1, got %d", len(tl))
	}
	if tl[0].Kind != EvItemClaimed || tl[1].Kind != EvMerge || tl[2].Kind != EvItemDone {
		t.Fatalf("timeline out of order: %s %s %s", tl[0].Kind, tl[1].Kind, tl[2].Kind)
	}

	// empty item id is rejected rather than returning everything
	if _, err := st.ItemTimeline("  "); err == nil {
		t.Fatal("ItemTimeline must require an item id")
	}
}

// TestFormatEvent: the line must be readable without knowing the schema.
func TestFormatEvent(t *testing.T) {
	line := FormatEvent(RunEvent{
		Kind: EvMerge, Actor: "integrator", ItemID: "t1", Title: "src/util.js#alpha",
		Detail: "merged abc123 (conflict resolved by model) — 3 files",
		Files:  3, Tokens: 1500, Seconds: 42, TS: 0,
	}, 0)
	for _, want := range []string{"MERGE", "integrator", "t1", "src/util.js#alpha", "3 files", "1500 tok", "42s"} {
		if !strings.Contains(line, want) {
			t.Errorf("line missing %q:\n%s", want, line)
		}
	}
	// Relative time prefix when a start ts is supplied.
	rel := FormatEvent(RunEvent{Kind: EvItemDone, TS: 65000}, 5000)
	if !strings.HasPrefix(rel, "01:00 ") {
		t.Errorf("expected a MM:SS prefix, got %q", rel)
	}
}

// TestRunReportDistinguishesSeedFromBonus is the core reporting contract:
// the operator's work and the swarm's own work must never be conflated.
func TestRunReportDistinguishesSeedFromBonus(t *testing.T) {
	st := openMarkStore(t)

	seed := []string{"src/a.js", "src/b.js", "src/c.js"}
	for _, title := range seed {
		if _, err := st.CreateItem(server.CreateItemInput{Peer: "seed", Title: title, Kind: "task"}); err != nil {
			t.Fatalf("seed %s: %v", title, err)
		}
	}
	// bonus work by a worker
	for _, title := range []string{"src/d.js", "src/e.js"} {
		if _, err := st.CreateItem(server.CreateItemInput{Peer: "w1", Title: title, Kind: "task"}); err != nil {
			t.Fatalf("bonus %s: %v", title, err)
		}
	}
	// finish two seeds and one bonus
	for _, title := range []string{"src/a.js", "src/b.js", "src/d.js"} {
		items, _, err := st.ListItems("", 0, 0)
		if err != nil {
			t.Fatalf("list: %v", err)
		}
		for _, it := range items {
			if it.Title == title {
				if err := st.SetStatus(it.ID, StatusDone); err != nil {
					t.Fatalf("done %s: %v", title, err)
				}
			}
		}
	}

	open, total, err := st.SeedProgress()
	if err != nil {
		t.Fatalf("SeedProgress: %v", err)
	}
	if open != 1 || total != 3 {
		t.Fatalf("SeedProgress = (%d,%d), want (1,3)", open, total)
	}

	rep := st.BuildRunReport(time.Now(), "seed-done", 3, 1, []ItemSpend{{ItemID: "t1", Tokens: 4200}})
	if rep.SeedTotal != 3 || rep.SeedOpen != 1 || len(rep.SeedDone) != 2 {
		t.Fatalf("seed accounting wrong: total=%d open=%d done=%d", rep.SeedTotal, rep.SeedOpen, len(rep.SeedDone))
	}
	if rep.ExtraTotal != 2 || rep.ExtraDone != 1 || rep.ExtraOpen != 1 {
		t.Fatalf("bonus accounting wrong: total=%d done=%d open=%d", rep.ExtraTotal, rep.ExtraDone, rep.ExtraOpen)
	}
	if rep.Merged != 3 || rep.Rejected != 1 {
		t.Fatalf("integration accounting wrong: %+v", rep)
	}

	out := rep.Render()
	for _, want := range []string{"SEED WORK", "BONUS WORK", "src/a.js", "src/b.js", "STILL OPEN", "src/c.js"} {
		if !strings.Contains(out, want) {
			t.Errorf("report missing %q", want)
		}
	}
	// A stopped run must read as "resumable", not "lost".
	if !strings.Contains(rep.ResumeHint(), "resume") {
		t.Errorf("resume hint should say work resumes: %q", rep.ResumeHint())
	}
}

// TestReportWithNothingOpen: the no-op re-run case.
func TestReportWithNothingOpen(t *testing.T) {
	st := openMarkStore(t)
	if _, err := st.CreateItem(server.CreateItemInput{Peer: "seed", Title: "src/x.js", Kind: "task"}); err != nil {
		t.Fatal(err)
	}
	items, _, _ := st.ListItems("", 0, 0)
	if err := st.SetStatus(items[0].ID, StatusDone); err != nil {
		t.Fatal(err)
	}
	rep := st.BuildRunReport(time.Now(), "seed-done", 1, 0, nil)
	if len(rep.StillOpen) != 0 {
		t.Fatalf("nothing should be open, got %d", len(rep.StillOpen))
	}
	if !strings.Contains(rep.ResumeHint(), "no-op") {
		t.Errorf("all-done hint should mention no-op, got %q", rep.ResumeHint())
	}
}

func mergges(s []RunEvent) []RunEvent { return s }
