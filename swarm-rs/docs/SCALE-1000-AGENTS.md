# 1000-agent 规模审查

> 版本：v1.0（2026-11-01）
> 触发：`max_workers` 默认值就是 1000（`RunConfig::default`，与 Go 一致），说明这个规模是设计目标而非理论值。
> 方法：通读 1000 worker 下每条会执行的路径，找"在小规模看不出来、在千级会断"的东西。

---

## 1. 发现并修掉的两个缺陷

### 1.1 缩容速度是扩容的 1/64

**证据**（Go `cmd/swarm/runner.go`）：

```go
maxSpawnPerRound := 64          // L1619
...
if need > maxSpawnPerRound {    // L1692
    need = maxSpawnPerRound
}
...
// high pressure: freeze; critical: shrink by 1.
if sample.CPUPct > 92 || sample.MemPct > 92 {
    want = active - 1           // L1681
}
```

扩容每 tick 最多 64 个，缩容每 tick 恰好 1 个。autoscaler tick 是 1s（Go L1626，Rust 一致）。

**后果**：`max_workers = 1000` 时，主机越过 92% 内存后——

| 方向 | 速率 | 1000 worker 耗时 |
|---|---|---|
| 扩容 | 64/s | ~16s |
| 缩容 | 1/s | **~1000s（16 分钟）** |

guard 12 的唯一职责是防止 OOM kill，而它的响应速度比它要防的故障慢 64 倍。**这是 Go 版就存在的缺陷，Rust 忠实移植了它。**

**修法**：`decide()` 里把缩容批量化。刻意**不改** `desired_workers()`——那里保持 Go 的"one fewer"形状，让"每个 worker 的决策"仍然可读；不对称的补偿放在 `decide()`，紧挨着它必须匹配的那个 64 上限。

```rust
let gap = active - want;
let batch = if load.is_critical() {
    gap.saturating_mul(64).min(active / 4).max(1)
} else {
    gap
};
ScaleDecision::Shrink(idle.min(batch).min(64))
```

`active / 4` 是每 tick 最多缩掉四分之一，`idle.min(...)` 保证只退空闲 worker——正在干活的 worker 占着 worktree 和 live claim，不能从它脚下撤。

**测试**：`shrink_keeps_pace_with_spawn`（500 全空闲 + 内存临界 → 必须 > 1 且 ≤ 64）、`critical_shrink_only_retires_idle_workers`（500 active 仅 3 idle → 只退 3）。

### 1.2 `sysload::sample()` 阻塞 autoscaler 的 async loop

`sample()` 要睡 250ms 才能算出 CPU 增量（`sysload.rs` 文档明说）。它在 `autoscaler_loop` 里被**同步调用**，于是这 250ms 内整个 loop 的 `select!` 全部停摆——包括取消检查。

在 1000 worker 下这条尤其致命：autoscaler 是唯一能缩容的东西，它卡 250ms 就等于缩容延迟 25%。

**修法**：`spawn_blocking` 挪到阻塞线程池。tick 是 1s，晚 250ms 拿到样本毫无代价。

---

## 2. 审查过但确认没问题的

| 路径 | 1000-agent 下的风险 | 结论 |
|---|---|---|
| `CancellationToken` | 1000 worker = 1000 个 50ms 定时器 | **已修**（本轮之前改为 `Notify`，零定时器） |
| `observe()` / `digest` | 每 worker 每轮 5+ 连查，占满 tokio worker 线程 | **已修**（本轮之前改为 `spawn_blocking`） |
| `Store` 并发 | 单 `Mutex<Connection>`，对应 Go `SetMaxOpenConns(1)` | 设计如此；串行写是避免 "database is locked" 的手段，不是疏漏 |
| `resource_leases` | 千级并发 claim 的资源冲突扫描 | 事务内完成，无额外往返 |
| `workers: AsyncMutex<Vec<WorkerState>>` | autoscaler 每 tick 全量遍历 | O(n) 但 n=1000 可忽略；且只在 1s tick 上 |
| worktree 创建 | `add_worktree` 串行 await | 受 git 自身限制，并行也快不了；spawn 上限 64/tick 已是天然节流 |
| `ItemLedgers` | 长跑内存增长 | 已有 `remove()`，item terminal 即清理 |

