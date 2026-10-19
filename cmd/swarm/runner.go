package main

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"swarm-go/internal/gitutil"
	"swarm-go/internal/harness"
	"swarm-go/internal/prompts"
	"swarm-go/internal/server"
	"swarm-go/internal/store"
	"swarm-go/internal/sysload"
)

// runConfig holds the CLI knobs for cmdRun.
type runConfig struct {
	maxWorkers      int
	minWorkers      int
	initialWorkers  int
	maxTurns        int
	hardTurnFactor  int
	idleTurnsToExit int
	leaseSec        int
	maxItems        int
	maxItemTurns    int
	maxItemTokens   int
	budgetTokens    int64
	maxItemFails    int
	testCmd         string
	mission         string
	conventions     string
	serverPort      int
	harnessName     string // "echo" for offline tests, "direct" for OpenAI-compatible
	apiBase         string
	apiKey          string
	model           string
}

func defaultRunConfig() runConfig {
	return runConfig{
		maxWorkers:      1000,
		minWorkers:      1,
		initialWorkers:  8,
		maxTurns:        10,
		hardTurnFactor:  6,
		idleTurnsToExit: 2,
		leaseSec:        300,
		maxItems:        200,
		maxItemTurns:    6,
		maxItemTokens:   2_000_000,
		maxItemFails:    3,
		serverPort:      17771,
		harnessName:     "direct",
	}
}

// swarmRunner is the Go port of the JS Runner. It owns the worker pool,
// autoscaler, integrator, lease renewer and reaper.
type swarmRunner struct {
	cfg    runConfig
	dir    string
	repo   *gitutil.Repo
	store  *store.Store
	gate   *harness.AdaptiveSemaphore
	guard  *sysload.Guard
	harness harness.Harness

	mu       sync.Mutex
	workers  map[string]*workerState
	stopped  bool
	stopCh   chan struct{}
	wg       sync.WaitGroup
	submitCh chan struct{} // event-driven integrator wake

	// token accounting
	tokensSpent int64
	budgetExceeded bool
}

type workerState struct {
	name      string
	dir       string
	branch    string
	status    string // working / idle / retired / crashed
	turn      int
	idleCount int
	syncSkew  int
	redoCount int
	lastReply string
	lastDigestTs int64
	itemHold  *itemHold
	rateLimitedStreak int
	loopDone  bool
	reapFailCount int
	reapFailAt    time.Time
	tokenUsage workerUsage
}

type itemHold struct {
	itemID string
	turns  int
	since  time.Time
}

type workerUsage struct {
	apiCalls     int
	inputTokens  int
	outputTokens int
	turns        int
}

func newSwarmRunner(dir string, cfg runConfig, st *store.Store) (*swarmRunner, error) {
	repo := gitutil.New(dir)
	repo.GateCommand = cfg.testCmd

	// AIMD gate: start at min(64, max(8, maxWorkers*4)) like the JS CLI.
	initialGate := cfg.maxWorkers * 4
	if initialGate < 8 {
		initialGate = 8
	}
	if initialGate > 64 {
		initialGate = 64
	}
	gate := harness.NewAdaptiveSemaphore(initialGate)

	var h harness.Harness
	if cfg.harnessName == "echo" {
		h = harness.NewEcho()
	} else {
		dh, err := harness.NewDirect(harness.DirectConfig{
			BaseURL: cfg.apiBase,
			APIKey:  cfg.apiKey,
			Model:   cfg.model,
		})
		if err != nil {
			return nil, fmt.Errorf("direct harness: %w", err)
		}
		h = dh
	}

	return &swarmRunner{
		cfg:      cfg,
		dir:      dir,
		repo:     repo,
		store:    st,
		gate:     gate,
		guard:    sysload.NewGuard(nil, sysload.Config{}),
		harness:  h,
		workers:  make(map[string]*workerState),
		stopCh:   make(chan struct{}),
		submitCh: make(chan struct{}, 1),
	}, nil
}

