# 借鉴 Super-agent 提升 swarm-rs：可落地方案

> 版本：v1.0（2026-11-01）
> 对象：`swarm-rs/`（10.5k 行 / 270 测试 / 0 警告，`cargo test --workspace` 实测通过）
> 上游材料：DeepSeek Harness 团队《Super-agent 设计探索》（2026-10-31/11-01，微信公众号）+《Super-agent 开放问题求解报告》（`docs/SUPER-AGENT-DESIGN-EXPLORATION.md`）+ 三项机理仿真（`docs/SUPER-AGENT-RESULTS.md`，本目录随附）
> 性质：**只做设计，不动代码**。每条都给出 swarm-rs 侧的现状行号、要改什么、怎么验证，以及哪些**不要**照搬。

---

## 0. 结论先说

swarm-rs 是 Go `agent-swarm` 的 Rust 重写，而 Go 版的形态恰好就是 Super-agent 架构的一个**早期、具体、无模型的实例**：Router≈Runner 的四个后台循环，Worker≈worker_loop，事件面≈`events` 表，记忆≈`run_log` + `items`。所以 Super-agent 的开放问题在 swarm-rs 里**不是假设，是已经发生的具体缺陷**。

按"收益 ÷ 改动量"排序，最值得做的四件事：

| 序 | 做什么 | 为什么 | 改动量 |
|---|---|---|---|
| **A** | **租约锁从"item 级"细化到"资源级"** | Super-agent Exp2 实测：租约锁清零返工且吞吐 +9%；LLM 协商烧 121 万 token 仍留 21 次竞态。swarm-rs 现在只对 `item_id` 上锁，两个 worker 改同一文件照样互踩 | 中 |
| **B** | **接上 AIMD 闸门** | 421 行实现 + 15 个测试全绿，`runner.rs` 里从未构造。所有 worker 同时打 LLM，第一次 429 零退避 | 小 |
| **C** | **补 runner 接线测试** | 熔断器 70 个测试全喂手工 `World`，`observe()` 至今有 4 个字段是字面量。这就是他们自己记录的"263 测试全绿但功能是空的" | 小 |
| **D** | **验收单/合同 + 打扰率度量** | Super-agent 公理六：把人的注意力从 O(任务数) 降到 O(异常数+验收数)。swarm-rs 现在只有三层报告，没有"这个 item 到底验收了没有" | 小-中 |

**明确不做**：Router 分层/联邦、A2A 五层协议栈、端侧 daemon 零信任、记忆压实 Worker、五档自治旋钮。理由见 §5——swarm-rs 是单机单信任域的离线编排器，这些是它现在不需要的复杂度。

---

## 1. 先对齐：Super-agent ↔ swarm-rs 概念映射

| Super-agent（原文/报告） | swarm-rs 对应物 | 现状 |
|---|---|---|
| Router（全局序点，只路由不执行） | `Runner` 的 4 个后台循环（autoscaler / integrator / renewer / reaper） | 已有 |
| Worker（上下文隔离并发） | `worker_loop`（`runner.rs` L681-875） | 已有 |
| 两级事件过滤 / 订阅直通 | 无。worker 每轮全量读 `digest()`（`runner.rs` L943-964） | **缺失** |
| 资源注册表 + 租约锁 | `claims` 表只按 `item_id` 加租约 | **粒度太粗** |
| 任务合同（SOW）/ 验收标准 | `items.detail` 自由文本，无结构化 DoD | **缺失** |
| 模式通告器（正在干什么/为什么/等谁） | `report()` 的三层报告（end-of-run） | **只有终点，没有过程** |
| 记忆分层 L0-L4 / LSM 压实 | `run_log` 全量 + `items` 状态 | 有事实层，无摘要层 |
| 动作系统调用门 | `bash_guard.rs` 的静态黑名单 | **是黑名单不是门控** |
| Watchdog（独立于执行） | 熔断器在 worker 自己进程内 | **不独立** |
| 全局熔断三档 | `wait_for_stop` 的 4 个天花板 | 已有雏形 |

---

## 2. 方案 A：资源级租约锁（最高优先）

### 2.1 问题（已在本仓库实证）

`SWARM-RUN-ISSUES.md` §2.3 已经摸到了边缘：资源正则 `resources.rs` 会把 `src/a.js` 提出来，**但它只被用在两个地方**——`artifact_key`（item 去重）和 `presplit_oversized_items` 的拆分轴。**没有任何地方用它做并发互斥**。

也就是说：`wg-config` 和 `wg-store` 两个 item 如果都写了 `crates/wg-core/src/lib.rs`，两个 worker 会**同时**改它，最后在 `integrator_loop` 的三路合并里爆炸——或者更糟，silently 丢失一方的修改（lost-update）。

