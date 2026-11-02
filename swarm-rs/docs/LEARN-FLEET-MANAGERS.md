# 学习 agent-fleet-manager / cc-fleet：对 swarm-rs 的可借鉴点

> 版本：v1.0（2026-11-02）
> 对象：
> - `dreamers-laboratory/agent-fleet-manager`（139★，Apache-2.0，Python stdlib only，单 SQLite）
> - `ethanhq/cc-fleet`（185★，Apache-2.0，Go，Claude Code 的第三方模型舰队）
> 两者都读了源码（engine.py 229 行 / CLAUDE.md 196 行），不是只看 README。

---

## 1. agent-fleet-manager：三条规则，两条 swarm-rs 正好缺

这个项目的全部设计挂在三条规则上（`engine.py` 文件头）：

1. **Workers never write canonical state.** 每个 batch 拿到一个 `write_scope` 目录，worker 只往里扔结果文件，父进程在 `reconcile` 时折叠进正式状态。
2. **Every canonical write lands in `change_log` with an actor and a reason.**
3. **Change detection is a content hash, so re-running anything is safe.**

### 1.1 规则 1：崩溃半径 = 一个 scratch 目录

README 原话：*"a crashed or misbehaving worker can corrupt at most its own scratch directory."*

对照 swarm-rs：worker 直接 `git commit` 到自己的分支，然后 integrator 合并。**worker 能写的不只是 scratch**——它能 commit 到 `swarm/swarm-wN` 分支，而这条分支会被 integrator merge。bash guard 拦 scratch 产物，但那是黑名单不是边界。

更关键的是 `reconcile` 的那句：**"The engine treats a missing result file as a failure, so dead workers resolve themselves."**

swarm-rs 的对应问题：worker 进程被 kill 时，它的 claim 要等租约过期（默认 300s）才被 `reap_expired` 回收。这 300 秒里 item 显示 `in_progress` 但没人干活。而 agent-fleet-manager 的答案是"没有结果文件就是失败"——**不需要等任何超时**。

### 1.2 规则 2：变更台账（swarm-rs 的 run_log 缺两个字段）

```sql
CREATE TABLE change_log (
    change_id  INTEGER PRIMARY KEY,
    at         TEXT NOT NULL DEFAULT (...),
    actor      TEXT NOT NULL,     -- 谁
    entity     TEXT NOT NULL,     -- 改什么
    entity_id  TEXT NOT NULL,
    field      TEXT,              -- 改哪个字段
    old_value  TEXT,              -- 旧值
    new_value  TEXT,              -- 新值
    reason     TEXT NOT NULL      -- 为什么
);
```

swarm-rs 的 `run_log` 有 `actor` / `kind` / `detail`，但**没有 `old_value` / `new_value`**。现在的拒绝记录是"因为 X 被拒"，但看不到"从什么状态变成了什么状态"。

而 `field` + `old`/`new` 正是回答"这个 item 的状态怎么流转过来的"的最小集合。我在 guard 9 交接那轮加的是"改了哪些文件"，那是**工作产物**的交接；`change_log` 记的是**状态流转**的交接。两件事，swarm-rs 只有前者。

### 1.3 规则 3：内容哈希 = 重复执行安全

```python
changed = int(bool(result["sha256"]) and result["sha256"] != source["last_sha256"])
```

`observation.sha256` 与 source 的上次哈希比较，得出 `changed` 标志。**这直接解决我上一轮从 OpenRath 学到的"submission 幂等键"需求**，而且更简单：

swarm-rs 的 `merge_only` 遇到 noop 时静默走 `resolve_submission(true)` —— 如果按内容哈希判断，"这个 commit 的内容和已合并的相同"就该是 `changed=0` 但**仍然 accepted**，而"同一个 submission 被处理两次"应该是第二次直接识别为重复。哈希让这两件事可区分，现在的代码不能。

### 1.4 一个可以直接抄的小东西：SKILL.md

它用 57 行教一个 coding agent 怎么操作整个循环，其中最关键的一段：

> 4. **Write nothing anywhere else. The write scope is the entire surface a worker may touch.**
> 5. ... **Never edit the source, action, or observation tables directly — reconcile is the only door into canonical state.**