---

## 3. 仍未验证的（需要真实压测）

以下三项**无法用单测证明**，只能靠真跑：

1. **AIMD 退避路径** —— M4 基准 4-8 并发全程 0 次 429，`note_rate_limit` 从未触发。需压到 32-64 并发。
2. **千级 worker 的启动延迟** —— 64/tick 的 spawn 上限下，1000 worker 需 ~16s 才能全部就位；期间 `active_workers` 计数与真实进度的偏差需要实测。
3. **缩容批量化后的稳态** —— 四分之一批次的缩容会不会在临界值附近震荡（缩到 91% → 放量 → 又到 92% → 再缩）。理论上有此风险，需要真机观察；若震荡明显，应加滞回（hysteresis）。

---

## 4. 已做的两次真实压测

### 4.1 echo harness，200 item，`--workers 64 --max-workers 1000`

| 观测 | 值 |
|---|---|
| worktree 峰值 | 64（= `--workers`，未继续爬） |
| RSS 峰值 | 119 MB |
| CPU 累计 | 3.4 s |
| guard 11 | 正确触发：`giving up after 64 stalled spawns` |

**结论**：echo harness 下 guard 11 在 3 次 stalled spawn 后就 `GiveUp`，所以 worker 数停在初始的 64，爬不到 1000。这是**正确行为**（端点不驱动工具时不该无限扩容），但也意味着 echo 压测**测不到千级并发**。64 worker + 119MB 的资源曲线是健康的。

### 4.2 真模型，300 item 全争抢同一个文件，`--workers 8 --max-workers 1000`

这是资源租约锁的极端场景：300 个 seed item 的 title 全部解析到同一个资源键 `readme.md`。

| 观测 | 值 |
|---|---|
| worktree 峰值 | **304** |
| RSS 峰值 | **48 MB** |
| CPU 累计 | 12.8 s |
| done / failed | 3 / **0** |
| accepted | 3/5（60%），2 次 gate 拒绝 |
| tokens | 289,556 in / 7,742 out / 41 api calls |
| 崩溃 | **无**（12 分钟 wall-clock 正常收尾） |

**304 个 worker、48MB 内存、零崩溃** —— 编排层在三百级并发下是稳的。

**但暴露出一个真实的效率问题**：12 分钟里 304 个 worker 只有 1 个真正在干活，301 个在排队。原因就是资源锁本身——300 个 item 争同一个 `readme.md`，任何时候只有一个 peer 能持有它。这是**锁的正确行为**，不是缺陷：同样的 seed 在任何实现下都只能串行。

不过 worker 的行为暴露了两个可以改进的点（都是 worker 自己在 event 里诊断出来的）：

1. **`/marks` 不可靠**：多个 worker 报告 `GET /marks?resource=README.md` 返回 count 0，即使锁正被持有。worker 因此无法判断该不该重试，只能盲轮询。`/marks` 反映的是 intent mark，而资源租约是独立的新机制——两者没有打通。
2. **worktree 基线过期导致 merge conflict**：2 次 gate 拒绝都是 merge conflict，根因是 worker 从 `base` 分叉后直接 append，没有先合并 integration。worker 自己总结出了正确流程（先 `git merge integration` 再 append 到文件末尾）。

这两点都指向同一处：**worker 缺一份关于资源锁的可靠说明**。`/marks` 与 `resource_leases` 语义不一致会让模型做出错误决策，而这正是 Super-agent 公理一（最小充分披露）要解决的问题——不是加更多提示词，而是让系统提供的事实本身一致。

### 4.3 已修：`/marks` 现在同时报告两种机制

