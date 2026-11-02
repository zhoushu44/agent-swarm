# 学习 OpenRath / OpenPARL：对 swarm-rs 的可借鉴点

> 版本：v1.0（2026-11-02）
> 对象：`Rath-Team/OpenRath` v2.0（清华/中大，BSD-3，959★）+ `GuanxingLu/OpenPARL`（Kimi K2.5 PARL 复现，Apache-2.0）
> 前提：两者都是**研究/框架**项目，swarm-rs 是**单机离线编排器**。下面每条都注明"该不该搬"。

---

## 1. OpenRath 的核心思想：Session 是一等公民

OpenRath 的立论是"大多数 agent 框架从 agent loop 开始，我们从 Session 开始"。它的 Session 带 `parent_session_ids`（**多父**，不是单父）、`lineage_kind`、`lineage_operator`，构成一个 DAG，并提供：

| 能力 | 文件 |
|---|---|
| `ancestors_bfs` / `descendants_dfs_preorder` | `session/graph/traverse.py` |
| `validate_acyclic`（父缺失或成环即报错） | 同上 |
| `LineageKind`：`OP_FORK` / `OP_DETACH` / **`OP_MERGE`** / `OP_SESSION_COMPRESS` | `session/graph/kind.py` |

**`OP_MERGE` 是关键**：OpenRath 的 session 图明确把"两个 session 合并成一个"作为一等操作。而 swarm-rs 的 `items` 表只有 `parent_id`（单父），拆分是严格的树，**没有 merge 语义**——worker 之间只能通过 `integration` 分支间接汇合，没有"这两个子任务的结果合成一个"的原语。

### 该不该搬

**部分搬。** swarm-rs 的多父 DAG 是过度设计（它的并行单位是文件，天然无环）。但 **`OP_MERGE` 对应的需求是真实的**：现在 10 个 Implement + 10 个 Document item 落在同一批文件上，做完之后**没有任何机制说"这些子项合成的一个整体"**。`seed_progress` 只能告诉你 31/46 terminal，不能告诉你"`wg-config` 这个 seed 的所有子项都完成了"。

而这恰好是 `needsSplit` 的反问题——README 约束 6 已经为"拆"定义了规则，却没有为"合"定义。

---

## 2. OpenRath 的 Effect Ledger：swarm-rs 缺的那一半

这是**最有价值的一条**。`runtime/effects.py` 的设计：

```
prepare(run_id, tool_name, effect_class, arguments_digest, idempotency_key)
  → 记录一次"即将发生的副作用"，带参数摘要和幂等键
```

三个 swarm-rs 直接对应的点：

### 2.1 幂等键 = 防重复合并

OpenRath 用 `ON CONFLICT (run_id, idempotency_key) DO NOTHING`，并在复用键但参数不同时**显式报错**：

```python
"effect idempotency key was reused with different input"
```

swarm-rs 的对应物是 `integrator_loop` 的 submission 处理。它已经有 `merge-base --is-ancestor` 的 stale 检查，但**没有幂等键**。如果同一个 submission 被处理两次（integrator tick 与进程重启的竞态），第二次会怎样？`merge_only` 会走 `noop` 分支然后 `resolve_submission(id, true, ...)`——**静默标记为成功**。这正是 OpenRath 用 `NEEDS_REVIEW` 而不是"盲目重放"要防的情况。

### 2.2 NEEDS_REVIEW：歧义副作用不该自动重放

```python
# Idempotent calls may be retried under their stable key.
# Non-idempotent calls are never replayed automatically
# and move their Run to NEEDS_REVIEW.
```

`reconcile_stale_effects(grace_seconds=30)` 在 worker 崩溃后区分：幂等的可重试，非幂等的进人工复核。

swarm-rs 的 `reap_expired` 回收过期租约时，直接把 item 退回 `pending`——**不区分这个 item 的 worker 是死了还是只是慢了**。如果 worker 还活着只是在长考，它的 item 已经被别人抢走，两个人同时改同一个文件。guard 9 现在释放租约修掉了"卡住不释放"，但**没修"误释放"**。

### 2.3 参数摘要 = 可诊断性

