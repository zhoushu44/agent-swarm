package main

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/signal"
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
	// maxNoProgressTurns parks a worker as crashed after this many consecutive
	// turns that end with no submission and no idle — i.e. the endpoint is not
	// driving tools. See the no-progress guard in workerLoop.
	maxNoProgressTurns int
	testCmd            string
	mission            string
	conventions        string
	serverPort         int
	harnessName        string // "echo" for offline tests, "direct" for OpenAI-compatible
	apiBase            string
	apiKey             string
	model              string
	exitWhenDone       bool
}

func defaultRunConfig() runConfig {
	return runConfig{
		maxWorkers:         1000,
		minWorkers:         1,
		initialWorkers:     8,
		maxTurns:           10,
		hardTurnFactor:     6,
		idleTurnsToExit:    2,
		leaseSec:           300,
		maxItems:           200,
		maxItemTurns:       6,
		maxItemTokens:      2_000_000,
		maxItemFails:       3,
		maxNoProgressTurns: 3,
		serverPort:         17771,
		harnessName:        "direct",
	}
}

// swarmRunner is the Go port of the JS Runner. It owns the worker pool,
// autoscaler, integrator, lease renewer and reaper.
type swarmRunner struct {
	cfg     runConfig
	dir     string
	repo    *gitutil.Repo
	store   *store.Store
	gate    *harness.AdaptiveSemaphore
	guard   *sysload.Guard
	harness harness.Harness

	mu       sync.Mutex
	workers  map[string]*workerState
	// itemHoldBy records which worker currently holds which item, so the
	// progress line can tell "idle" apart from "holding a claim and getting
	// nowhere". Written when a claim is taken, cleared on release/exit.
	itemHoldBy map[string]string
	stopped  bool
	stopCh   chan struct{}
	wg       sync.WaitGroup
	submitCh chan struct{} // event-driven integrator wake

	// token accounting
	tokensSpent int64
	// Cumulative token usage across ALL workers (current + retired). Done-line
	// and budget accounting read these so retired workers' spend is not lost.
	totalInputTokens  int64
	totalOutputTokens int64
	totalAPICalls     int64
	budgetExceeded    bool

	// itemSpend attributes token spend per item so the maxItemTokens breaker
	// and cost reporting work. Keyed by item id, guarded by mu.
	itemSpend map[string]int64

	startMs time.Time
}

type workerState struct {
	name              string
	dir               string
	branch            string
	status            string // working / idle / retired / crashed
	turn              int
	idleCount         int
	syncSkew          int
	redoCount         int
	lastReply         string
	lastDigestTs      int64
	itemHold          *itemHold
	rateLimitedStreak int
	// noProgressStreak counts consecutive turns that ended WITHOUT a submission
	// and without going idle — the signature of a model that ignores tool_choice
	// and replies in plain prose. Past maxNoProgressTurns the worker is parked as
	// crashed so it stops burning tokens on a endpoint it cannot drive.
	noProgressStreak int
	loopDone         bool
	reapFailCount    int
	reapFailAt       time.Time
	tokenUsage       workerUsage
	// lastArtifact is the output fingerprint ("f3+120-8") from the most recent
	// auto-commit. The empty-output breaker compares it across turns: three
	// turns in a row with an unchanged (or empty) fingerprint means the model
	// is talking, not working.
	lastArtifact    string
	prevArtifact    string
	emptyArtifactAt time.Time
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

	// Three-way merge arbitration: on a merge conflict, ask the same model to
	// produce the merged file instead of rejecting the submission outright.
	// The gate still runs afterwards, so a bad arbitration is caught exactly
	// like a bad hand-merge. Nil resolver (echo harness, or no API configured)
	// keeps the original reject-and-retry behaviour.
	if cfg.harnessName != "echo" && strings.TrimSpace(cfg.model) != "" {
		repo.Resolver = func(ctx context.Context, cf gitutil.ConflictFile) (string, error) {
			resp, err := h.Complete(ctx, harness.Request{
				Model: cfg.model,
				Messages: []harness.Message{
					{Role: "user", Content: gitutil.MergePromptFor(cf)},
				},
				MaxTokens: 8192,
			})
			if err != nil {
				return "", err
			}
			merged, err := gitutil.DecodeMerged(resp.Text)
			if err != nil {
				return "", err
			}
			return merged, nil
		}
	}

	return &swarmRunner{
		cfg:        cfg,
		dir:        dir,
		repo:       repo,
		store:      st,
		gate:       gate,
		guard:      sysload.NewGuard(nil, sysload.Config{}),
		harness:    h,
		workers:    make(map[string]*workerState),
		itemHoldBy: make(map[string]string),
		itemSpend:  make(map[string]int64),
		stopCh:     make(chan struct{}),
		submitCh:   make(chan struct{}, 1),
	}, nil
}