`active_marks` 原来只读 intent mark（Go 的行为）。资源租约升级后，"谁持有这个资源"有了两个答案，而 `/marks` 只说其中一个——于是 worker 看到 `count: 0` 就去 claim，撞上 409 再盲轮询。

现在 `/marks` 一次返回两者，并给出 worker 真正该看的字段：

```json
{
  "resource": "README.md",
  "count": 0,          // intent mark 数（保持 Go 兼容语义）
  "held": 1,           // 租约数——决定"能不能 claim"的是这个
  "isHeld": true,
  "marks": [...],      // 提示（hint）
  "leases": [...]      // 权威（authority）
}
```

`count` 保持原义以免破坏既有客户端；`held` / `isHeld` 是新增的权威字段。

**过程中踩到并修掉一个自死锁**：第一版让 `active_marks` 调 `resource_leases_for()`，而后者自己再去拿 `Store` 的 `Mutex`——`active_marks` 已经持有了。这个锁不可重入，于是 `/marks` 测试**挂住而不是失败**。这正是 README 约束和 `SWARM-RUN-ISSUES.md` 记录过的那个坑（`reap_expired` 也踩过），我又踩了一遍。改成自由函数 `resource_leases_tx(&conn, ...)` 接收调用方已持有的 guard，并补了回归测试 `marks_does_not_deadlock_reading_both_halves`。

**仍未修**：worktree 基线过期导致的 merge conflict（§4.2 第 2 点）。那需要 worker 在编辑前同步 integration——属于 worker 行为引导，不是系统事实不一致，优先级低于本条。

---

## 7. 半成品交接与失败拆分（v1.2 修完）

304-worker 压测和随后的代码审查暴露出同一类根因：**系统只记录结果，不记录过程。**

| # | 缺陷 | 后果 | 修法 |
|---|---|---|---|
| 1 | guard 9 释放 item 但**不释放租约** | item 显示 `pending`，别的 worker 去抢撞 409；`renewer_loop` 还不断续期，最多卡 300s | 释放时一并 `release(&token)`。**git 工作不受影响**——commit 在 worker 分支上，未提交改动在磁盘上 |
| 2 | `artifact_fingerprint` 算完即弃 | "上次干到哪"对下一个人完全不可见 | 改为 `progress()`：文件清单 + head sha，同一个 read 同时喂 guard 8 和交接 |
| 3 | FAIL 事件只有一句话 | 下一个人从零开始，且**通常重复刚才失败的方案** | 事件文本带上 `changed N file(s): a.js, b.js (head abc1234)` |
| 4 | 反复 gate 失败不拆分 | 换人重试同一方案，直到 `max_item_fails` 判死 | gate 拒绝时 `split_failed_item()`，拆成 per-resource 子项各自过门禁。`split_item` 幂等 + 深度上限，不会炸板 |
| 5 | merge 不告知模型基线变了 | 模型按读到的旧内容整文件覆写，制造 merge conflict | brief 里加 `INTEGRATION MOVED: N commit(s) landed...`，要求覆写前重读 |

第 2、3 项是同一个改动的两面：把已经在算的 fingerprint 从"喂完 guard 8 就扔"变成"同时作为交接材料"。

第 5 项值得特别说明：merge **确实**每轮都做（guard 5），所以 worktree 本身是最新的——但模型不知道。它的失效模式是"读文件 → 形成计划 → 整文件覆写"，如果这期间有 peer 合入了同一文件，覆写就静默回退对方的工作。这是信息问题不是同步问题，所以解法是在 brief 里把基线变化说出来，而不是再加一次 merge。

**测试**：`a_guard9_released_item_is_immediately_reclaimable`（释放后立刻可被抢）、`a_released_item_hands_over_what_was_done`（交接说明点名文件和 head）。

---

## 8. v1.2 对比压测：一次修复无效，以及为什么

同一场景、同参数重跑（300 item 全争抢 `README.md`，8 初始 worker，12 分钟墙钟），对比修复前后：

