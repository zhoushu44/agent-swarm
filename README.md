# swarm-go

Go port of the [agent-swarm](../swarm) multi-agent parallel dev orchestrator. Workers claim items off a SQLite-backed board, drive an LLM through a 4-tool agent loop (bash / write / http / finish), and an integrator merges their worktrees.

**Status: feature-complete.** 192 tests green. Every guard below was added after a real run demonstrated the failure it prevents; each entry names the measurement.

## Quick start

```bash
swarm seed <dir> --file seed.json     # what you want done — intent is enough
swarm run  <dir>                      # that's it
swarm status <dir>                    # board + "seed work: N/M terminal"
swarm log   <dir>                     # what happened, as structured events
```

No time limits, no worker counts, no file lists. You describe the work; the
swarm decides how to cut it up and how long to run.

### What you write, and what it does with it

```json
[
  { "title": "让 app 正确处理错误", "detail": "到处都在吞错误。修好。" }
]
```

That item names no files and no symbols. What happens:

1. **Presplit** — if the detail *does* mention real files, the item is split
   into one child per file before any worker starts. Zero cost.
2. **First-turn split** — if it does not, the first worker to claim it gets
   `needsSplit: true`, reads the code to find what is involved, creates one
   child per file, and takes one of them. The rest run in parallel.
3. **Symbol-level dedup** — two items naming different functions of the same
   file do not conflict and are not folded into one.

So a 30-minute job becomes several parallel 5-minute jobs without you writing
a spec.

### How the run knows when to stop

There is no wall clock to set. The ceiling is a **silence timeout**:

- progress (a merge, a submission, an item reaching a terminal state) resets the timer
- `--idle-stop` (default **10 min**) with no progress ends the run, with a full report
- `--exit-when-done` (default on) exits as soon as **the items you seeded** are terminal — workers keep inventing useful bonus work, and that no longer holds the door open
- a generous hard ceiling (`--max-wall-clock`, default 6h) remains only as a backstop for pathological cases

Net effect: small jobs leave in minutes, big jobs run as long as they keep
moving, dead ones stop within ten minutes.

### Interrupting and resuming

Stopping a run (Ctrl-C, silence timeout, or seed-done) loses nothing:

- finished items are **not re-claimable** — a second run with the same seed does
  not redo them
- open items resume where they were
- everything that passed the gate stays on the `integration` branch

The end-of-run report says exactly which items are in which state, split into
**SEED WORK** (what you asked for) and **BONUS WORK** (what the swarm found on
its own), plus what is still open and what each item cost in tokens.
`swarm log <dir>` gives the per-event trail.

## Autoscaler: the worker pool is elastic, not fixed

The single most important behavior for long-running jobs: **the worker count is not fixed at `--workers`**. An autoscaler loop (`autoscalerLoop`, 1s tick) continuously grows and shrinks the pool between `--workers` (floor) and `--max-workers` (ceiling, default 1000).

Decision inputs each tick:

- `open` = pending + in_progress items on the board
- `recentSplit` = items split in the last 5 min (a burst of splits signals incoming sub-item load)
- `demand = max(open, recentSplit)`, then `want = min(demand, open*2, maxWorkers)` — cap at 2× open items so we never spawn more workers than there is work to feed, and never exceed `--max-workers`
- resource guard (`sysload`): if CPU or Mem > 92%, **shrink by 1**; if merely high pressure, **freeze** (no new spawns)

Behavior:

- **Scale up**: if `want > active`, spawn up to 64 workers per round until demand is met. A burst of auto-split sub-items or a large seed immediately pulls up w17, w18, … — you saw this in the "full-on" bench where splitting 4 big items spawned w17–w42.
- **Scale down**: if `want < active`, retire **idle** workers only — a worker mid-task is never killed. Under `--exit-when-done` the floor (`minWorkers`) drops to 0 so the pool drains completely at the end.
- **Net effect for a day-long run**: `seed 500 → initial --workers 16 → backlog builds → pool grows toward --max-workers → tail clears → workers retire → last few finish`. The ceiling is never the worker count; the real limiters are the LLM API's 429 capacity (governed by the AIMD gate) and the integrator's serial merge.

## Windows worktree locks: self-heal via dev/swarm-clean.ps1