// run starts the HTTP server, worker pool, autoscaler, integrator, renewer and
// reaper, then blocks until SIGINT/SIGTERM or budget exhaustion.
func (r *swarmRunner) run(ctx context.Context) error {
	// 1. HTTP API server (workers use it for claim/submit/context).
	srv := server.New(r.store)
	httpSrv := &http.Server{Addr: fmt.Sprintf(":%d", r.cfg.serverPort), Handler: srv}
	go func() {
		_ = httpSrv.ListenAndServe()
	}()
	defer func() { _ = httpSrv.Shutdown(context.Background()) }()

	// 2. Recover orphan claims from a previous crash.
	_, _ = r.store.ReapExpired() // also clears expired claims
	// Note: Go store does not yet have releaseOrphanClaims; ReapExpired is the closest.

	// 2b. Ensure the integration branch exists so worker worktrees have a base.
	if err := r.repo.EnsureBranch(ctx, r.repo.Integration(), "HEAD"); err != nil {
		return fmt.Errorf("ensure integration branch: %w", err)
	}

	// 3. Initial worker spawn.
	for i := 0; i < r.cfg.initialWorkers; i++ {
		if err := r.spawnWorker(ctx); err != nil {
			return fmt.Errorf("spawn initial worker %d: %w", i, err)
		}
	}

	// 4. Background loops.
	r.wg.Add(4)
	go r.autoscalerLoop(ctx)
	go r.integratorLoop(ctx)
	go r.renewerLoop(ctx)
	go r.reaperLoop(ctx)

	// 5. Wait for shutdown signal or budget stop.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	select {
	case <-ctx.Done():
	case <-sigCh:
	case <-r.stopCh:
	}

	r.mu.Lock()
	r.stopped = true
	r.mu.Unlock()
	close(r.stopCh)

	// Drain: wait for workers to finish current turn.
	done := make(chan struct{})
	go func() {
		r.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(30 * time.Second):
	}
	return nil
}

// ---------- worker pool ----------

func (r *swarmRunner) spawnWorker(ctx context.Context) error {
	r.mu.Lock()
	// find next free worker name
	n := len(r.workers) + 1
	name := fmt.Sprintf("w%d", n)
	for {
		if _, ok := r.workers[name]; !ok {
			break
		}
		n++
		name = fmt.Sprintf("w%d", n)
	}
	r.mu.Unlock()

	// Create worktree on the integration branch baseline.
	dir, err := r.repo.AddWorktree(ctx, name, r.repo.Integration())
	if err != nil {
		return fmt.Errorf("add worktree: %w", err)
	}
	branch := r.repo.WorkerBranch(name)

	w := &workerState{
		name:   name,
		dir:    dir,
		branch: branch,
		status: "idle",
	}
	r.mu.Lock()
	r.workers[name] = w
	r.mu.Unlock()

	r.wg.Add(1)
	go r.workerLoop(ctx, w)
	return nil
}