| 版本 | done | rejected | accepted 率 | api calls | tokens in |
|---|---|---|---|---|---|
| v1.1（修复前） | 3 | 2 | 60% | 41 | 289,556 |
| v1.2（我以为修好了） | 3 | **5** | **38%** ↓ | 227 | 2,480,324 |
| v1.3（真正修好） | **7** | 4 | **64%** | 1,873 | 22,713,314 |

### 8.1 v1.2 反而更差——因为基线提示根本没生效

v1.2 的 5 次拒绝**全是 merge conflict**，和 v1.1 同一个病。而 worker 自己在 event 里写：

> "every worktree starts at stale commit 7258967 'base' with README.md='base'... FIX: `git fetch . integration && ...`"

**它们自己诊断出了根因和正确解法，但系统从未告诉过它们。**

原因是我写的 bug：`baseline_note` 在 merge **之后**才读 worktree HEAD。

```rust
// 错：merge 已经把 HEAD 挪到 integration 了
merge(integration).await;
let before = head(worktree);              // ← 已经是合并后的 HEAD
rev_list(before..integration)             // ← 永远是 0，提示永不触发
```

修法：把 `head_before` 的读取挪到 merge **之前**，作为参数传进去。

### 8.2 三个修复只有一個真正被验证

| 修复 | v1.3 是否触发 | 原因 |
|---|---|---|
| 基线提示 | ✅ **生效**（拒绝数从 5 降到 4，done 从 3 升到 7） | — |
| guard 9 释放租约 | ⚪ 未触发 | 300 个 item 抢 1 个文件，292 个 worker **从来没 claim 到过**，走的是 guard 4/8/11 而非 guard 9 |
| 失败触发拆分 | ⚪ 未触发 | 同上；且 `board` 显示 0 个 split |

**这是重要教训：我为一个场景写的修复，被另一个场景的基准"验证"了。** v1.2 我声称"修好了"，实际只对了一半——而如果只看 accepted 率从 60% 掉到 38%，我可能会误以为是自己其他改动引入的回归。

### 8.3 为什么 accepted 率不能单独作为指标

v1.3 的 accepted 率（64%）和 v1.1（60%）看起来差不多，但 **done 从 3 变成 7（+133%）**。原因是 v1.3 让 worker 活得久得多（1873 次 api 调用 vs 41 次），有更多机会完成——`autoscaler-give-up=188` 说明大量 worker 因为抢不到文件而空转退出。

所以这个场景真正的瓶颈是**资源争抢**，不是接受率。300 个 item 挤在一个文件上，任何实现的吞吐都被锁串行化——这也正是租约锁的正确行为。

### 8.4 测试缺口

`the_baseline_warning_fires_when_integration_moved` 是这次补的回归测试：它直接调用 `baseline_note_for_testing` 并断言提示非空。原来的实现会返回空字符串，这个测试会红。

**教训**：v1.2 我加了"半成品交接"的测试（那个有 teeth，直接断言文件列表），但基线提示只加了逻辑没加测试——所以它坏了而我不知道。**没有测试的修复等于没修。**

---

## 9. guard 9 验证场景：又抓出两个缺陷

换一个针对性场景重跑：**12 个 item，各自独立文件**（无资源争抢，worker 真的能 claim），`--max-item-turns 2` 迫使 guard 9 触发。

结果：**19 done / 0 failed / accepted 66%**——场景本身成功了。但两个修复仍未按预期工作，深挖发现两个真缺陷：

### 9.1 缺陷：拆分对"单文件任务"永远无效

`split_item` 要求 **≥2 个真实资源**才拆。而"创建 src/alpha.rs 三个函数"这类任务只命名 1 个文件，所以永远返回 `false`。

更糟的是它**静默失败**——调用方以为拆了，实际没有。失败触发拆分这条路对这个场景完全没作用，而表面上看起来"正常"。

已在代码里注明这是有意的设计选择（单文件无法按资源拆），并让调用方能区分"拆了"和"没法拆"。

