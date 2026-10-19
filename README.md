# swarm-go

Go port of the [agent-swarm](../swarm) multi-agent parallel dev orchestrator. Workers claim items off a SQLite-backed board, drive an LLM through a 4-tool agent loop (bash / write / http / finish), and an integrator merges their worktrees.

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

## v5.1.0 fixes driven by this benchmark

1. **Agent loop** ([internal/harness/agentloop.go](internal/harness/agentloop.go)) — 4-tool loop (bash/write/http/finish) ported from JS `harness.js`. Without it Go workers returned after one completion and the benchmark could not run at all.
2. **Claim deadlock** ([internal/store/claims.go](internal/store/claims.go)) — `Claim` now admits `in_progress` items and reaps expired leases inside the transaction (aligned with JS `tryClaim`); `reaperLoop` calls `ReapExpired` every 10s as a backstop. Fixes items stuck `in_progress` forever after a lease timeout.
3. **Item divergence** ([internal/store/items.go](internal/store/items.go)) — `CreateItem` dedups worker-created items by extracted file path (`f:` key, ported from JS `dedupKey`/`ARTIFACT_RE`), falling back to exact-title match. Seed callers bypass dedup.
4. **DONE-line token accounting** ([cmd/swarm/runner.go](cmd/swarm/runner.go)) — runner accumulates `totalInputTokens` / `totalOutputTokens` / `totalAPICalls` itself so retired workers' spend is not lost when the reaper evicts them from the map. `apiCalls` now counts real agent-loop HTTP rounds (`lr.Steps`), not turns.
5. **Bench mission constraint** ([bench/bench.ps1](../bench/bench.ps1)) — both sides run with `--mission` pinning scope to the 20 seeded utils, which is what actually stops LLM-driven divergence.