"只有一道门进正式状态"——这句话应该出现在 swarm-rs 的 worker prompt 里。现在的 prompt 说"NEVER commit scratch files"（黑名单），没说"你唯一的出口是 POST /submit"（白名单）。Super-agent 公理四（机制与策略分离）和这条是同一个意思。

---

## 2. cc-fleet：一个把"不变式"当代码审查边界的项目

`CLAUDE.md` L100-142 是一份 **11 条不变式清单**，每条都标注"do not regress"。swarm-rs 的 README 也有"六条不能违反的约束"，所以这个模式本身不新鲜。但有四条 cc-fleet 写得比 swarm-rs 更严：

### 2.1 「先过门，再产生副作用」

> **Gates before side effects.** ... fail-closed ... **Fail-before-mutation.**

swarm-rs 的对应物：`claim()` 在同一事务内检查资源租约——这个做对了。但 `spawn_worker` 是先 `add_worktree` 成功、再 `active_workers.fetch_add(1)`。如果中间失败，计数和实际不一致。小事，但方向是对的。

### 2.2 「分类的错误信封，永不返回裸错误」

> **Classified `Result` envelopes.** `subagent.Run` / `teammate check` never return raw Go errors — always a `Result` with `ok` and a stable UPPER_SNAKE `error_code` + suggestion ... **The skills dispatch on `error_code`; preserve the vocabulary.**

**这条最有价值。** cc-fleet 的错误是**机器可判定的稳定词表**，调用方（skill/模型）可以按 `error_code` 分派，而不是靠字符串匹配。

对照 swarm-rs：我这轮修 merge conflict 时写的是

```rust
if reason.contains("conflict") || reason.contains("Conflict") { ... }
```

**字符串匹配错误原因**。这正是 cc-fleet 明确反对的模式——`RepoError` 已经有 `is_conflict()`（`repo.rs` L70），我却在解析它的 `to_string()`。应该让错误带上稳定码，让调用方 switch，而不是 contains。

### 2.3 「名字先验证再使用」

> **Name validation before use** (`internal/ids`). Provider/team/agent names reach file paths ... re-validated in `config.Load` and `profile`.

swarm-rs 的对应物：`sanitize_ref` / `strip_verbatim`（`repo.rs` L411/L424）——而 xtask 报告这两个**没有生产调用者**。worker 名字来自 `format!("w{}", n)`，安全；但 `add_worktree(worker)` 的 worker 名如果来自外部（比如 item title 拼出来的），就会进文件路径。目前没有这条路径，但**校验函数存在却没用**，和 `advance_hold` 是同一类问题。

### 2.4 「单一出口」

> **Single outlets.** Every tmux call goes through `tmux.Server`; every atomic write through `fileutil.AtomicWrite`; every Claude Code path through `claudepaths`.

swarm-rs 基本做到了（所有 git 走 `Repo`，所有 store 走 `Store`）。这条是确认，不是缺口。

---

## 3. 明确不搬的

| 特性 | 为什么不搬 |
|---|---|
| cc-fleet 的 tmux pane / 四条执行lane | swarm-rs 的 worker 是进程不是终端会话，没有"人看着 pane"的需求 |
| cc-fleet 的 key safety（apiKeyHelper / 五种 secret backend / MaskKey） | **重要但规模不匹配**。swarm-rs 的 api_key 来自 CLI flag，本来就暴露在进程表里。真要做，最小版本是"key 从 stdin/文件读，不进 argv"——见下 |
| cc-fleet 的 flock 九把锁 | swarm-rs 单进程，SQLite 的 WAL + busy_timeout 已经够 |
| agent-fleet-manager 的 cadence 调度（next_due_at / 周期巡检） | swarm-rs 是一次性跑完，不是持续巡检 |
| agent-fleet-manager 的 OTLP 导出 | 有价值但属于可观测性基建，不是编排正确性 |

---

## 4. 该采纳的，按优先级