`arguments_digest()` 对工具参数做规范化 JSON + SHA256。swarm-rs 的 `run_log` 有 `detail` 字段存拒绝原因，但**工具调用本身没有摘要**——出问题时无法回答"这个 worker 到底发起了什么操作"。

### 该不该搬

**该搬，且优先级高。** 但只需搬最薄的一层：给 submission 加一个 `(item_id, commit_sha)` 的唯一约束 + 重复时的显式处理，而不是整套 ledger。OpenRath 的 ledger 是为多 worker、多进程、PostgreSQL 设计的；swarm-rs 是单进程单 SQLite，过度搬运会引入比它解决的更多问题。

---

## 3. OpenPARL 的教训：结构 > 提示词（swarm-rs 已经做对）

BLOG.md 的发现对 swarm-rs 是**验证性的**，不是新信息：

| OpenPARL 实验 | 结论 | swarm-rs 对应 |
|---|---|---|
| Single / Delegate-only / PARL 三模式对比 | **Delegate-only（强制委派）RL 崩溃**：`assign_task` 率 0.80→0.10，截断 23%、重复 24% | swarm-rs 的 Router **根本不是模型**，不存在"模型不愿分工"的问题 |
| PARL 保留直接工具 | 率 0.03→1.00，截断/重复 <5% | — |
| 冻结 subagent 的 `solver_success_rate` 恒定 0.40-0.46 | **subagent 不学习**，学会的只是编排 | swarm-rs 的 worker 每轮都是全新 context，同样不学习——但它的 `history`/`run_log` 是跨 worker 的，这是它的补偿机制 |
| `n_assign` → ~12 而 distinct names → ~5 | **复用专家而非每次新建** | swarm-rs 的 `create_item` 去重折叠（我刚实现的那条）正是同一思想的确定性版本 |

**最重要的一条印证**：OpenPARL 用 RL 才发现"必须给模型留直接工具的退路"，而 Super-agent 那篇文章和 swarm-rs 的架构**从设计上就规避了这个问题**——把路由交给确定性代码，模型只需要在"这一个任务"里干活。

### 该不该搬

**不用搬**，但它证明了一件事：swarm-rs"Router 不是模型"的选择比 OpenPARL 用 RL 逼出来的行为更彻底。这是值得写进 README 的论据。

---

## 4. 其他值得记的

| OpenRath 特性 | 评价 |
|---|---|
| `Selector`（LLM 路由 + 普通 Python `if`/`while`） | 不搬。swarm-rs 的控制流全是确定性的 |
| `Memory` 作为 `Parameter` 类比 | **部分相关**：swarm-rs 的 `events`/`run_log` 就是它的 Memory，但无 schema 无生命周期（Super-agent 报告 §3.5 已指出） |
| Sandbox 决定工具在哪跑 | 不搬。swarm-rs 的 worktree 就是隔离 |
| PostgreSQL + Redis + S3 生产三件套 | 不搬。单机 SQLite 是特性不是缺陷 |
| `openrath-migrate --check` | **值得抄一个**：swarm-rs 有 additive migration 但没有"检查迁移是否该跑"的命令 |

---

## 5. 结论：三条该做的，按优先级

| # | 做什么 | 为什么 | 成本 |
|---|---|---|---|
| 1 | **submission 幂等键**：`(item_id, commit_sha)` 唯一约束，重复时不静默标记成功 | OpenRath Effect Ledger 的核心教训；当前重复处理会静默 `noop → accepted`，污染 accepted 率 | 小 |
| 2 | **租约误释放保护**：reap 前确认 peer 真的死了，而不是只看租约过期 | 长考中的 worker 会被抢走 item，两人同改一文件 | 中 |
| 3 | **`needsMerge`：seed 的所有子项 terminal 才算 seed terminal** | `OP_MERGE` 的最小可用版本；现在"拆了"有规则、"合了"没有 | 小 |

第 1、3 条各是一处小改动；第 2 条要设计"worker 心跳"或等价物，是三者中唯一有真实设计成本的。

**明确不搬**：session DAG、Effect Ledger 全量、Selector、生产三件套。swarm-rs 是单机离线编排器，这些解决的是它没有的问题。