Super-agent 报告 §3.4 把这件事定量了（Exp2）：

| 治理模式 | 完成任务 | 返工次数 | 浪费工时 | Router 仲裁消息 | 额外 token |
|---|---|---|---|---|---|
| naive（先干再说） | 553 | 97.6 | 167 min | 97.6 条 | 19.5 万 |
| **lease（租约锁）** | **603** | **0** | **0** | **0** | **0** |
| llm-negotiate | 580 | 21.4 | 51 min | 0 | **121 万** |

注意 lease 不只是"少出错"，**吞吐还高 9%**（省下的返工工时 > 排队成本 6.4%）。而"让 LLM 协商"是最差解：烧 121 万 token 仍留 21 次竞态。

### 2.2 现状代码

`store.rs` 的 claim 语义（README 六条约束第 2 条）：

```rust
// claims(token, item_id, peer, expires_at)
// claim() 在事务内回收过期租约并退回 pending
```

`runner.rs` L863-865（worker 退出时释放）：

```rust
for c in self.store.active_claims_by_peer(&self.peer()).unwrap_or_default() {
    let _ = self.store.release(&c.token);
}
```

`renewer_loop`（L532-557）每 `lease_sec/3` 续期。**这套机制本身是对的，只是锁的粒度是 item 而不是资源。**

### 2.3 改什么

**新增 `resource_leases` 表**（这是唯一需要动 schema 的地方，README 约束 1 要求 Go 兼容——所以必须是 additive migration，且 Go 打开时忽略未知表）：

```sql
CREATE TABLE IF NOT EXISTS resource_leases (
    resource   TEXT NOT NULL,          -- normalize() 后的资源键，如 src/a.js
    token      TEXT PRIMARY KEY,       -- 关联的 claim token
    peer       TEXT NOT NULL,
    expires_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS resource_leases_res_idx ON resource_leases(resource);
```

三处接线：

1. **`claim()` 里加冲突扫描**。claim 一个 item 前，用 `item_resources(&item.title, &item.detail)` 取出资源列表，检查这些资源是否已被**别的 peer** 持有未过期租约。若冲突 → 拒绝 claim（返回一个可区分的错误，让 worker 转去 claim 别的 item，而不是重试同一个）。
   - 已有基础设施：`store.rs` 的 claim 是单事务，`reap_expired` 也在事务内——这里直接复用同一事务，不要新开。
   - **注意 `reap_expired` 的自死锁坑**（README/`SWARM-RUN-ISSUES.md` 记录过）：Rust 的 `std::sync::Mutex` 不可重入，必须先提交事务再清 marks。资源租约的回收必须走同一条路径。

2. **`release()` 连带清资源租约**。token 是主键，一条 `DELETE FROM resource_leases WHERE token=?` 就够。

3. **`renewer_loop` 顺带续资源租约**（L550-553 那个循环里加一条 UPDATE）。

### 2.4 关键设计决策（容易做错的地方）

- **资源键必须复用 `resources.rs::normalize`，不要自己写一套**。README 约束 3 说"资源正则逐字一致"是「`error.message` 不算资源」的唯一依据。如果租约用另一套归一化，就会出现"扫描说不冲突、合并时冲突"。
- **`#symbol` 后缀要保留**。`resources.rs` 的测试 `symbol_granularity_keeps_same_file_apart` 明确锁定了这个行为：`src/video.js#pollVideo` 和 `src/video.js#buildVideoBody` **不该**互斥。照搬这个语义，否则同文件不同函数的两个 item 会被无谓串行化。
- **冲突时的行为是"换 item"，不是"等待"**。worker 应该去 claim 下一个无冲突的 item。这也是 lease 模式吞吐更高的原因（Exp2：排队只损失 6.4% 产能，而返工损失 167 分钟）。
- **Windows 反斜杠路径的老问题会在这里放大**。`resources.rs` L178-191 记录了：反斜杠只产出 basename，所以 `src\a\http.js` 和 `src\b\http.js` 会被误判为同一资源。修它要改正则、会改变 dedup 语义，那是独立决策——但**至少在租约冲突的拒绝原因里打出完整资源键**，让人能看出是不是误判。

### 2.5 验证

```rust
// 新测试，store_behaviour.rs：
#[test] fn resource_lease_blocks_a_second_peer();
#[test] fn resource_lease_expiry_releases_the_resource();
#[test] fn symbol_granularity_does_not_conflict();   // 复用 resources.rs 的既有断言
#[test] fn release_clears_resource_leases();
```

再加一个 e2e：两个 item 同写一个文件，断言第二个 worker 不会并发进入该文件。

