# 变更记录

遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 与语义化版本。

---

## [0.2.0] — 2026-11-02

借鉴 DeepSeek Harness 团队《Super-agent 设计探索》及四个开源多智能体项目（OpenRath / OpenPARL / agent-fleet-manager / cc-fleet）后的系统性升级。

**测试 270 → 333，构建警告 0。**

### 新增

- **资源级租约锁**（`resource_leases` 表）—— 原来只锁 item，两个 item 写同一个文件要等到 merge 才炸。现在 claim 在同一事务内按资源键上锁，冲突时拒绝并**点名资源**。
- **runner 接线测试**（`tests/runner_wiring.rs`）—— 熔断器 70 个测试全喂手工 `World`，没有一个问过 `observe()` 产出的世界对不对。
- **半成品交接** —— guard 9 收回任务时，FAIL 事件带上 `changed N file(s): ... (head <sha>)`，下一个人能看到上次干到哪，而不是一句"换个方式"。
- **`/marks` 同时报告两种协调机制** —— intent mark（提示）与 resource lease（权威）原本是两张表、`/marks` 只说前者，worker 看到 `count: 0` 就去 claim 然后撞 409。
- **item 去重折叠** —— Go 有两级去重（`items.go` L358-395），Rust 侧 `artifact_key` 被重新导出后**无人调用**，整个机制缺失。
- **`DoD:` 段落 + accepted 指标** —— `detail` 里的 `DoD:` 进 worker prompt；报告输出 `accepted: N/M` 与一次验收通过率。
- **AIMD 闸门可观测** —— `GateState` 每轮采样，报告打印 `llm gate: limit=.. ceiling=.. slow_start=.. rate_limits=.. backoffs=..`。
- **`--api-key-file` / `--api-key-stdin`** —— key 不再进进程表与 shell history。`--api-key` 保留兼容。
- **`xtask`** —— `cargo run -p xtask` 列出没有生产调用者的公开方法。
- **`RepoError::code()` / `remedy()`** —— `MERGE_CONFLICT` / `GATE_FAILED` / `GIT_ERROR` / `REPO_ERROR` 稳定词表 + 可操作的补救说明。

### 修复

按发现顺序，**每一项都有回归测试**。其中 4 项属于同一形态：**有测试、全绿，但生产行为是空的**。

| # | 缺陷 | 症状 |
|---|---|---|
| 1 | `observe()` 六个字段硬编码 0 | budget / item-token / resource / exit-when-done 四条熔断器全部失效 |
| 2 | gate 失败原因被丢成 `"gate failed"` | 每条 rejection 在 db 里都是同一句假话，排障无据 |
| 3 | `sysload` 未实现 | 资源守卫永不触发，可能 OOM |
| 4 | `ItemLedgers` 从不写入 | token 账本永远空，报告失真 |
| 5 | `stalled_spawns` 从不递增 | guard 11 永不触发，会无限 spawn 到 1000 |
| 6 | `workerLoop` 从不 claim/submit | echo 之外什么都不会发生 |
| 7 | `evaluate_post_turn` 传硬编码 `true` | 一次传输错误被当成"端点不工作"，3 次就 park 掉 worker、丢掉已做的工作 |
| 8 | `advance_hold` 无生产调用者 | `held_for` 恒 0，`--max-item-seconds` 是静默空操作 |
| 9 | `reap_backoff` 无调用者 | worktree 删不掉时每 10s 重试，永不退避 |
| 10 | `artifact_key` 无调用者 | item 去重整个缺失，重复 item 让多个 worker 用不同 id 抢同一文件 |
| 11 | 基线提示在 merge **之后**读 HEAD | `before..integration` 恒为 0，提示永不触发；300-item 对比压测 accepted 率反降 60%→38% |
| 12 | HARD RULES 重建靠字符串精确匹配 | `body_gen.rs` 快照的顺序与 `render_hard_rules` 排序不同，匹配永远失败——**`--no-create` 从未进过任何一个 prompt** |
| 13 | 闸门 ceiling 被慢启动翻倍到超过 worker 总数 | 96 worker 的池子 limit 走到 128，闸门永远不可能成为瓶颈 |
| 14 | storm 窗口在计数器之前 return | 两个 429 只记一个，报告显示 `rate_limits=1` 看起来合理 |

另有三项 UTF-8 / 并发 / 性能修复：

- `String::truncate` 按字节切，多字节序列中间 panic —— HTTP 回包 / bash 输出 / gate 失败信息 / 冲突文件视图共 4 处，M4 基准直接崩出来过。
- guard 9 释放 item 但**不释放租约** —— item 显示可抢，别人一抢撞 409，最多卡 300 秒。
- 阻塞 `Mutex` 包 async（`observe()` 5 连查 + `digest` 改走 `spawn_blocking`）；`CancellationToken` 从 50ms 轮询改为 `Notify`。

### 变更

- **错误分派改为稳定码**：runner 原来用 `reason.contains("conflict")` 决定是否拆分，改一下错误文案行为就静默改变。
- **submission 判重改用 tree sha**：commit sha 每次 merge 都变，tree 才是内容身份。原来 noop 静默走 accepted，重复提交会虚涨 accepted 率。
- **worker prompt 新增白名单规则** `submit-is-the-only-door`：黑名单（"别 commit scratch"）永远列不全，白名单（"唯一出口是 POST /submit"）可以。
- **`run_log` 加 `field` / `old_value` / `new_value`**：原来只记"被拒了"，不记"从 submitted 变成 pending"。

### 已知限制

- `src/video.js` 与 `src/video.js#pollVideo` 是不同的资源键 —— 这是"同文件不同函数可并行"的基础，代价是整文件 item 不会阻塞其中的符号 item。修它要教租约理解前缀包含关系，会改变所有调用方的去重语义，已固定成测试。
- Go 版三角对比（wall / peak RSS / token）**未做**。
- `sanitize_ref` 曾被 xtask 误报为"无调用者"，实际 `worker_branch()` 一直在用 —— 已核实，未改动。

---

## [0.1.0] — 2026-10-31

Go `agent-swarm` v5.8.0 的初始 Rust 重写：8 个 crate、270 测试、0 警告。