### 9.2 缺陷：merge conflict 也会触发拆分（方向相反）

反过来也错：原来的逻辑对**任何** gate 失败都尝试拆分。而 merge conflict 的含义是"两个 worker 写了同一区域"——把 item 拆成更多子项会让冲突**更容易发生**，不是更少。

现在只有"任务太大"类失败才拆；conflict 走另一条路（租约 + 基线提示 + 下面的补救说明）。

### 9.3 缺陷：conflict 拒绝原因不可操作

`RepoError::Conflict` 的 `to_string()` 就是 `"merge conflict"` 四个字。下一个人拿到这句话，不知道该干什么。

而 worker 自己在 300-item 压测里已经总结出了正确做法：

> "Re-read the file from the integration branch, then append your change instead of rewriting the whole file from an older copy."

现在这句补救说明由**系统**追加到拒绝原因里，而不是靠每个 worker 各自踩坑重新发现。

### 9.4 这轮的真实收获

| 缺陷 | 怎么发现的 |
|---|---|
| 基线提示不生效 | v1.2 对比压测（accepted 反降） |
| 拆分对单文件无效 | guard 9 场景（0 个 split） |
| conflict 也触发拆分 | 读代码（方向性错误） |
| conflict 原因不可操作 | 读 worker 的 event 自诊断 |

**两个缺陷是被"针对性场景"逼出来的，不是被单元测试**。这也说明：修一个功能需要一个能让它触发的场景，否则你只是在改自己想象中的一个 bug。

### 9.5 仍未验证

guard 9 的租约释放在这个场景里**依然没触发**（`--max-item-turns 2` 下 worker 都在 2 轮内完成或放弃，没熬到 guard 9）。它的正确性目前只有单测保证。要触达需要让任务真的很难——比如要求 20 轮才能做完的事。

---

## 10. guard 9 终于触发了——以及一个更严重的发现

### 10.1 又一轮失败

换 `--max-item-seconds 45`（墙钟判据，不依赖任务难度），任务改成"创建 20 个函数"确保 worker 会长期持有。结果：**7 done / 0 failed / accepted 78%**，但 `breakers: none`——guard 9 仍未触发。

### 10.2 根因：`advance_hold` 从来没有生产调用者

```rust
pub fn advance_hold(&mut self, d: Duration) { ... }
```

这个方法有 **5 个单元测试**，`runner.rs` 里**一次都没调用**。所以 `held_for` 永远是 `Duration::ZERO`，guard 9 的时间判据**结构上不可达**——`--max-item-seconds` 一直是个静默的空操作。

这不是"难触达"，是"永远不可能触发"。而 5 个测试全绿，因为每个测试都自己调 `advance_hold`。**这正是本项目反复出现的同一个教训的第三次实例**：

> `SWARM-RUN-ISSUES.md` §4：「263 个测试全绿和'功能是空的'完全可以同时成立。」

前两次是 `observe()` 的假字段和 `evaluate_post_turn` 的假 `turn_ok`，都是"有测试、无接线"。这次是"有测试、无调用者"。

### 10.3 修复

在 `worker_loop` 里记录 turn 开始时间，turn 结束后 `cb.advance_hold(turn_started.elapsed())`。同时把时间判据抽成自由函数 `hold_outran_clock(held_for, cfg)`，让"0 表示禁用"这个语义可被直接测试。

### 10.4 验证成功

同参数重跑，guard 9 真实触发，交接说明带上了具体信息：

```
FAIL w3 — released after over the per-item limit — do not repeat this approach;
try a different one (held 169.26s > 45s limit).
Partial work to build on: changed 1 file(s): examples/demo.rs (head ebd582df1b)
```

对比修复前，下一个人现在知道：

- 上次卡了多久、超限多少
- **改了哪个文件**
- **哪个 commit**（可以直接 `git show ebd582df1b` 看上次的尝试）

而不再是一句"换个方式试试"。