func (r *swarmRunner) workerLoop(ctx context.Context, w *workerState) {
	defer r.wg.Done()
	defer func() {
		w.loopDone = true
		r.releaseWorkerClaims(w)
	}()

	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		default:
		}
		if w.status == "retired" || w.status == "crashed" {
			return
		}

		// Idle suppression: skip a turn when nothing claimable and no active claim.
		if r.cfg.harnessName != "echo" {
			claimable, _ := r.store.ClaimableCount()
			mine, _ := r.store.ActiveClaimsByPeer(w.name)
			open, _ := r.store.OpenCount()
			stats, _ := r.store.Stats()
			coldStart := stats.Total == 0
			if claimable == 0 && len(mine) == 0 && !coldStart {
				w.status = "idle"
				w.idleCount++
				if open == 0 && w.idleCount >= r.cfg.idleTurnsToExit && r.activeCount() > r.cfg.minWorkers {
					w.status = "retired"
					return
				}
				select {
				case <-ctx.Done():
					return
				case <-time.After(1 * time.Second):
					continue
				}
			}
		}

		// Budget gate.
		if r.cfg.budgetTokens > 0 && !r.budgetExceeded && r.tokensSpent > r.cfg.budgetTokens {
			r.budgetExceeded = true
			_, _ = r.store.FailAllOpenItems("runner", "budget exhausted")
			close(r.stopCh)
			return
		}

		w.turn++
		hardMax := r.cfg.maxTurns * r.cfg.hardTurnFactor
		if w.turn > hardMax {
			w.status = "retired"
			return
		}
		if w.turn > r.cfg.maxTurns && r.noOpenWork() {
			w.status = "retired"
			return
		}
		w.status = "working"

		// Sync baseline: merge integration branch into worktree.
		synced, conflictFiles := r.syncWorktree(ctx, w)
		if synced.conflict {
			w.syncSkew++
			if w.syncSkew >= 8 {
				w.status = "retired"
				return
			}
			if w.syncSkew >= 3 {
				if w.redoCount >= 2 {
					w.status = "retired"
					return
				}
				w.redoCount++
				if r.resetWorktree(ctx, w) {
					w.syncSkew = 0
					_, _ = r.store.AddContext(server.ContextEntry{
						Peer: w.name, Kind: "CONFLICT",
						Text: fmt.Sprintf("worker %s reset to integration baseline (redo #%d). HOT FILES: %s",
							w.name, w.redoCount, strings.Join(conflictFiles, ", ")),
						Tags: []string{"conflict", "absorb"},
					})
				} else {
					w.status = "retired"
					return
				}
			}
		} else if synced.ok {
			w.syncSkew = 0
		}

		// Build turn brief.
		brief := r.buildBrief(w)

		// Build prompt.
		prompt := prompts.BuildWorkerPrompt(prompts.Options{
			PeerName:   w.name,
			ServerPort: r.cfg.serverPort,
			Mission:    r.cfg.mission,
			Conventions: r.cfg.conventions,
			Turn:       w.turn,
			Brief:      brief,
			Harness:    r.cfg.harnessName,
		})

		// Run turn via harness (AIMD-gated).
		var result harness.Response
		var err error
		if r.cfg.harnessName == "echo" {
			result, err = r.harness.Complete(ctx, harness.Request{
				Messages: []harness.Message{{Role: harness.RoleUser, Content: prompt}},
			})
		} else {
			err = r.gate.Do(ctx, func() error {
				var e error
				result, e = r.harness.Complete(ctx, harness.Request{
					Model:    r.cfg.model,
					Messages: []harness.Message{{Role: harness.RoleUser, Content: prompt}},
				})
				return e
			})
			if err == nil {
				r.gate.NoteOK()
			} else {
				var he *harness.HTTPError
				if errors.As(err, &he) && he.StatusCode == 503 {
					r.gate.NoteRateLimit()
				}
			}
		}

		// Auto-commit any worktree changes.
		_, _ = r.autoCommit(ctx, w)

		// Track reply and idle.
		reply := result.Text
		if len(reply) > 200 {
			reply = reply[:200]
		}
		w.lastReply = reply
		isIdle := strings.Contains(reply, "IDLE")

		if isIdle {
			w.idleCount++
			w.status = "idle"
			open, _ := r.store.OpenCount()
			if open == 0 && w.idleCount >= r.cfg.idleTurnsToExit && r.activeCount() > r.cfg.minWorkers {
				w.status = "retired"
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(5 * time.Second):
			}
		} else {
			w.idleCount = 0
			// Item hold circuit breaker: abandon after maxItemTurns or maxItemTokens.
			claims, _ := r.store.ActiveClaimsByPeer(w.name)
			for _, c := range claims {
				if w.itemHold == nil || w.itemHold.itemID != c.ItemID {
					w.itemHold = &itemHold{itemID: c.ItemID, turns: 1, since: time.Now()}
				} else {
					w.itemHold.turns++
				}
				itemTokens := r.itemTokens(c.ItemID)
				if w.itemHold.turns > r.cfg.maxItemTurns || itemTokens > r.cfg.maxItemTokens {
					r.autoSplitItem(w, c.ItemID)
					_ = r.store.Release(c.Token)
					w.itemHold = nil
				}
			}
		}

		// Rate-limit backoff (does not consume turn budget).
		if err != nil {
			var he *harness.HTTPError
			if errors.As(err, &he) && he.StatusCode == 503 {
				w.rateLimitedStreak++
				backoff := time.Duration(5000*(1<<uint(min(w.rateLimitedStreak-1, 4)))) * time.Millisecond
				if backoff > 60*time.Second {
					backoff = 60 * time.Second
				}
				w.turn-- // don't consume turn budget
				select {
				case <-ctx.Done():
					return
				case <-time.After(backoff):
				}
			} else {
				w.rateLimitedStreak = 0
				select {
				case <-ctx.Done():
					return
				case <-time.After(3 * time.Second):
				}
			}
		} else {
			w.rateLimitedStreak = 0
		}
	}
}