---

## 3. 方案 B：接上 AIMD 闸门（最省事）

### 3.1 问题

`swarm-harness/src/aimd.rs` 有 421 行完整实现 + 15 个测试（`gate.rs` 慢启动、429 退避、`note_ok` / `note_rate_limit`），**但 `runner.rs` 里从未构造 `AdaptiveGate`**。`OPTIMIZATION-SWARM-RS.md` §2.1 记录了这个 P1："所有 worker 同时打 LLM，第一次 429 不会有任何退避。"

### 3.2 改什么

`WorkerHandle` 加一个 `gate: Arc<Mutex<AdaptiveGate>>`（runner 级共享，因为闸门治理的是"整个池子打 LLM 的速率"，不是单 worker）。`worker_loop` 的 turn 段包起来：

```rust
// runner.rs L747-769，现在是这样：
let result = if is_echo { ... } else {
    match swarm_harness::DirectHarness::new(...) {
        Ok(c) => swarm_harness::run_agent_loop(&c, ...).await.unwrap_or_default(),
        Err(_) => break,
    }
};

// 改为：整个 LLM 调用段过闸门
let mut g = self.gate.lock().await;
let permit = g.run(/* 这一轮的 future */).await;   // 注意 aimd.rs 的 API 形态
drop(g);
```

成功 `note_ok()`，429/503 `note_rate_limit()`。

### 3.3 注意

- `aimd.rs` 的确切 API 要先读一遍再接线（`gate.run` 的签名、`GateGuard`、`GateRunError` 的语义）。**不要照我的伪代码写**，以文件为准。
- echo 路径**不要过闸门**——离线测试不该被闸门拖慢，也不该消耗慢启动额度。
- 这是"接线"不是"重写"，`OPTIMIZATION-SWARM-RS.md` §2.1 的建议就是这么做的。

### 3.4 验证

`aimd.rs` 已有 15 个单测。补一个：runner 里两个 worker 共享一个闸门，断言第二次 429 后的退避窗口比第一次长（AIMD 的乘法递减）。

---

## 4. 方案 C：补 runner 接线测试（治本）

### 4.1 问题

这是 swarm-rs 自己记录在案的最大教训（`SWARM-RUN-ISSUES.md` §4）：**263 个测试全绿和"功能是空的"完全可以同时成立**——70 个熔断器测试全喂手工构造的 `World`，只有 2 个测试碰到 runner。

我读代码确认：**同一个 bug 模式还在**。`runner.rs` L918-939 的 `observe()`：

```rust
World {
    ...
    reply_says_idle: false,        // ← 字面量
    submitted_this_turn: false,    // ← 字面量，而 result.submitted 就在手边（L747）
    turn_ok: true,                 // ← 字面量
    http_status: None,            // ← 字面量
    ...
}
```

前六个字段（tokens / workers / cpu / mem / queue）他们修过了，这**后四个**是同一类错误的新实例。

### 4.2 改什么

1. **`observe()` 填真值**。`result.submitted` → `submitted_this_turn`；`result.steps == 0` → `turn_ok` 的合理代理；`reply_says_idle` 从 `result.reply` 判定（先看看 `swarm-harness` 有没有现成的 idle 检测，没有就加一个纯函数 + 测试）。
2. **写 §4 里点名却始终没写的那个测试**（`SWARM-RUN-ISSUES.md` §4 结尾自己说"可操作的改进"）：

```rust
#[tokio::test]
async fn observe_reports_a_nontrivial_world() {
    // 起一个真 runner（真 store + 真 git + echo harness）
    let world = runner.observe(&cb).await;
    assert!(world.active_workers > 0, "pool is visible");
    assert!(world.cpu_pct >= 0.0);
    assert_eq!(world.queued_submissions, store.queued_submissions().unwrap().len());
}
```

3. **给每条熔断器补一个"端到端触发"测试**：不是喂手工 World，而是构造一个真实会让它触发的场景。优先 guard 2（budget）、guard 4（no-claim-no-submit）、guard 9（item hold）——这三条恰好是被 `observe()` 假字段直接影响的。

### 4.3 验证

新测试必须**先红后绿**：把 `observe()` 的某个字段改回 0，确认新测试失败。

---

## 5. 方案 D：验收单 + 打扰率度量

### 5.1 借鉴什么

Super-agent 公理六：**系统的最高 SLO 不是任务吞吐，而是"把人的注意力占用从 O(任务数) 降到 O(异常数 + 验收数)"**。以及 §4.4 的"合同先行"：任务进来先签验收标准（Definition of Done），完成后逐条勾对，而不是"我觉得做完了"。

### 5.2 swarm-rs 的对应缺口