// run starts the HTTP server, worker pool, autoscaler, integrator, renewer and
// reaper, then blocks until SIGINT/SIGTERM or budget exhaustion.
func (r *swarmRunner) run(ctx context.Context) error {
	r.startMs = time.Now()
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
	r.wg.Add(5)
	go r.autoscalerLoop(ctx)
	go r.integratorLoop(ctx)
	go r.renewerLoop(ctx)
	go r.reaperLoop(ctx)
	go r.progressLoop(ctx)

	// 5. Wait for shutdown signal or budget stop.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	// exit-when-done: board clear + integration queue empty + no active workers.
	var doneWatch <-chan time.Time
	if r.cfg.exitWhenDone {
		t := time.NewTicker(3 * time.Second)
		defer t.Stop()
		doneWatch = t.C
	}

	for {
		select {
		case <-ctx.Done():
			goto shutdown
		case <-sigCh:
			goto shutdown
		case <-r.stopCh:
			goto shutdown
		case <-doneWatch:
			open, _ := r.store.OpenCount()
			queued := 0
			if qs, err := r.store.QueuedSubmissions(); err == nil {
				queued = len(qs)
			}
			r.mu.Lock()
			active := 0
			for _, w := range r.workers {
				if w.status != "retired" && w.status != "crashed" {
					active++
				}
			}
			r.mu.Unlock()
			if open == 0 && queued == 0 && active == 0 {
				stats, _ := r.store.Stats()
				r.mu.Lock()
				in, out, apiCalls := r.totalInputTokens, r.totalOutputTokens, r.totalAPICalls
				r.mu.Unlock()
				wall := time.Since(r.startMs).Milliseconds()
				fmt.Printf("[swarm] DONE: {\"done\":%d,\"total\":%d,\"failed\":%d,\"integrations\":%d,\"tokenUsage\":{\"apiCalls\":%d,\"inputTokens\":%d,\"outputTokens\":%d},\"wallTimeMs\":%d}\n",
					stats.Done, stats.Total, stats.Failed, stats.Submitted, apiCalls, in, out, wall)
				goto shutdown
			}
		}
	}

shutdown:

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
			PeerName:    w.name,
			ServerPort:  r.cfg.serverPort,
			Mission:     r.cfg.mission,
			Conventions: r.cfg.conventions,
			Turn:        w.turn,
			Brief:       brief,
			Harness:     r.cfg.harnessName,
		})

		// Run turn via harness (AIMD-gated).
		var result harness.Response
		var err error
		loopSteps := 0
		loopSubmitted := false
		if r.cfg.harnessName == "echo" {
			result, err = r.harness.Complete(ctx, harness.Request{
				Messages: []harness.Message{{Role: harness.RoleUser, Content: prompt}},
			})
		} else {
			err = r.gate.Do(ctx, func() error {
				var e error
				if dh, ok := r.harness.(*harness.DirectHarness); ok {
					var lr harness.AgentLoopResult
					lr, e = dh.RunAgentLoop(ctx, r.cfg.model, prompt, harness.AgentLoopConfig{
						WorkDir:    w.dir,
						ServerPort: r.cfg.serverPort,
					})
					result = harness.Response{Text: lr.Reply, Usage: lr.Usage}
					loopSteps = lr.Steps
					loopSubmitted = lr.Submitted
				} else {
					result, e = r.harness.Complete(ctx, harness.Request{
						Model:    r.cfg.model,
						Messages: []harness.Message{{Role: harness.RoleUser, Content: prompt}},
					})
					loopSteps = 1
				}
				return e
			})
			if err == nil {
				r.gate.NoteOK()
			} else {
				var he *harness.HTTPError
				if errors.As(err, &he) && (he.StatusCode == 503 || he.StatusCode == 429) {
					r.gate.NoteRateLimit()
				}
			}
		}

		// Token accounting.
		if err == nil {
			w.tokenUsage.apiCalls++
			w.tokenUsage.inputTokens += result.Usage.PromptTokens
			w.tokenUsage.outputTokens += result.Usage.CompletionTokens
			w.tokenUsage.turns++
			r.mu.Lock()
			r.tokensSpent += int64(result.Usage.TotalTokens)
			r.totalInputTokens += int64(result.Usage.PromptTokens)
			r.totalOutputTokens += int64(result.Usage.CompletionTokens)
			r.totalAPICalls += int64(loopSteps) // real HTTP rounds, not turns
			// Item-level ledger: attribute this round's spend to whatever item
			// the worker holds. Without it the maxItemTokens circuit breaker
			// reads a constant 0 and never fires.
			if claims, cerr := r.store.ActiveClaimsByPeer(w.name); cerr == nil {
				for _, c := range claims {
					r.itemSpend[c.ItemID] += int64(result.Usage.TotalTokens)
				}
			}
			r.mu.Unlock()
		}

		// Auto-commit any worktree changes.
		_, _ = r.autoCommit(ctx, w)

		// Track reply and idle.
		reply := result.Text
		if err != nil {
			fmt.Fprintf(os.Stderr, "[swarm] %s turn %d harness err: %v\n", w.name, w.turn, err)
		}
		if len(reply) > 200 {
			reply = reply[:200]
		}
		w.lastReply = reply
		// Idle is an OBJECTIVE signal, not something the model has to announce.
		// Measured on a real run: with 0 pending items the model kept replying
		// with work summaries and never said "IDLE", so the string match never
		// fired and a worker burned ~1.5 min of tokens after the board was done.
		// Now: no open items AND no claim held by this worker => idle.
		open, _ := r.store.OpenCount()
		myClaims, _ := r.store.ActiveClaimsByPeer(w.name)
		isIdle := strings.Contains(reply, "IDLE") || (open == 0 && len(myClaims) == 0)

		if isIdle {
			w.idleCount++
			w.status = "idle"
			w.noProgressStreak = 0
			if open == 0 && w.idleCount >= r.cfg.idleTurnsToExit && r.activeCount() > r.cfg.minWorkers {
				w.status = "retired"
				return
			}
			// Nothing to do and we are the last worker: leave immediately
			// instead of spinning out the idle budget.
			if open == 0 && len(myClaims) == 0 && r.activeCount() <= r.cfg.minWorkers {
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
			// No-progress guard (direct harness only): a turn that ends with NO
			// submission and is NOT idle means the model never drove a tool to
			// completion — the signature of an endpoint that ignores tool_choice and
			// replies in plain prose. Without this, such a worker loops forever: each
			// turn re-holds its claim, the item stays in_progress, and tokens burn at
			// 0 submissions. Park the worker as crashed past the threshold so it stops
			// spending; the deferred releaseWorkerClaims returns its item to pending.
			if r.cfg.harnessName != "echo" && !loopSubmitted && err == nil {
				w.noProgressStreak++
				if w.noProgressStreak >= r.cfg.maxNoProgressTurns {
					fmt.Fprintf(os.Stderr, "[swarm] %s parked: %d turns with no submission (endpoint likely ignores tool_choice / non-agent model)\n", w.name, w.noProgressStreak)
					w.status = "crashed"
					return
				}
			} else {
				w.noProgressStreak = 0
			}

// Empty-output breaker: the fingerprint from the last auto-commit says
// whether the turn produced anything. A static (or empty) fingerprint
// across turns means the model is talking, not working — park it instead
// of burning its whole token budget.
if w.lastArtifact == "" || w.lastArtifact == w.prevArtifact {
	if w.emptyArtifactAt.IsZero() {
		w.emptyArtifactAt = time.Now()
	}
} else {
	w.emptyArtifactAt = time.Time{}
}
w.prevArtifact = w.lastArtifact
if !w.emptyArtifactAt.IsZero() && time.Since(w.emptyArtifactAt) > 3*time.Duration(r.cfg.maxItemTurns)*time.Second {
	fmt.Fprintf(os.Stderr, "[swarm] %s parked: %d turns with no new output (fingerprint %q static)\n", w.name, 3, w.lastArtifact)
	w.status = "crashed"
	return
}
			// Item hold circuit breaker: abandon after maxItemTurns or maxItemTokens.
			claims, _ := r.store.ActiveClaimsByPeer(w.name)
r.mu.Lock()
for _, c := range claims {
	r.itemHoldBy[w.name] = c.ItemID
}
r.mu.Unlock()
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
r.mu.Lock()
delete(r.itemHoldBy, w.name)
r.mu.Unlock()
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
	// Output fingerprint: (files changed, lines added, lines removed) since
	// the integration baseline. A worker whose fingerprint does not move is
	// burning tokens without producing anything — the signature the progress
	// line used to be blind to.
	w.lastArtifact = r.artifactFingerprint(ctx, w.dir)
	return sha, nil
}

// artifactFingerprint summarises what a worktree has actually produced. It
// returns a compact string; an empty string means "nothing measurable".
func (r *swarmRunner) artifactFingerprint(ctx context.Context, dir string) string {
	out, err := r.repo.GitIn(ctx, dir, "diff", "--numstat", "HEAD")
	if err != nil || strings.TrimSpace(out) == "" {
		return ""
	}
	files, add, del := 0, 0, 0
	for _, line := range strings.Split(strings.TrimSpace(out), "\n") {
		parts := strings.Split(line, "\t")
		if len(parts) < 3 {
			continue
		}
		files++
		add += atoiSafe(parts[0])
		del += atoiSafe(parts[1])
	}
	return fmt.Sprintf("f%d+%d-%d", files, add, del)
}

func atoiSafe(s string) int {
	n := 0
	for _, c := range strings.TrimSpace(s) {
		if c < '0' || c > '9' {
			return 0
		}
		n = n*10 + int(c-'0')
	}
	return n
}

func (r *swarmRunner) itemTokens(itemID string) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return int(r.itemSpend[itemID])
}