| # | 做什么 | 来自 | 为什么 | 成本 |
|---|---|---|---|---|
| 1 | **错误带稳定码，调用方 switch 而非 `contains`** | cc-fleet L114 | 我这轮自己写了 `reason.contains("conflict")`——正是它反对的模式。`RepoError::is_conflict()` 已存在却没用 | 小 |
| 2 | **submission 按内容哈希判重** | afm 规则 3 | 同一个 submission 处理两次现在会静默 `noop → accepted`，污染 accepted 率 | 小 |
| 3 | **`change_log` 加 `old_value` / `new_value`** | afm 规则 2 | 现在能回答"为什么被拒"，不能回答"状态从什么变成了什么" | 小 |
| 4 | **worker prompt 改白名单表述**："唯一出口是 POST /submit，除此之外不许写任何地方" | afm SKILL.md | 现在是黑名单（"别 commit scratch"），黑名单永远列不全 | 极小 |
| 5 | **api_key 支持 stdin/文件**，`--api-key` 标 deprecated | cc-fleet L102 | key 现在在进程表和 shell history 里。swarm-rs 是本地工具，风险低，但这是零成本的正确化 | 小 |
| 6 | **把 `sanitize_ref` / `strip_verbatim` 接上**（xtask 报的无调用者） | cc-fleet L121 | 和 `advance_hold` 同一类；目前无外部输入路径所以不危险，但名字进文件路径前必须校验 | 小 |

第 1 条是我自己刚犯的错，最该先修。第 4 条是一句话的事，但把"黑名单"改成"白名单"是真实的语义改进。

---

## 5. 实施结果（全部完成）

| # | 建议 | 结果 |
|---|---|---|
| 1 | 错误稳定码 | ✅ `RepoError::code()` + `remedy()`；runner 两处 `contains` 全部换成 `code()` switch |
| 2 | tree 判重 | ✅ `MergeResult.tree_sha` + `Repo::tree_of()` + `Store::last_accepted_tree()`；noop 现在区分"没新东西"和"已整合过" |
| 3 | 状态溯源 | ✅ `run_log` 加 `field`/`old_value`/`new_value`；accept / reject / duplicate 三条路径都写 |
| 4 | prompt 白名单 | ✅ 新增 `submit-is-the-only-door` 规则。**并抓出第 12 个缺陷**（见 §5.1） |
| 5 | api-key 不进 argv | ✅ `--api-key-file` / `--api-key-stdin`；`--api-key` 保留兼容 |
| 6 | 接上 sanitize_ref | ❌ **建议本身有误**：`sanitize_ref` 早已被 `worker_branch()` 调用。xtask 当时报的是另一批名字，我误读了。未改动 |

### 5.1 顺带抓出的第 12 个缺陷：`--no-create` 从未生效

加新规则后 golden 测试失败，追下去发现：

`build_worker_prompt` 重建 HARD RULES 块时，靠 `out.find(&render_hard_rules(DEFAULT_RULES))` 定位。但 `body_gen.rs` 是从 Go 渲染器**捕获的静态快照**，其规则顺序是 Go 的顺序，而 `render_hard_rules` 按 `(priority, id)` 排序——**两个字符串永远不同**，`find` 永远 `None`，替换静默跳过。

**后果**：`swarm run --no-create` 时，那条禁令从未进入任何一个 worker 的 prompt。`state.rs` 的注释说"prompt 是劝告，server 是硬闸"——server 侧确实拦着，所以没造成数据问题，但 prompt 层一直空转。

改成用 `HARD RULES:\n` banner + 空行定界，不再依赖规则顺序；banner 缺失时 `debug_assert` 响亮失败，而不是静默丢规则。

### 5.2 教训：这是同一个模式的第 N 次

| 缺陷 | 表面 | 实际 |
|---|---|---|
| `advance_hold` | 5 个单测 | 无生产调用者 |
| `evaluate_post_turn` | 传了参数 | 传的是硬编码 `true` |
| HARD RULES 重建 | 有替换逻辑 | `find` 永远失败 |
| `--no-create` prompt | README 说有 | 从未进 prompt |

**共同点：都有测试覆盖，都全绿。** 因为每个测试都自己提供了被测对象。

xtask 的"无生产调用者"检查抓不到这类——`find` 和替换**都有**调用者，只是逻辑永不成功。能抓它的只有"断言副作用真的发生了"：新测试断言新规则在 prompt 里**且**默认规则还在**且**只有一个 rules 块。

原来的测试 `assert!(p.contains("Do NOT create new items"))` 之所以空转，是因为那条字符串来自 `NO_CREATE_RULE` 常量，而 `rules` 向量里确实 `push` 了它——断言的是"我 push 过了"，不是"prompt 里有它"。