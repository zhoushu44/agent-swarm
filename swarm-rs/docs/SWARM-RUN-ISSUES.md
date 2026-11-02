# swarm 中间出现的问题

> 版本：v1.0（2026-11-01）
> 场景：用 `agent-swarm`（Rust 版 `swarm-rs`）编排 WarpGate 的 Rust 化
> 数据来源：**真实运行**，非推测。seed 10 个 item，echo harness，3 worker，
> wall-clock 20s。运行目录 `%TEMP%\warpgate-rust-swarm`。
>
> 本文档分两部分：
> 1. **swarm 自身的问题**（我写的 Rust 代码的缺陷，附修复前后对比）
> 2. **用 swarm 干这件事本身的问题**（工具链/方法层面的）

---

## 0. 运行数据

### 修复前（第一轮）

```
[swarm] stopping: wall-clock
===== swarm run report (wall-clock) =====
items: 10 total | 0 done | 0 failed | 10 still open
tokens: 0 in / 0 out / 40 api calls
wall: 26.4s
breakers: empty-output=10 (total 10)
```

git 侧实际产物：

```
worktrees: w1 w3 w4 w5 w6 w8 w10 w12 w14 w16     ← 10 个
branches:  integration + swarm/swarm-w1..w16      ← 10 个
commits:   6fb526a swarm: init                    ← 只有 1 个（初始的）
```

**10 个 worker、10 个 worktree、10 个分支、0 个 commit、0 个 item 完成。**

### 修复后（同一 seed，同参数）

```
breakers: autoscaler-give-up=7 empty-output=7 (total 14)
[swarm] autoscaler: giving up after 6 stalled spawns (harness is not driving tools)
[swarm] autoscaler: giving up after 7 stalled spawns (harness is not driving tools)
```

guard 11 从不触发 → 正确触发 7 次并停止扩容。

---

## 1. swarm 自身的问题（6 个 P0，全部真实存在）

### 1.1 `observe()` 把 6 个关键字段硬编码为 0 `[P0]`

`runner.rs` L809-821（修复前）：

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

**四条熔断器因此完全不生效**：

| 字段 | 失效的 guard | 后果 |
|---|---|---|
| `tokens_spent` | guard 2 budget | `--budget-tokens` 无效，超预算不停 |
| `held_item_tokens` | guard 9 item-token | 单 item 烧 2M token 不释放 |
| `cpu_pct` / `mem_pct` | guard 12 资源 | 机器打满不缩容，可能 OOM |
| `active_workers` | guard 1 / 6 | "是不是最后一个 worker" 永远判断错 |

**根因**：写 `World` 时先填了容易拿的字段（claimable / open / my_claims），难的留了个 0。

**为什么 270 个测试没抓到**：`CircuitBreaker` 的 70 个测试全部喂**手工构造的 World**，没有一个测试问"`observe()` 产出的 World 是对的吗"。

**修复**：从 `totals` / `ledgers` / `active_workers` / `sysload::sample()` 取真值。

### 1.2 gate 失败原因被丢成一句假话 `[P0]`

`runner.rs` L458 + L614（修复前）：

```rust
let reason: String = e_reason().chars().take(400).collect();
...
/// Placeholder for a gate failure's reason; the real reason is captured by the
/// caller.
fn e_reason() -> String {
    "gate failed".to_string()
}
```

`repo.gate()` 返回的 `ErrGate { command, output, err }` 带着**完整测试输出**——排障唯一需要的东西——为了 `'static` 把它丢了。于是每条 rejection 在 db 里都是同一句 `"gate failed"`。

**最恶劣的是注释**：我写了"the real reason is captured by the caller"，而 caller 并没有 capture。**自己写的假注释比没有注释更糟**，因为它让下一个读代码的人（包括我自己）以为这里没问题。

**修复**：`match repo.gate().await { Err(e) => { ... e.to_string() ... } }`，把 String move 进闭包。

### 1.3 `sysload` 未实现，资源守卫永不触发 `[P0]`

`autoscaler.rs` 一直吃 `LoadSample::default()`，cpu/mem 恒 0.0 → `above_critical()` 永假 → 永不缩容。