### 10.5 一个诚实的保留

我写的回归测试最终**没有牙齿**。原打算断言"runner 会把耗时记到 hold 上"，但 `world_snapshot` 用的是合成 peer，看不到真实 worker 的 claim——断言不了。最后的版本只测了 `hold_outran_clock` 这个纯函数（它本来就有覆盖），加了一条"claim 存在性"的弱断言。

**这个 bug 是真实运行逼出来的，不是测试。** 单测抓不到它，因为缺陷恰好在"测试自己提供了被测对象"这件事上。

---

## 5. 结论

| 规模 | 状态 |
|---|---|
| 64 worker（echo） | **已验证**，119MB，guard 11 正确触发 |
| 304 worker（真模型） | **已验证**，48MB，0 崩溃，0 failed |
| 1000 worker | **未触达** —— 需求（item 数）是真实上限，echo 下 guard 11 会提前放弃 |

**1000 worker 需要 1000 个独立 item 才能触达**，且 echo harness 测不到（guard 11 拦截）。要真测千级，需要：≥1000 个分散在不同文件上的 item + 真模型。这会消耗可观的 token，建议作为单独的压测任务排期。

已修复的两个缺陷（缩容 1/64 不对称、`sysload` 阻塞 async loop）在 304 并发下均未表现异常，因为内存压力从未接近 92%——它们的正确性目前由单测保证，尚待高压环境验证。

---

## 6. AIMD 闸门：从未被执行过的路径

M4 基准 4-8 并发全程 0 次 429，所以 `note_rate_limit` 这条退避路径**从未在真实系统里跑过**。`aimd.rs` 有 421 行实现和一批单测，但每个测试只驱动一次状态转移。

### 6.1 一次误判（记下来因为它有教育意义）

通读 `release_one` 时，我认为发现了 slot 泄漏：

```rust
async fn release_one(&self) {
    { let mut s = self.state.lock().await; if s.active > 0 { s.active -= 1; } }
    self.handoff().await;   // ← handoff 里又 active += 1
}
```

"先减后加，净变化为零，那 slot 到底有没有释放？"——看起来是双重计数。

**我错了。** 写了两个测试想证明泄漏，结果都通过：slot 是**转移**而不是复制。减一让出容量，`handoff` 把这个容量交给等待者并重新计数，等待者自己的 `GateGuard` 在 drop 时再减一次。链条上每次 drop 都精确平衡。

教训：**"看起来像双重计数"和"是双重计数"是两件事**，而这里的代码注释原本就没解释这个不变量——所以下一个读代码的人（我）会重新怀疑一遍。已在 `release_one` 上补写注释说明为什么这是对的。

### 6.2 补的测试

| 测试 | 锁住什么 |
|---|---|
| `release_frees_capacity_for_the_next_acquirer` | 连续 5 次 acquire/release 不堵塞 |
| `run_does_not_leak_a_slot_per_call` | limit-2 闸门跑 6 次后两个 slot 仍空闲 |
| `concurrent_runs_conserve_capacity_and_survive_a_429` | **40 并发 + 中途 429**：全部结算（无死锁）且结束时 `active == 0` |

第三个是真正的新覆盖：它按 runner 的方式驱动整个状态机——饱和、退避、恢复——而不只是一次转移。

### 6.3 仍未验证

真端点上的 429 退避。要触达需要把并发压到端点限流（估计 32-64）。上面的测试证明闸门**内部**正确，但没证明它在真实 429 下的恢复行为符合预期。

---

## 11. AIMD 压测：可观测性补上，然后暴露出第 13 个缺陷

### 11.1 先补可观测性

验证 AIMD 的前提是能看见它。原来 `aimd.rs` 有 15 个单测，但**运行时的 limit / ceiling / 退避次数完全不可见**——报告里只有 `breakers: none`，无法区分"端点健康"和"闸门从没触发"。

新增：