- `items` 表没有"验收标准"字段，`detail` 是自由文本。
- `report()` 输出 tokens / breakers / top-N 表，**没有"多少个 item 真的被验收了"**。
- guard 8（empty-output）靠 `artifact_fingerprint()`（`runner.rs` L971-1005）判断"有没有产出"，这是**产出存在性**，不是**产出正确性**。

### 5.3 改什么（小步）

1. **约定式 DoD，不改 schema**：seed item 的 `detail` 支持一个 `DoD:` 段落，runner 解析出来带进 worker prompt（`swarm-prompts` 那边加一个可选段）。这样不动 Go 兼容性。
2. **报告加两行**：
   ```
   accepted: N/M (guard 8 拦下 K 个空产出)
   ```
   accepted = `submitted && gate 通过` 的 item 数。gate 已经存在（`repo.gate()`），只是没被算成验收指标。
3. **`run_log` 加一列 `accepted`**（additive migration，同 §2.3 的理由）——这是 Super-agent 的 L4 事实层在 swarm-rs 的对应物，只增不改。

### 5.4 明确不做

五档自治旋钮、任务台账泳道、模式通告器、接管协议（SBAR）、A2A 协议栈、端侧零信任 daemon。swarm-rs 是**单机、单信任域、无人在回路的离线编排器**——这些设计解决的是"用户随时插进来"和"跨组织协作"的问题，照搬只会增加没有调用方的复杂度。Super-agent 报告自己的路线图也把这类事情排在"安全与协作地基"之后。

---

## 6. 还有两处小改进（顺手）

| 项 | 现状 | 建议 |
|---|---|---|
| **README 数字滞后** | 写着"263 passed / 10604 行"，实测 270 测试 | 改掉。文档说谎比没有文档更糟（他们自己在 `OPTIMIZATION-SWARM-RS.md` §1.2 骂过假注释） |
| **`CancellationToken` 50ms 轮询** | `runner.rs` L43-46，1000 worker = 1000 个定时器 | 换 `tokio::sync::Notify`。`OPTIMIZATION-SWARM-RS.md` §3.1 已记录，纯性能，无行为变更 |
| **阻塞 `Mutex` 包 async** | `Store` 用 `std::sync::Mutex` 包 rusqlite，慢查询堵 tokio worker | `spawn_blocking` 包住 store 调用。§3.2 已记录 |

---

## 7. 实施顺序与验证矩阵

| 步 | 内容 | 依赖 | 验证 |
|---|---|---|---|
| 1 | README 数字修正 + `observe()` 四个字段填真值 | 无 | 新测试先红后绿 |
| 2 | runner 接线测试（`observe_reports_a_nontrivial_world` + guard 2/4/9 端到端） | 1 | 同上 |
| 3 | `resource_leases` 表 + claim 冲突扫描 + release 清理 + renewer 续期 | 无 | 4 个 store 测试 + 1 个 e2e |
| 4 | AIMD 闸门接线（先读 `aimd.rs` 实际 API） | 无 | 429 退避递增测试 |
| 5 | DoD 段落 + 报告 accepted 行 + `run_log.accepted` | 3 | 报告输出断言 |
| 6 | `Notify` 替轮询 + `spawn_blocking` | 无 | 全量回归 + 计时对比 |

每步完成后跑 `cargo test --workspace` 与 `cargo build --workspace`（README 声明 0 warnings，别破功）。

---

## 8. 附：本次调研的来源与一处勘误

- **swarm-rs 位置**：`C:\Users\zs\Desktop\多agent\swarm\swarm-rs`（不在会话工作目录 `WarpGate2` 下）。
- **"Super-agent" 指什么**：DSH（DeepSeek Harness）团队 2026-10-31/11-01 发布在微信公众号的《Super-agent 设计探索》，GitHub 上的《开放问题求解报告》即其配套技术评论（`FranciscoLaurent/super-agent-open-questions`，MIT，含三项可复现机理仿真）。**DeepSeek Harness 安装包内（183MB `app.asar` + `app.asar.unpacked`）不含任何 Super-agent 实现**——我全文检索过，`superAgent`/`SuperAgent`/`超级智能体` 零命中，仅命中无关 npm 包 `superagent`。安装包里实际存在的是 Agent Teams / workflow / ralph / auto-review 等插件。
- **勘误**：runtime context 给的 DSH 实现检出路径 `...\app.asar\dsh\` 不存在；实际在 `...\resources\app.asar` 与 `...\resources\app.asar.unpacked\dsh\`。
- **网络**：`web_search` 因 DeepSeek API 余额不足（HTTP 402）不可用；以上材料经 GitHub API / codeload 获取。