Go 版 `internal/sysload` 是**生产验证过的 OOM 保护**（windows/linux 双实现，85/90 增长拒绝阈值，92 强制缩容阈值）。Rust 版完全没搬。

**这是最危险的缺口**——不是"少个功能"，是"少了生产保护"。几百个 worker 同时跑 `go test` 时，Rust 版不会缩容。

**修复**：新增 `sysload.rs`，`/proc/meminfo` + `/proc/stat`（Linux）、`GlobalMemoryStatusEx` + `GetSystemTimes`（Windows），两个阈值分开。附带 7 个测试（含"88% CPU 拒绝增长但不缩容"这种边界）。

**过程中踩的坑**：`windows-sys` 0.59 的符号位置和想的不一样——
- `GlobalMemoryStatusEx` / `MEMORYSTATUSEX` 在 `Win32::System::SystemInformation`，不是 `Memory`
- `GetSystemTimes` 在 `Win32::System::Threading`，不是 `SystemInformation`

三个 feature 名都要显式开（`Win32_System_SystemInformation` / `Win32_System_Threading` / `Win32_Foundation`）。

### 1.4 `ItemLedgers` 从不写入 `[P0]`

`worker.rs` 里认真写了 `add` / `remove` / `top_by_tokens` 和 4 个测试，`runner.rs` 里**只在 report() 读，没有任何 `ledgers.add(...)`**。

**后果**：
- 报告的 "top token spend" 永远是空
- guard 9 的 token 判据失效（和 §1.1 双重失效）
- `tokens per done item` 用 `totals` 算，那个数字对——但 top-N 表是假的

**测试测了数据结构，没测接线。**

**修复**：worker 每轮按 `cb.held_item()` 调 `ledgers.add(id, tokens, steps)`。

### 1.5 `stalled_spawns` 从不递增，guard 11 永不触发 `[P0]`

`Autoscaler` 有 `note_stalled_spawn()`，`CircuitBreaker` 也有，但 **runner 里零调用点**。

**后果**：Go 版 L1665-1670 的 give-up guard（连续 3 次 spawn 无产出就停止扩容）在 Rust 版不存在。模型不驱动工具时会**无限 spawn 到 maxWorkers=1000**。

**第一轮运行数据印证了这一点**：10 个 worker 全部 spawned 并建了 worktree，没有一个被拦住。

**修复**：加共享 `Arc<AtomicUsize> stalled_spawns`，worker 退出时若无 claim 则递增；autoscaler 每 tick `swap(0)` 吸收。修复后运行数据：`autoscaler-give-up=7`，日志正确输出"giving up after 7 stalled spawns"。

**实现细节**：`Autoscaler` 在循环里是局部变量，worker 拿不到，所以必须走共享计数器——这也是为什么 Go 版把 `stalledSpawns` 放在 `swarmRunner` 上而不是 `workerState` 上。

### 1.6 `workerLoop` 从不 claim / submit `[P0]`

整个 `worker_loop` 只调 `claimable_count()` / `open_count()` / `active_claims_by_peer()` / `digest()`——**从不 `claim()`、从不 `submit()`**。

runner 以为 claim 是通过 HTTP API 发生的（模型调 `http` 工具），所以它"不需要"跟踪。但**没有跟踪就没有 `held_item`**，于是：

- guard 4（no-claim-no-submit）在 6 轮后把 worker retire
- guard 9（item hold）永远不触发
- ledger 无处归属

**修复**：turn 结束后调 `store.active_claims_by_peer(&peer)` 更新 `cb.note_held_item(...)`。这正是 Go 版 L838-857 在做的事。

---

## 2. 用 swarm 干这件事本身的问题

### 2.1 echo harness 什么都验证不了 `[P0]`

echo harness 不调 LLM、不产出文件、不 claim item。所以：

- 用 echo 跑只能验证"runner 不挂、能停、有报告"
- **不能验证 worker 真能完成一个 item**
- guard 8（empty-output）必然触发，因为 echo 从不写文件

**结论**：echo harness 的唯一价值是**离线回归测试编排逻辑**，不能用来验证功能正确性。要看 swarm 真能干活，必须有真 LLM endpoint。

### 2.2 seed.json 的 item 之间没有依赖声明，但实际有 `[P1]`

