package store

// Run report + resume support.
//
// The operator needs two things after a run (or a 25-minute wall-clock cut):
//
//  1. WHAT HAPPENED — which seeded items finished, which the swarm invented
//     on its own, what is still open, and how much of the budget each item
//     burned. Without this a stopped run looks like lost work.
//
//  2. RESUME — enough state to continue without redoing finished items. The
//     items table already carries status + created_by, so "resume" is simply
//     "run again on the same repo/db": done items are not re-claimable, and
//     the integration branch already holds everything that passed the gate.
//
// This file turns that state into a printable report.

import (
	"fmt"
	"sort"
	"strings"
	"time"

	"swarm-go/internal/server"
)

// ItemSpend is one item's token cost, filled in by the runner (which owns the
// accounting) and passed into the report.
type ItemSpend struct {
	ItemID    string
	Tokens    int64
	APICalls  int
	TurnsHeld int
}

// RunReport is the printable end-of-run summary.
type RunReport struct {
	StartedAt   time.Time
	EndedAt     time.Time
	Wall        time.Duration
	StopReason  string // "seed-done", "wall-clock", "signal"
	SeedTotal   int
	SeedOpen    int
	ExtraTotal  int // worker-created + auto-split
	ExtraDone   int
	ExtraOpen   int
	Merged      int
	Rejected    int
	InputTokens  int64
	OutputTokens int64
	APICalls    int

	SeedDone   []server.Item
	SeedFailed []server.Item
	StillOpen  []server.Item
	TopSpend   []ItemSpend
}

// BuildRunReport assembles the report from the store plus the runner's
// counters. merged/rejected/spend come from the caller because they live in
// the runner and the submission ledger respectively.
func (s *Store) BuildRunReport(startedAt time.Time, stopReason string, merged, rejected int, spend []ItemSpend) RunReport {
	seedOpen, seedTotal, _ := s.SeedProgress()

	all := []server.Item{}
	if rows, _, err := s.ListItems("", 0, 0); err == nil {
		all = rows
	}
	rep := RunReport{
		StartedAt:  startedAt,
		EndedAt:    time.Now(),
		Wall:       time.Since(startedAt),
		StopReason: stopReason,
		SeedTotal:  seedTotal,
		SeedOpen:   seedOpen,
		Merged:     merged,
		Rejected:   rejected,
	}
	for _, it := range all {
		terminal := it.Status == StatusDone || it.Status == StatusFailed
		if it.CreatedBy == "seed" {
			switch {
			case it.Status == StatusDone:
				rep.SeedDone = append(rep.SeedDone, it)
			case it.Status == StatusFailed:
				rep.SeedFailed = append(rep.SeedFailed, it)
			}
			continue
		}
		rep.ExtraTotal++
		if terminal {
			rep.ExtraDone++
		} else {
			rep.ExtraOpen++
		}
	}
	rep.StillOpen, _ = s.OpenItemSummary()
	rep.TopSpend = rankSpend(spend, 8)
	return rep
}

// rankSpend sorts spend descending and caps it.
func rankSpend(spend []ItemSpend, limit int) []ItemSpend {
	out := append([]ItemSpend(nil), spend...)
	sort.Slice(out, func(i, j int) bool {
		if out[i].Tokens != out[j].Tokens {
			return out[i].Tokens > out[j].Tokens
		}
		return out[i].ItemID < out[j].ItemID
	})
	if len(out) > limit {
		out = out[:limit]
	}
	return out
}

// Render formats the report for the terminal. It is deliberately explicit
// about what was asked for versus what the swarm found on its own, and about
// what is unfinished — a stopped run must never look like a failed one.
func (r RunReport) Render() string {
	var b strings.Builder
	fmt.Fprintf(&b, "\n========== SWARM RUN REPORT ==========\n")
	fmt.Fprintf(&b, "wall:        %s\n", r.Wall.Round(time.Second))
	fmt.Fprintf(&b, "stopped by:  %s\n", r.StopReason)
	fmt.Fprintf(&b, "tokens:      %s in / %s out / %d api calls\n",
		humanTokens(r.InputTokens), humanTokens(r.OutputTokens), r.APICalls)
	fmt.Fprintf(&b, "integration: %d merged, %d rejected\n", r.Merged, r.Rejected)

	fmt.Fprintf(&b, "\n--- SEED WORK (what you asked for): %d/%d terminal ---\n", r.SeedTotal-r.SeedOpen, r.SeedTotal)
	for _, it := range r.SeedDone {
		fmt.Fprintf(&b, "  [done]    %s\n", it.Title)
	}
	for _, it := range r.SeedFailed {
		fmt.Fprintf(&b, "  [FAILED]  %s\n", it.Title)
	}
	if r.SeedOpen > 0 {
		fmt.Fprintf(&b, "  (%d seed item(s) still open — see below)\n", r.SeedOpen)
	}

	fmt.Fprintf(&b, "\n--- BONUS WORK (the swarm's own): %d done / %d open ---\n", r.ExtraDone, r.ExtraOpen)

	if len(r.StillOpen) > 0 {
		fmt.Fprintf(&b, "\n--- STILL OPEN (resume picks these up; nothing is lost) ---\n")
		for _, it := range r.StillOpen {
			who := it.CreatedBy
			if who == "" {
				who = "?"
			}
			fmt.Fprintf(&b, "  [%s] %s  (by %s)\n", it.Status, it.Title, who)
		}
	}

	if len(r.TopSpend) > 0 {
		fmt.Fprintf(&b, "\n--- TOP TOKEN SPEND ---\n")
		for _, sp := range r.TopSpend {
			fmt.Fprintf(&b, "  %-22s %s tokens / %d calls / %d turns\n",
				truncate(sp.ItemID, 22), humanTokens(sp.Tokens), sp.APICalls, sp.TurnsHeld)
		}
	}
	b.WriteString("======================================\n")
	return b.String()
}

// ResumeHint explains what re-running does, so an operator does not assume a
// stopped run must be redone from scratch.
func (r RunReport) ResumeHint() string {
	if len(r.StillOpen) == 0 {
		return "Nothing left open: re-running with the same seed would be a no-op (all items terminal)."
	}
	return fmt.Sprintf("Re-run the same command on the same repo: the %d done item(s) are not re-claimable, "+
		"the %d open item(s) resume, and everything already merged stays on the integration branch.",
		r.SeedTotal-r.SeedOpen+r.ExtraDone, len(r.StillOpen))
}

func humanTokens(n int64) string {
	switch {
	case n >= 1_000_000:
		return fmt.Sprintf("%.2fM", float64(n)/1_000_000)
	case n >= 1_000:
		return fmt.Sprintf("%.1fk", float64(n)/1_000)
	default:
		return fmt.Sprintf("%d", n)
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n-1] + "…"
}
