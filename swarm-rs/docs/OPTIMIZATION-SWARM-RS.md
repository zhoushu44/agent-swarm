# swarm-rs 优化建议（自审版）

> 版本：v1.0（2026-11-01）
> 对象：`swarm-rs/`，10604 行 / 263 测试 / 0 警告
> 方法：**审自己刚写的代码**。以下每一条都有行号或可复现证据，不含"以后可以更好"这类空话。
>
> 分级沿用 Go 版文档：`[BUG]` 实际错误、`[RISK]` 特定条件下坏、`[PERF]` 性能、`[DESIGN]` 结构性。

---

## 0. 速览

| # | 问题 | 类型 | 后果 | 优先级 |
|---|---|---|---|---|
| 1 | `observe()` 把 6 个关键字段硬编码为 0 | BUG | **guard 2 / 9 / 12 / 13 全部失效** | P0 |
| 2 | gate 失败原因被 `e_reason()` 丢弃 | BUG | 每个 rejection 都写同样的假原因 | P0 |
| 3 | `sysload` 未实现 | BUG | 资源守卫永不触发，可能 OOM | P0 |
| 4 | `ItemLedgers` 从不写入 | BUG | token 账本永远是空的，报告失真 | P0 |
| 5 | `stalled_spawns` 从不递增 | BUG | guard 11 永不触发 | P0 |
| 6 | `workerLoop` 从不 claim / submit | BUG | echo 之外什么都不会发生 | P0 |
| 7 | `Runner.dir` / `gate` 字段已删但 `AdaptiveGate` 未用 | DESIGN | AIMD 闸门形同虚设 | P1 |
| 8 | 熔断器状态在 spawn 边界丢失 | DESIGN | guard 7/8/9 的计数随 worker 重建归零 | P1 |
| 9 | `CancellationToken` 用 50ms 轮询 | PERF | 每个循环多一个定时器 | P2 |
| 10 | store 用阻塞 `Mutex` 包在 async 里 | PERF | 长时间查询会堵住 runtime worker | P2 |

---

## 1. P0：五个"测试全绿但功能是空的"

这是最严重的一类问题：**263 个测试全过，但它们测的是纯函数，没测接线。**

### 1.1 `observe()` 把 6 个关键字段硬编码为 0 `[BUG]`

`runner.rs` L809-821：

```rust
World {
    tokens_spent: 0,
    held_item_tokens: 0,
    active_workers: 0,
    cpu_pct: 0.0,
    mem_pct: 0.0,
    queued_submissions: 0,
    ...
}
```

**后果**——这些字段直接决定四条熔断器是否触发：

| 字段 | 失效的 guard | 表现 |
|---|---|---|
| `tokens_spent` | guard 2（budget） | `--budget-tokens` 完全无效，超预算也不停 |
| `held_item_tokens` | guard 9（item token） | 单 item 烧 2M token 不会释放 |
| `cpu_pct` / `mem_pct` | guard 12（资源） | 机器打满也不会缩容 |
| `active_workers` | guard 1 / 6 | "是不是最后一个 worker" 永远判断错 |
| `queued_submissions` | guard 13（exit-when-done） | 还有活没干就宣布 seed-done |

`observe()` 明明拿到了 `claimable` / `open` / `my_claims` / `seed_*`，却漏了这六个——因为我在写 `World` 时**先填了容易拿的，难的留了个 0**，而测试只测了 `CircuitBreaker` 本身（喂手工构造的 `World`），没有一个测试走到 `observe()`。

**建议**：给 `WorkerHandle` 加一个 `world()` 方法，把六个字段从真实来源填上；再加一个测试，断言 `observe()` 产出的 `World` 里 `active_workers > 0`、`tokens_spent >= 0` 等非平凡值。

### 1.2 gate 失败原因被 `e_reason()` 丢弃 `[BUG]`

`runner.rs` L458 和 L614：

```rust
let reason: String = e_reason().chars().take(400).collect();
...
/// Placeholder for a gate failure's reason; the real reason is captured by the
/// caller.
fn e_reason() -> String {
    "gate failed".to_string()
}
```