func (r *swarmRunner) activeCount() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	n := 0
	for _, w := range r.workers {
		if w.status != "retired" && w.status != "crashed" {
			n++
		}
	}
	return n
}

func (r *swarmRunner) noOpenWork() bool {
	open, _ := r.store.OpenCount()
	return open == 0
}

func (r *swarmRunner) releaseWorkerClaims(w *workerState) {
	claims, _ := r.store.ActiveClaimsByPeer(w.name)
	for _, c := range claims {
		_ = r.store.Release(c.Token)
	}
}

// ---------- sync / brief / commit ----------

type syncResult struct {
	ok       bool
	conflict bool
}

func (r *swarmRunner) syncWorktree(ctx context.Context, w *workerState) (syncResult, []string) {
	// Merge integration branch into worker branch.
	_, err := r.repo.GitIn(ctx, w.dir, "merge", "--no-edit", r.repo.Integration())
	if err != nil {
		// Check if it's a conflict.
		out, _ := r.repo.GitIn(ctx, w.dir, "diff", "--name-only", "--diff-filter=U")
		files := strings.Split(strings.TrimSpace(out), "\n")
		return syncResult{conflict: true}, files
	}
	return syncResult{ok: true}, nil
}

func (r *swarmRunner) resetWorktree(ctx context.Context, w *workerState) bool {
	// Hard reset to integration branch.
	_, err := r.repo.GitIn(ctx, w.dir, "reset", "--hard", r.repo.Integration())
	return err == nil
}

func (r *swarmRunner) buildBrief(w *workerState) string {
	d, err := r.store.Digest(w.name, w.lastDigestTs)
	if err != nil {
		return ""
	}
	w.lastDigestTs = d.Now
	var lines []string
	if w.lastReply != "" {
		lines = append(lines, "LAST TURN: "+w.lastReply)
	}
	if len(d.Claims) > 0 {
		var claims []string
		for _, c := range d.Claims {
			claims = append(claims, c.ItemID+" ("+c.Title+")")
		}
		lines = append(lines, "YOUR ACTIVE CLAIMS: "+strings.Join(claims, "; "))
	}
	if len(d.NewContext) > 0 {
		lines = append(lines, "NEW FINDINGS since your last turn:")
		for _, e := range d.NewContext {
			text := e.Text
			if len(text) > 160 {
				text = text[:160]
			}
			lines = append(lines, fmt.Sprintf("- [%s] %s: %s", e.Kind, e.Peer, text))
		}
	}
	lines = append(lines, fmt.Sprintf("BOARD SNAPSHOT: %d pending / %d in_progress / %d done",
		d.Stats.Pending, d.Stats.InProgress, d.Stats.Done))
	for i, it := range d.PendingPreview {
		if i >= 20 {
			break
		}
		title := it.Title
		if len(title) > 80 {
			title = title[:80]
		}
		lines = append(lines, fmt.Sprintf("- %s: %s [p%d]", it.ID, title, it.Priority))
	}
	return strings.Join(lines, "\n")
}

func (r *swarmRunner) autoCommit(ctx context.Context, w *workerState) (string, error) {
	dirty, err := r.repo.IsDirty(ctx, w.dir)
	if err != nil || !dirty {
		return "", err
	}
	sha, committed, err := r.repo.CommitAll(ctx, w.dir, fmt.Sprintf("wip: %s turn %d (auto)", w.name, w.turn))
	if err != nil || !committed {
		return "", err
	}
	return sha, nil
}

func (r *swarmRunner) itemTokens(itemID string) int {
	// Simplified: we don't track per-item tokens yet.
	return 0
}