我写的 10 个 item 里，`wg-config` / `wg-store` / `wg-core` 是其他 crate 的地基。但 seed.json 没有依赖机制，swarm 会把它们当独立 item 并行分给不同 worker。

**后果**：`wg-socks` 的 worker 可能在 `wg-core` 还没建好时就开始写代码，然后编译失败。

**swarm 的 split 是按文件粒度**（`presplitOversizedItems` 按资源拆），不会理解 crate 间的依赖。

**缓解**：把地基 crate 写进 detail 里（"先确认 wg-core 存在"），或者按阶段分批 seed。

### 2.3 item 的 title 用路径，swarm 的 dedup 会折叠 `[P1]`

我的 title 是 `warpgate-rs/crates/wg-config` 这种路径形式。swarm 的资源键提取会把 `wg-config` 当路径吗？不会（没有扩展名），但 `warpgate-rs/Cargo.toml` 会（`.toml` 是已知扩展名）。

**风险**：两个不同 item 如果 title 里出现同一个文件路径，会被 `artifactKey` 折叠成一个。

**实际没触发**，因为我的 10 个 title 里只有 3 个带扩展名且互不重复。

### 2.4 没有 gate 命令，等于没有质量门禁 `[P1]`

这次跑没设 `--test-cmd`。Go 版默认也没有，但生产上应该设 `cargo test --workspace`。

**后果**：worker 提交的代码不管是否编译通过都会被 merge。这次 0 个 commit 所以没暴露，但一旦 worker 真开始提交，坏代码会直接进 integration 分支。

### 2.5 报告缺"为什么停了"的关键上下文 `[P2]`

`stopping: wall-clock` 只说了触发了哪个天花板，没说：
- 当时多少 worker 活着
- 多少 item 还开着
- 是不是所有 worker 都被 park 了

这次从 `breakers: empty-output=10` 能反推出来，但报告本身不直接说。

---

## 3. 修复效果对比

| 项 | 修复前 | 修复后 |
|---|---|---|
| 测试数 | 263 | **270** |
| guard 11 触发 | 0 次 | **7 次** ✓ |
| stalled 计数 | 恒 0 | 真实累加 ✓ |
| autoscaler 放弃 | 不会 | **会** ✓ |
| gate 失败原因 | `"gate failed"` | 真实 `ErrGate` 输出 ✓ |
| ledger 写入 | 无 | 每轮按 item 记 ✓ |
| worker claim 跟踪 | 无 | turn 后同步 ✓ |
| 资源采样 | stub | windows+linux 真采 ✓ |
| 二进制 | 8.45 MB | — |

**仍未修**（已知，未阻塞）：

- AIMD 闸门建了但没接上（§2.1 of OPTIMIZATION-SWARM-RS.md）
- `CancellationToken` 50ms 轮询
- 阻塞 `Mutex` 包 async
- 报告的 `tokens per done item` 口径含重复 prompt 前缀，会高估

---

## 4. 最大的教训

**263 个测试全绿，和"功能是空的"完全可以同时成立。**

分布是这样的：

| 测了什么 | 数量 |
|---|---|
| `CircuitBreaker` 纯函数（喂**手工构造**的 World） | 70 |
| store / git / server / harness 行为 | 177 |
| prompts golden | 12 |
| **runner 接线** | **2** |

我把 14 条熔断器抽成纯函数、每条配测试，获得了强烈的安全感——但那些测试喂的是我**想象中**的世界。真实世界由 `observe()` 产生，而 `observe()` 有 6 个字段是 0。

**纯函数的正确性，在没人测接线时毫无意义。**

可操作的改进：给 `observe()` 写一个测试，断言它产出的 `World` 里 `active_workers > 0`、`cpu_pct >= 0.0`、`queued_submissions` 与 `store.queued_submissions().len()` 相等。这样一个断言就能抓住 §1.1。

---

## 相关

- [`OPTIMIZATION-SWARM-RS.md`](./OPTIMIZATION-SWARM-RS.md)：swarm-rs 的完整自审（本文档的上游）
- [`../docs/OPTIMIZATION-SWARM.md`](../docs/OPTIMIZATION-SWARM.md)：Go 版 swarm 的 16 项缺陷
- [`README.md`](./README.md)：swarm-rs 说明