**后果**：`repo.gate()` 返回的 `ErrGate { command, output, err }` 里带着**完整的测试输出**（这正是排障唯一需要的东西），但我在 `tokio::spawn` 的闭包里为了 `'static` 把它丢成了一句 `"gate failed"`。于是：

- `resolve_submission(id, false, "gate failed", 3)` → db 里每条 rejection 都是同一句
- FAIL context 事件里没有任何测试输出
- 运营方看到 409 只能干瞪眼

注释还写着"the real reason is captured by the caller"——** caller 并没有 capture**。这是我自己写的假注释。

**建议**：把 `repo.gate()` 的 `ErrGate` 在 spawn **之前** `to_string()`，把 String move 进闭包。

### 1.3 `sysload` 未实现，资源守卫永不触发 `[BUG]`

`autoscaler.rs` 用 `LoadSample::default()`：

```rust
autoscaler.decide(open, recent, active, idle, LoadSample::default())
```

`LoadSample::default()` 的 cpu/mem 恒为 0.0 → `above_critical()` 永假 → `allows_growth()` 永真。

**后果**：Go 版有 `sysload` 包（windows/linux 双实现，85/90/92 三个阈值），Rust 版完全没搬。几百个 worker 同时 `go test` 时，Rust 版不会缩容，**会把机器拖到 OOM**。Go 版这条是生产验证过的保护。

**建议**：实现 `swarm-cli::sysload`：
- Linux：读 `/proc/stat` + `/proc/meminfo`
- Windows：`GlobalMemoryStatusEx` + `GetSystemTimes`
- 加 `#[cfg(windows)]` / `#[cfg(unix)]` 分支，测试注入假采样

**这是 Rust 版目前最危险的缺口**——它不是"少个功能"，是"少了生产保护"。

### 1.4 `ItemLedgers` 从不写入 `[BUG]`

`runner.rs` L563/L594 只在 `report()` 里读，全文件没有任何 `ledgers.add(...)`。

**后果**：
- `guard 9` 的 token 判据失效（因为 `held_item_tokens` 也硬编码 0，双重失效）
- 报告的 "top token spend" 永远是空
- `tokens per done item` 用的却是 `totals`，所以那个数字对——但 top-N 表是假的

我在 `worker.rs` 里认真写了 `add` / `remove` / `top_by_tokens` 和 4 个测试，然后在 `runner.rs` 里忘了调用。**测试测了数据结构，没测接线。**

**建议**：worker 每轮 turn 结束后，按当前 claim 调 `ledgers.add(item_id, tokens, api_calls)`；item terminal 时 `remove`。

### 1.5 `stalled_spawns` 从不递增，guard 11 永不触发 `[BUG]`

`Autoscaler` 有 `note_stalled_spawn()`，`CircuitBreaker` 也有，但 `runner.rs` 里**没有任何调用点**。

**后果**：Go 版 L1665-1670 的 give-up guard（连续 3 次 spawn 无产出就停止扩容）在 Rust 版不存在。如果模型不驱动工具，Rust 版会**无限 spawn worker 到 maxWorkers=1000**，Go 版会在 3 次后停。

**建议**：worker loop 退出时，若整个生命周期内没有 claim 也没有 submit，调 `autoscaler.note_stalled_spawn()`。

### 1.6 `workerLoop` 从不 claim / submit `[BUG]`

整个 `worker_loop` 只调了 `store.claimable_count()` / `open_count()` / `active_claims_by_peer()` / `digest()`——**从不 `claim()`、从不 `submit()`**。

**后果**：echo harness 下能跑（因为不产出文件，guard 8 会 park 它），但接真模型时：
- worker 读看板、生成 prompt、调 LLM
- 模型调 `http` 工具 → 打到 `127.0.0.1:17771` 的 API → claim 成功
- 但 runner 侧完全不知道，`held_item` 永远是 None
- 于是 guard 4（no-claim-no-submit）会在 6 轮后把 worker retire 掉

也就是说：**真模型下每个 worker 会在第 7 轮被误杀**，因为 runner 不跟踪 claim。