// itemSpendSnapshot returns the (item -> tokens) ledger for reporting.
func (r *swarmRunner) itemSpendSnapshot() map[string]int64 {
	r.mu.Lock()
	defer r.mu.Unlock()
	out := make(map[string]int64, len(r.itemSpend))
	for k, v := range r.itemSpend {
		out[k] = v
	}
	return out
}

func (r *swarmRunner) autoSplitItem(w *workerState, itemID string) {
	item, err := r.store.GetItem(itemID)
	if err != nil {
		return
	}
	total, _ := r.store.ItemCount()
	depth, _ := r.store.ItemDepth(itemID)
	// Depth 2, not 4: a third-level split produced titles like
	// "06-openai-chat-endpoint (tests) (core impl) (core impl) (core impl)"
	// in practice, and those children all edit the same file.
	if total >= r.cfg.maxItems || depth >= 2 {
		return
	}
	// Split by resource, not by "core impl" / "tests": every child must name
	// exactly ONE resource token (path or path#symbol) so the stigmergy
	// conflict scan can keep them apart. A child that names nothing concrete
	// is worse than no split — it becomes an unfinishable god task.
	resources := store.ResourcesOf(item)
	if len(resources) <= 1 {
		// Nothing to split along: leave it for a stronger model / a human.
		return
	}
	_ = r.store.SetStatus(itemID, "split")
	for _, res := range resources {
		_, _ = r.store.CreateItem(server.CreateItemInput{
			Peer: "auto-split",
			// Bare resource token as the title: no verbs, no parentheticals.
			// This is what makes N workers agree on the same sub-item.
			Title: res,
			Detail: fmt.Sprintf("Split from %q — deliver %s only. %s", item.Title, res, item.Detail),
			ParentID: &itemID,
			Kind:     "task",
		})
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
				Text:   fmt.Sprintf("integration rejected for item %s (stage=stale): %s", sub.ItemID, reason),
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
			Text:   fmt.Sprintf("integration failed for item %s: %s", sub.ItemID, reason),
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
				Text:   fmt.Sprintf("integration rejected for item %s (stage=no-op): %s", sub.ItemID, reason),
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
			Text:   fmt.Sprintf("merged item %s into integration @%s:\n%s", sub.ItemID, res.SHA[:10], stat),
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

// ---------- progress ----------

// progressLoop prints a JS-style status line every 5s:
// [MM:SS] pending=N inprog=M submtd=K done=D/T integ=I inflight=L w=[w1:working ...] | R tok/s
func (r *swarmRunner) progressLoop(ctx context.Context) {
	defer r.wg.Done()
	if r.cfg.harnessName == "echo" {
		return // offline: no rate signal, stay quiet
	}
	t := time.NewTicker(5 * time.Second)
	defer t.Stop()
	var lastTok int64
	var lastTs time.Time
	var lastDone, lastSubm int
	firstSample := true
	for {
		select {
		case <-ctx.Done():
			return
		case <-r.stopCh:
			return
		case now := <-t.C:
			stats, err := r.store.Stats()
			if err != nil {
				continue
			}
			queued := 0
			if qs, qerr := r.store.QueuedSubmissions(); qerr == nil {
				queued = len(qs)
			}
			r.mu.Lock()
			wparts := make([]string, 0, len(r.workers))
			stalled := []string{}
			for _, w := range r.workers {
				wparts = append(wparts, w.name+":"+w.status)
				// A worker holding a claim while its no-progress streak climbs
				// is exactly what the operator needs to see; the old
				// "inflight=40960" line never showed it.
				if _, held := r.itemHoldBy[w.name]; held && w.noProgressStreak > 0 {
					stalled = append(stalled, fmt.Sprintf("%s(np%d)", w.name, w.noProgressStreak))
				}
			}
			tok := r.tokensSpent
			r.mu.Unlock()

			rate := ""
			if !lastTs.IsZero() {
				dt := now.Sub(lastTs).Seconds()
				if dt > 0 {
					rate = fmt.Sprintf(" | %.0f tok/s", float64(tok-lastTok)/dt)
				}
			}
			// Deltas since the previous sample: the honest liveness signal.
			delta := ""
			if firstSample {
				delta = " | (sampling)"
			} else {
				dDone := stats.Done - lastDone
				dSubm := stats.Submitted - lastSubm
				if dDone > 0 || dSubm > 0 {
					delta = fmt.Sprintf(" | +%d done +%d subm", dDone, dSubm)
				} else {
					delta = " | NO PROGRESS"
				}
			}
			firstSample = false
			lastTok = tok
			lastTs = now
			lastDone = stats.Done
			lastSubm = stats.Submitted

			stalledPart := ""
			if len(stalled) > 0 {
				stalledPart = " STALLED=[" + joinNonEmpty(stalled, " ") + "]"
			}
			elapsed := int(now.Sub(r.startMs).Seconds())
			fmt.Printf("[%02d:%02d] pending=%d inprog=%d submtd=%d done=%d/%d integ=%d inflight=%d w=[%s]%s%s%s\n",
				elapsed/60, elapsed%60,
				stats.Pending, stats.InProgress, stats.Submitted, stats.Done, stats.Total,
				queued, r.gate.Limit(), joinNonEmpty(wparts, " "), rate, delta, stalledPart)
		}
	}
}

func joinNonEmpty(xs []string, sep string) string {
	out := xs[:0]
	for _, x := range xs {
		if x != "" {
			out = append(out, x)
		}
	}
	return strings.Join(out, sep)
}

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
		// Sweep expired leases periodically (JS does it inline in tryClaim; the
		// Go store's Claim does that too, but only when someone attempts that
		// exact item — the periodic sweep covers items nobody re-attempts).
		_, _ = r.store.ReapExpired()
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