func (r *swarmRunner) autoSplitItem(w *workerState, itemID string) {
	item, err := r.store.GetItem(itemID)
	if err != nil {
		return
	}
	total, _ := r.store.ItemCount()
	depth, _ := r.store.ItemDepth(itemID)
	if total >= r.cfg.maxItems || depth >= 4 {
		return
	}
	base := strings.TrimSuffix(item.Title, filepath.Ext(item.Title))
	_ = r.store.SetStatus(itemID, "split")
	_, err1 := r.store.CreateItem(server.CreateItemInput{
		Peer: "auto-split", Title: base + " — core implementation",
		Detail: "Split from \"" + item.Title + "\" — core implementation only, no tests. " + item.Detail,
		ParentID: &itemID, Kind: "task",
	})
	_, err2 := r.store.CreateItem(server.CreateItemInput{
		Peer: "auto-split", Title: base + " — tests",
		Detail: "Split from \"" + item.Title + "\" — tests only, assumes core impl exists. " + item.Detail,
		ParentID: &itemID, Kind: "task",
	})
	if err1 != nil || err2 != nil {
		_ = r.store.SetStatus(itemID, "pending")
	}
}

// ---------- integrator ----------

func (r *swarmRunner) integratorLoop(ctx context.Context) {
	defer r.wg.Done()
	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		case <-r.submitCh:
			// event-driven wake
		case <-time.After(200 * time.Millisecond):
			// fallback poll
		}
		subs, err := r.store.QueuedSubmissions()
		if err != nil || len(subs) == 0 {
			continue
		}
		for _, s := range subs {
			select {
			case <-r.stopCh:
				return
			default:
			}
			r.integrateOne(ctx, s)
		}
	}
}

func (r *swarmRunner) integrateOne(ctx context.Context, sub store.Submission) {
	branch := r.repo.WorkerBranch(sub.Peer)
	// Stale check: commit sha must still be on the branch.
	if sub.CommitSHA != "" {
		anc, err := r.repo.Git(ctx, "merge-base", "--is-ancestor", sub.CommitSHA, branch)
		if err != nil || anc != "" {
			reason := fmt.Sprintf("stale submission: branch %s no longer contains submit sha %s", branch, sub.CommitSHA[:10])
			_ = r.store.ResolveSubmission(sub.ID, false, reason, r.cfg.maxItemFails)
			_, _ = r.store.AddContext(server.ContextEntry{
				Peer: sub.Peer, Kind: "FAIL",
				Text: fmt.Sprintf("integration rejected for item %s (stage=stale): %s", sub.ItemID, reason),
				ItemID: &sub.ItemID, Tags: []string{"integration", "stale"},
			})
			return
		}
	}

	res, err := r.repo.Integrate(ctx, branch, fmt.Sprintf("integrate %s (%s)", sub.Peer, sub.ItemID))
	if err != nil {
		// Gate failed or conflict: reject with reason.
		reason := err.Error()
		if len(reason) > 400 {
			reason = reason[:400]
		}
		_ = r.store.ResolveSubmission(sub.ID, false, reason, r.cfg.maxItemFails)
		_, _ = r.store.AddContext(server.ContextEntry{
			Peer: sub.Peer, Kind: "FAIL",
			Text: fmt.Sprintf("integration failed for item %s: %s", sub.ItemID, reason),
			ItemID: &sub.ItemID, Tags: []string{"integration", "failed"},
		})
		return
	}

	// Noop check: submit sha must be in merged ancestry.
	if sub.CommitSHA != "" && res.SHA != "" {
		anc, err := r.repo.Git(ctx, "merge-base", "--is-ancestor", sub.CommitSHA, res.SHA)
		if err != nil || anc != "" {
			reason := fmt.Sprintf("no-op merge: submit sha %s not in merged %s ancestry", sub.CommitSHA[:10], res.SHA[:10])
			_ = r.store.ResolveSubmission(sub.ID, false, reason, r.cfg.maxItemFails)
			_, _ = r.store.AddContext(server.ContextEntry{
				Peer: sub.Peer, Kind: "FAIL",
				Text: fmt.Sprintf("integration rejected for item %s (stage=no-op): %s", sub.ItemID, reason),
				ItemID: &sub.ItemID, Tags: []string{"integration", "no-op"},
			})
			return
		}
	}

	_ = r.store.ResolveSubmission(sub.ID, true, "", r.cfg.maxItemFails)
	if len(res.Files) > 0 {
		stat := strings.Join(res.Files, "\n")
		if len(stat) > 1200 {
			stat = stat[:1200]
		}
		_, _ = r.store.AddContext(server.ContextEntry{
			Peer: sub.Peer, Kind: "PATCH_SUMMARY",
			Text: fmt.Sprintf("merged item %s into integration @%s:\n%s", sub.ItemID, res.SHA[:10], stat),
			ItemID: &sub.ItemID, Tags: []string{"integration", "patch"},
		})
	}
}