On Windows a swarm run can stall with **zero output** when `.swarm/worktrees/wN` is held by a live process — an open editor/terminal, antivirus scanner, or a zombie `git.exe` left over from a crashed run. `git worktree add` / `RemoveAll` then fail at the OS level and workers never start producing. This is an environment problem, not a swarm logic bug, so swarm deliberately does **not** force-kill file handles (that could kill your editor).

Instead run the bundled cleanup before (re)starting:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File dev/swarm-clean.ps1 -WorkDir <repoDir>
```

What it does, in order, safe-by-default:

1. kills only **zombie** `git.exe` processes (empty `CommandLine` — never a git command you actually launched)
2. `git worktree prune` to drop stale registry entries
3. removes leftover `.swarm/worktrees/*` dirs; a dir still held by a live process is **skipped with a warning**, not force-deleted
4. **keeps the board db** (`swarm.db` / `state.db`) so board state survives; pass `-ResetDB` only when you want a full reseed (destructive)

If a worktree is still locked after the run, close whatever is holding it and re-run. The same script ships in the `swarm-operator` / `swarm-orchestrator` skills so the agent self-heals automatically before a run.

## v5.4.0: survive non-compliant tool-calling endpoints

Some endpoints ignore `tool_choice:"required"` and answer in plain prose when the model judges "no tool is needed" (observed on deepseek-v4-flash behind certain proxies, and reproducible on other non-official/self-hosted endpoints — vLLM, relays, middle-tier proxies all vary in `tool_choice` compliance). Previously the agent loop treated `len(tool_calls)==0` as "task complete", so a worker would claim an item, get a plain-text "I'll help you!" reply, and exit having done **nothing** — board shows all `in_progress`, zero output, token flow drops to 0.

Two layers of defense:

- **Runtime guard** (`agentloop.go`): a plain-text reply with no tool call is only a legitimate finish when work was already **submitted**. Otherwise the loop re-primes the model with a hard tool-use instruction and retries (up to 3×), then gives up rather than spinning to MaxSteps. Turns the failure mode from "silent no-op marked done" into "forced to actually call tools".
- **Prompt hard rule** (`prompts.go` `always-call-a-tool`): NEVER reply with plain text alone; every turn must make at least one tool call. Teaches compliant models to never trigger the guard in the first place.

Net effect: **any** OpenAI-compatible endpoint now works regardless of whether it honors `tool_choice` — compliant endpoints are unaffected (first turn already returns tool_calls), non-compliant ones get re-primed into working instead of silently no-oping.

## v5.8.0: no-progress guard — stop burning tokens on endpoints the model can't drive

v5.4's retry guard fixed *mislabeling* a plain-text turn as "done". But it left a worse failure mode open: on a task the model genuinely cannot drive tools for (too complex, long multi-step chain, weak endpoint), the agent loop retries 3×, gives up, returns `Submitted:false` — and the runner **ignored that flag**. The worker stayed `working`, kept its claim, and re-ran the same prompt next turn. Result: an infinite plain-text retry loop. Observed signature: token rate pulses (burst → 0 → burst), `pending` drains to 0 but `submtd=0`, workers stuck `working`, ~1.3M tokens burned in 6 min with **zero submissions**.

Fix (`runner.go`): the runner now consumes `AgentLoopResult.Submitted`. A turn that ends with **no submission and not idle** increments `noProgressStreak`; past `maxNoProgressTurns` (default 3) the worker is parked as `crashed` with a clear log line, and the loop's deferred `releaseWorkerClaims` returns its item to `pending` for another worker (or a fixed endpoint) to pick up. Compliant endpoints are unaffected — a normal turn either submits or goes idle, both of which reset the streak.

This is a **stop-loss**, not a cure: if the model truly can't drive tools for a task, no worker will complete it — the guard just makes the failure fast, visible, and cheap (bounded token spend) instead of an infinite silent burn.

## Benchmark: Go v5.1.0 vs JS v3.1.0

Real-token benchmark: 20 `util/*` tasks, deepseek-v4-flash, 8 workers, `--exit-when-done`. Both sides run the same mission constraint ("only the 20 seeded utils, no infrastructure items"). Metrics sampled from the real leaf process (`swarm.exe` / `node.exe`), not the `cmd.exe` wrapper.

| Metric | JS (node) | Go (swarm.exe) | Delta |
|---|---|---|---|
| **wall time** | 98.5s | 83.3s | **-15.4%** |
| **peak memory** | 96.5MB | **22.6MB** | **-76.6% (4.3×)** |
| total CPU | 2.2s | 2.1s | ~持平 |
| CPU utilization¹ | ~2.2% | ~2.5% | ~持平 |
| done | 20/20 | 20/20 | 持平 |
| item divergence² | none | none | 持平 |
| apiCalls³ | 265 | 286 | +7.9% |
| input tokens | 1,640,196 | 2,044,069 | +24.6% ⚠️ |
| output tokens | 58,788 | 45,668 | **-22.3%** |

¹ CPU utilization = totalCpu / wall (single-core equivalent). Both sides are I/O-bound on the LLM API — CPU is idle ~97% of the time, so Go's compute advantage does not show up here.

² "Divergence" = workers creating extra items beyond the 20 seeds (barrel files, README, package.json). Fixed in v5.1.0 by `f:`-key file-path dedup in `store.CreateItem` + prompt idempotency instruction + bench `--mission` constraint.

³ Go counts real HTTP rounds inside the agent loop (`lr.Steps`); JS counts one per turn.

### Takeaways

- **Memory is the headline win**: Go peaks at **22.6MB vs Node's 96.5MB — 4.3× less**. At `maxWorkers=1000` this is the difference between "fits on a laptop" and "OOM".
- **CPU is a non-factor** in this workload: the coordinator spends its life waiting on the LLM API. Both sides sit at ~2% CPU.
- **Wall time** favors Go by ~15% on identical task graphs — mostly from cheaper process startup and tighter tool-call loops.
- **Input tokens run ~25% higher** on Go: its prompt (agent-loop system preamble + BOARD SNAPSHOT serialization) is wordier than the JS equivalent. Output tokens (the expensive, latency-bound side) run 22% lower, which is why wall time still wins despite the input overhead.

### Reproduce

```powershell
# from bench/
powershell -NoProfile -ExecutionPolicy Bypass -File bench.ps1 -Side go -WorkDir go-repo -LogCsv go-metrics.csv
powershell -NoProfile -ExecutionPolicy Bypass -File bench.ps1 -Side js -WorkDir js-repo -LogCsv js-metrics.csv
```

`bench.ps1` walks the process tree each 5s sample and records the leaf `swarm.exe` / `node.exe` WorkingSet64 + CPU — sampling the `cmd /c` wrapper returns ~0 and is the reason earlier runs reported a bogus "6.7MB持平".

## v5.2.0: speed pass (parallel tool calls + AIMD slow start)

v5.2.0 (this tree) attacks LLM-round latency on two axes:

- **Parallel tool calls** ([internal/harness/agentloop.go](internal/harness/agentloop.go)) — when the model emits multiple independent `tool_calls` in one round (e.g. `write(a.js)` + `write(b.js)`), they now fan out to goroutines instead of executing serially. Results are appended in original order so the transcript stays deterministic. `finish` still short-circuits.
- **AIMD slow start** ([internal/harness/adaptive.go](internal/harness/adaptive.go)) — before the first 429/503, the inflight window **doubles** every 8 successful calls (64 → 128 → 256 → …, log₂(n) rounds to capacity) instead of creeping +1. The first rate-limit permanently switches back to classic AIMD additive increase.
- **Prompt BATCH instruction** ([internal/prompts/body.go](internal/prompts/body.go)) — tells the model to emit independent actions as parallel tool calls in one round, only serializing when a later call consumes an earlier call's output.
- **workers default 16** (was 8) — more lanes to keep the wider inflight window saturated.

### v5.2.0 benchmark (20 `util/*` tasks, deepseek-v4-flash, 16 workers)

3 runs, all 20/20 done, zero divergence:

| run | wall | apiCalls | input tokens | output tokens |
|---|---|---|---|---|
| 1 | **71.2s** | 380 | 2.53M | 54k |
| 2 | 81.4s | 407 | 2.84M | 63k |
| 3 | 122.1s | 413 | 2.95M | 65k |
| **mean** | **91.6s** | 400 | 2.77M | 61k |

Peak vs v5.1.0 (83.3s): **−17%**. Vs JS v3.1.0 (98.5s): **−46%**. Wall variance is dominated by LLM sampling luck on the long-tail critical path (run 3 had two workers stall ~20s at 18/20).

## v5.3.0: measured-and-rejected experiments

Two first-principles ideas were implemented, gated behind flags, and **benchmarked on a heterogeneous task set** (18 small utils + 3 large tasks: `lib/eventEmitter`, `lib/promisePool`, `lib/lruCache` — designed to create a long tail). Both lost. Code reverted in v5.3.0; the data is kept here so the ideas don't get re-proposed without new evidence.

| config | wall | done | apiCalls | input tokens | verdict |
|---|---|---|---|---|---|
| baseline | **100.6s** | 21/21 | 408 | 2.87M | — |
| `--aggressive-split` (big items split after 1–2 held turns) | 151.7s | 21/21 | 402 | 2.88M | **+51% wall** |
| `--speculative` (2 workers race one item, first submit wins) | 187.6s | 21/21 | 784 | **7.34M** | **+86% wall, 2.6× tokens** |
| both on | 355.6s | 29/29 (8 split children) | 1072 | **11.2M** | **+253% wall, 3.9× tokens** |

**Why they lost:**

- **aggressive-split** serializes the critical path — splitting one big task into "core" + "tests" children means child B can't claim until the integrator merges child A. On a 21-task / 16-worker board the workers are rarely idle, so the split never finds a free lane; it just adds an integration barrier.
- **speculative** burns 2× tokens on the raced item but the winner doesn't finish *faster* than a solo worker would have — the loser is pure waste. And because the board has more items than workers, the "idle worker picks up a raced item" scenario never materializes; racers displace *other* useful work.

Both mechanisms are sound in the regime they were designed for (very few huge items, many idle workers) — that regime just isn't this benchmark, and isn't the common case.

## v5.1.0 fixes driven by this benchmark

1. **Agent loop** ([internal/harness/agentloop.go](internal/harness/agentloop.go)) — 4-tool loop (bash/write/http/finish) ported from JS `harness.js`. Without it Go workers returned after one completion and the benchmark could not run at all.
2. **Claim deadlock** ([internal/store/claims.go](internal/store/claims.go)) — `Claim` now admits `in_progress` items and reaps expired leases inside the transaction (aligned with JS `tryClaim`); `reaperLoop` calls `ReapExpired` every 10s as a backstop. Fixes items stuck `in_progress` forever after a lease timeout.
3. **Item divergence** ([internal/store/items.go](internal/store/items.go)) — `CreateItem` dedups worker-created items by extracted file path (`f:` key, ported from JS `dedupKey`/`ARTIFACT_RE`), falling back to exact-title match. Seed callers bypass dedup.
4. **DONE-line token accounting** ([cmd/swarm/runner.go](cmd/swarm/runner.go)) — runner accumulates `totalInputTokens` / `totalOutputTokens` / `totalAPICalls` itself so retired workers' spend is not lost when the reaper evicts them from the map. `apiCalls` now counts real agent-loop HTTP rounds (`lr.Steps`), not turns.
5. **Bench mission constraint** ([bench/bench.ps1](../bench/bench.ps1)) — both sides run with `--mission` pinning scope to the 20 seeded utils, which is what actually stops LLM-driven divergence.

---

# Guards: what stops a run from wasting your time

Every entry below was added **after a real run demonstrated the failure**. The
measurement that motivated it is in the entry. Read this section if you want to
know why swarm is shaped the way it is.

## Coordination (no central scheduler)

Workers do not message each other. They leave traces in a shared environment and
read them — the stigmergy model. There is no lead agent, no global scheduler
deciding who may run in parallel.

| Mechanism | What it prevents |
|---|---|
| **Symbol-level resource keys** (`src/a.js#fn`) | Two items naming different functions of one file are separate items that do **not** conflict, instead of being folded into one or colliding at merge. |
| **Containment conflict matching** | A worker editing `src/a.js` is warned about a claim on `src/a.js#fn` and vice versa. Exact-match `IN (...)` silently missed both. |
| **`needsSplit` on claim** | A 30-minute job becomes N parallel 5-minute jobs. The first worker explores the code and creates one child per file — the operator writes intent, not a file list. |
| **Presplit before workers start** | When the detail *does* name real files, splitting happens before any worker spawns, so the board starts parallel instead of one worker discovering the size the hard way. |
| **Path validation (`looksLikeResource`)** | Prose like "try error.message" produced four junk items (`error.messag`, `error.msg`, `error.error`, `error.detail`), each burning a worker. Only real paths count now. |
| **git-tracked whitelist for splits** | `chat.js` and `openai/http.js` are not files. Splitting along them created children nobody could open. `git ls-files` decides. |
| **Three-way merge arbitration** | A merge conflict used to mean rollback + the item back to the pool + the whole task redone. Now the model resolves base/ours/theirs and the gate still judges the result. Measured: 2 of 8 submissions would have been wasted without it. |
| **Incremental turn brief** | The brief used to carry every event since the last turn (14.5k chars and growing). It now carries only traces on the resources the worker's own items name, plus its own item's notes. |

## Not wasting tokens

| Mechanism | What it prevents |
|---|---|
| **Item-level ledger** (tokens / apiCalls / turnsHeld) | The report printed "3.75M tokens / 0 calls / 0 turns". Now you can see an item burned 2.6M tokens across 80 calls in 2 turns and release it. |
| **`--max-item-seconds` (300)** | One worker held an item for 12 minutes while the rest of the board finished in 5. The lease is released with a FAIL note so the next worker does not repeat the dead end. |
| **Artifact fingerprint breaker** | A worker that produces nothing for 3 turns is parked. The fingerprint counts tracked edits **and** new files **and** staged work — keying it on `git diff HEAD` alone parked workers that were productively creating files. |
| **No-claim-no-submission retire** | A worker that neither claims nor submits for `maxItemTurns` turns retires. Echo harness (or any model that never drives tools) used to grind through all 60 turns. |
| **Autoscaler give-up guard** | When spawns keep producing nothing, the pool stops growing. Combined with skipping the resource guard while giving up — CPU pressure from echo busy-spinning would otherwise reset the pool to 1 worker, a self-sustaining factory. |
| **Scratch cleanup before commit** | `tmp_test_out.txt`, `scripts/_grep.js` and friends made the integrator reject the **entire** submission. One stray file wasted a whole turn. Cleaned automatically; measured 6 saves in one 15-minute run. |
| **bash guard** | Workers wrote `tmp_patch.mjs` and ran `node tmp_patch.mjs` to edit source — ~4× the tool calls versus one `write`. Blocked at the tool layer, because the prompt rule asking for `write` instead was ignored five times in fifteen minutes. |
| **Patch-script warning** | The same behaviour, surfaced in the log when the guard does not catch it. Prompt rules are advisory; visibility is not. |

## Knowing what happened

| Mechanism | What it prevents |
|---|---|
| **`swarm log`** | Stdout is transient. Every event lands in `run_log` with a closed vocabulary (RUN_START, MERGE, MERGE_REJECT, ARBITRATE, GATE, ITEM_*, WORKER_*, WALL_CLOCK) so a later agent can branch on them without parsing English. |
| **Three-layer report** | SEED WORK (what you asked for) vs BONUS WORK (what the swarm found) vs STILL OPEN. Conflating them made a finished run look unfinished. |
| **Seed lineage accounting** | `split` counted as open, and auto-split children were not seed work — so a run where every seed item was split and every child finished reported "2/4 terminal". Both fixed. |
| **`--exit-when-done`** | "Done" means the items you seeded are terminal, not "the board is empty" — workers keep inventing useful work, so an empty board never happens. |
| **Progress deltas on the status line** | `inflight=40960` never showed whether anything was happening. Now every 5s line says `+N done +M subm` or `NO PROGRESS`, plus `STALLED=[w1(np3)]` for workers holding a claim while getting nowhere. |
| **Resume without redo** | Finished items are not re-claimable. A 50-minute job split across three runs costs three runs' worth of work, not 3× the work. The report states exactly what resumes. |

## When the run ends

| Mechanism | What it prevents |
|---|---|
| **Silence timeout (`--idle-stop`, 10m)** | A fixed wall clock kills a big job that was still moving and keeps a dead one alive its full length. Progress (merge / submit / terminal item) resets the timer, so the ceiling adapts. |
| **Hard ceiling (`--max-wall-clock`, 6h)** | Backstop only, for pathological cases like a worker that submits junk every 9 minutes. Deliberately generous so it is not the binding constraint. |
| **`--exit-when-done`** | Small jobs leave in minutes instead of waiting out a ceiling. |

## Measured-and-rejected

Kept so these don't get re-proposed without new evidence. See the benchmark
tables above: aggressive-split (+51% wall), speculative racing (+86% wall,
2.6× tokens), both (+253% wall, 3.9× tokens).

## Still open

- **git ref claim** (replace the SQLite single-writer): the throughput gate for 100+ workers. Not needed below that — three real runs showed no write-lock contention at 3–5 workers.
- **LLM review of large diffs**: merge a >N-line change only after a model reviews it.
- **`protectedPaths`**: require operator confirmation before certain files change.