- `AdaptiveGate::{active_now, ok_streak, in_slow_start, rate_limit_count, backoff_count}` —— 非阻塞读取
- `Runner::gate_state()` + `GateState` 快照，每轮 turn 后采样
- 报告新增一行：`llm gate: limit=64 ceiling=64 slow_start=true rate_limits=0 backoffs=0`

### 11.2 两次压测，第二次才是有效的

| 轮次 | 配置 | 结果 |
|---|---|---|
| 第一次（设计错误） | 121 worker / 120 item | **无效**。72 次 API 调用 / 10 分钟——worker 在到达 LLM 前就因"抢不到 item"全部 sleep 1 秒，闸门从未被使用 |
| 第二次（修正） | 96 worker / 600 item | **有效**。208 次 API 调用 / 8 分钟，96 个 worker 全程持有 item |

第二次的数据：`limit=128 ceiling=128 rate_limits=0`。

**端点确实不限流**（208 次调用零 429），所以退避路径仍未触达——但暴露了别的问题。

### 11.3 第 13 个缺陷：闸门的上限比 worker 总数还大

`initial_window(max_workers)` = `clamp(max_workers*4, 8, 64)`，而慢启动每 8 次成功**把 ceiling 也翻倍**。96 个 worker 的池子：起点 64 → 翻倍到 128。

**128 > 96。闸门的并发上限超过了 worker 总数**，意味着它永远不可能成为瓶颈——而这正是它存在的唯一目的。

第一次修的时候我只 clamp 了 ceiling，没挡慢启动的翻倍，测试立刻抓到（`limit` 从 4 涨到 8）。真正的修法是加 `ceiling_floor`：

```rust
s.ceiling = s.ceiling.saturating_mul(2).min(s.ceiling_floor);
```

`pin_ceiling` 同时装 floor（挡慢启动翻倍）和即时 clamp（挡已经超限的 gate），且**永不放宽** live limit。

### 11.4 结论

| 项 | 状态 |
|---|---|
| 闸门可观测 | ✅ 已补，报告直接打印 |
| 闸门上限受池子约束 | ✅ 已修（`ceiling_floor`） |
| 真 429 退避行为 | ✅ **已用本地假端点验证**（见 §11.5） |

### 11.5 本地假端点：把最后一项也验了

端点不限流是环境问题，但只要起一个必然返回 429 的本地服务，就能完整驱动"HTTP 429 → 客户端重试 → `HarnessError` → `note_rate_limit()` → limit 下降 → 恢复"这条链。

新增三个测试：

| 测试 | 覆盖 |
|---|---|
| `a_429_is_retried_before_surfacing` | 429 是**瞬态**的：客户端重试，前两次失败第三次成功。与 400 的处理正好相反 |
| `a_persistent_429_surfaces_with_its_status` | 持续 429 必须带 429 状态冒出，调用方才能区分"被限流"和"请求格式错" |
| `a_throttling_endpoint_drives_the_gate_end_to_end` | **完整闭环**：前两轮 429 → limit 从 16 掉到 1；第三轮端点恢复 → 调用成功（证明闸门没被退避卡死） |

### 11.6 过程中修的第 14 个缺陷：storm 吞掉了计数

写闭环测试时发现 `rate_limit_count` 少算：两个 429 只记了 1 个。

根因是 `note_rate_limit` 里的 storm 检查**在计数器之前 return**：

```rust
if now.duration_since(last) < STORM_WINDOW {
    return; // storm: the first rejection already backed off
}
...
self.rate_limits.fetch_add(1, ...);   // 永远到不了
```

storm 窗口去重的是**退避动作**，不是**观测**——一次被折叠的风暴仍然是端点限流的证据。把计数器提到函数开头，风暴内外都计数。

这个缺陷和前面 13 个是同一类：**一个看起来在工作的路径，实际漏了一半**。只不过这次是"计数"漏了而不是"功能"漏了，所以更隐蔽——报告会显示 `rate_limits=1`，看起来合理，实际发生了两次。