// ---------- autoscaler ----------

func (r *swarmRunner) autoscalerLoop(ctx context.Context) {
	defer r.wg.Done()
	maxSpawnPerRound := 64
	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		case <-time.After(1 * time.Second):
		}

		open, _ := r.store.OpenCount()
		recentSplit, _ := r.store.RecentSplitCount(300000)
		active := r.activeCount()

		demand := open
		if recentSplit > demand {
			demand = recentSplit
		}
		capped := demand
		if open*2 > 8 {
			if capped > open*2 {
				capped = open * 2
			}
		} else if capped > 8 {
			capped = 8
		}
		ceilingByMax := r.cfg.maxWorkers
		if ceilingByMax <= 0 {
			ceilingByMax = 1 << 30 // unlimited
		}
		want := capped
		if want < r.cfg.minWorkers {
			want = r.cfg.minWorkers
		}
		if want > ceilingByMax {
			want = ceilingByMax
		}

		// Resource guard.
		allow, sample, _ := r.guard.Allow()
		if !allow {
			// high pressure: freeze; critical: shrink by 1.
			if sample.CPUPct > 92 || sample.MemPct > 92 {
				want = active - 1
				if want < 1 {
					want = 1
				}
			} else if want > active {
				want = active
			}
		}

		if want > active {
			need := want - active
			if need > maxSpawnPerRound {
				need = maxSpawnPerRound
			}
			for i := 0; i < need; i++ {
				if err := r.spawnWorker(ctx); err != nil {
					break
				}
			}
		} else if want < active {
			// Shrink: retire idle workers only.
			r.mu.Lock()
			for _, w := range r.workers {
				if active <= want {
					break
				}
				if w.status == "idle" {
					w.status = "retired"
					active--
				}
			}
			r.mu.Unlock()
		}
	}
}

// ---------- lease renewer ----------

func (r *swarmRunner) renewerLoop(ctx context.Context) {
	defer r.wg.Done()
	interval := time.Duration(r.cfg.leaseSec/3) * time.Second
	if interval < 1*time.Second {
		interval = 1 * time.Second
	}
	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		case <-time.After(interval):
		}
		r.mu.Lock()
		for _, w := range r.workers {
			if w.status == "retired" || w.status == "crashed" {
				continue
			}
			claims, _ := r.store.ActiveClaimsByPeer(w.name)
			for _, c := range claims {
				_ = r.store.Renew(c.Token)
			}
		}
		r.mu.Unlock()
	}
}

// ---------- reaper ----------

func (r *swarmRunner) reaperLoop(ctx context.Context) {
	defer r.wg.Done()
	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		case <-time.After(10 * time.Second):
		}
		r.mu.Lock()
		var toRemove []string
		for name, w := range r.workers {
			if !w.loopDone || (w.status != "retired" && w.status != "crashed") {
				continue
			}
			hasQueued, _ := r.store.HasQueuedSubmissionForPeer(name)
			if hasQueued {
				continue
			}
			if !w.reapFailAt.IsZero() && time.Since(w.reapFailAt) < time.Duration(10*(1<<uint(min(w.reapFailCount, 5))))*time.Second {
				continue
			}
			toRemove = append(toRemove, name)
		}
		r.mu.Unlock()

		for _, name := range toRemove {
			if err := r.repo.RemoveWorktree(ctx, name); err != nil {
				r.mu.Lock()
				if w, ok := r.workers[name]; ok {
					w.reapFailCount++
					w.reapFailAt = time.Now()
				}
				r.mu.Unlock()
				continue
			}
			r.mu.Lock()
			delete(r.workers, name)
			r.mu.Unlock()
		}
	}
}