**建议**：turn 结束后调 `store.active_claims_by_peer(&peer)` 更新 `cb.note_held_item(...)`。这正是 Go 版 L838-857 在做的事。

---

## 2. P1：设计层

### 2.1 `AdaptiveGate` 建了但没用 `[DESIGN]`

`Runner::new` 里 `let gate = AdaptiveGate::new(...)` 然后字段被我删了（因为 warning）。结果：AIMD 闸门、慢启动、429 退避全都没接上。

**后果**：所有 worker 同时打 LLM，第一次 429 不会有任何退避。Go 版这是核心的容量治理。

**建议**：`worker_loop` 里包一层 `gate.run(|| ...)`，成功 `note_ok()`，429/503 `note_rate_limit()`。

### 2.2 熔断器状态在 spawn 边界丢失 `[DESIGN]`

`spawn_worker` 每次 `CircuitBreaker::new(...)`，worker 退出后状态随之丢弃。

**后果**：guard 7/8/9 的 streak 计数在 worker 重建后归零。Go 版把这些计数存在 `workerState` 里，worker 退出就释放——这点两边一致，但 Go 版的 `stalledSpawns` 是 **runner 级**的，跨 spawn 累积；Rust 版虽然也有 runner 级的 `Autoscaler.stalled_spawns`，但没有递增点（§1.5）。

### 2.3 `report()` 的 `tokens per done item` 语义不清 `[DESIGN]`

`report.rs` L597：

```rust
if seed_total > 0 && stats.done > 0 {
    let per_item = (t.input_tokens + t.output_tokens) / stats.done.max(1);
```

`input_tokens` 包含所有 turn 的 prompt token，其中大量是重复的共享 prompt 前缀。这个数字会**显著高估**单 item 成本。Go 版报告没有这一项，我加的，但加错了口径。

---

## 3. P2：性能

### 3.1 `CancellationToken` 用 50ms 轮询 `[PERF]`

`runner.rs` L55：`tokio::time::sleep(Duration::from_millis(50))` 循环等待取消。

四个后台循环 + 每个 worker 一个，`maxWorkers=1000` 时就是 1000 个 50ms 定时器。用 `tokio::sync::Notify` 可以零定时器。

### 3.2 阻塞 `Mutex` 包在 async 里 `[PERF]`

`Store` 用 `std::sync::Mutex` 包 `rusqlite::Connection`，而所有调用都在 async 上下文里。一次慢查询（比如 `seed_progress` 的递归 CTE）会**阻塞 tokio worker 线程**。Go 版用 `database/sql` 连接池，不会这样。

**建议**：`spawn_blocking` 包住 store 调用，或换 `tokio::task::block_in_place`。

---

## 4. 为什么测试全绿却有这些 P0

因为 263 个测试的分布是：

| 测了什么 | 数量 |
|---|---|
| `CircuitBreaker` 纯函数（喂手工 World） | 63 |
| store 行为（真实 SQLite） | 46 |
| git 行为（真实 git） | 32 |
| server 行为（真实 HTTP） | 33 |
| harness 单元 | 66 |
| prompts golden | 12 |
| **runner 接线** | **2** |

**只有 2 个测试碰到 runner，且都只验证了 presplit 和 worktree 创建。** 没有一个测试断言"worker 真的 claim 了 item"或"budget 超了会停"。

这是我这轮最大的教训：**把熔断器抽成纯函数很美好，但如果没人测接线，纯函数的正确性毫无意义。**

---

## 5. 修复优先级

1. **§1.1 + §1.6**（observe 填真值 + worker 跟踪 claim）——不修的话真模型下必坏
2. **§1.3**（sysload）——不修的话生产没有资源保护
3. **§1.2 + §1.4 + §1.5**（gate 原因 / ledger / stalled）——不修的话报告排障失真
4. **§2.1**（AIMD 接上）
5. **§3.1 + §3.2**（性能）

---

## 相关

- [`../docs/OPTIMIZATION-SWARM.md`](../docs/OPTIMIZATION-SWARM.md)：Go 版的 16 项（我审 Go 的产出）
- [`README.md`](./README.md)：本 crate 说明
