# swarm-rs

`agent-swarm`（Go v5.8.0）的 Rust 重写。**全部完成**：配置、存储、prompt、harness、git 集成、HTTP API、runner（14 条熔断器 + autoscaler + integrator）、CLI。

**当前版本 `v0.3.0`** —— 变更记录见 [`CHANGELOG.md`](./CHANGELOG.md)。

```
cargo test --workspace     # 333 passed
cargo build --workspace    # 0 warnings
```

## 快速开始

```bash
cargo build --workspace --release
./target/release/swarm init <dir>
./target/release/swarm seed <dir> --file seed.json
./target/release/swarm run <dir> \
  --workers 8 --max-workers 64 \
  --harness direct \
  --api-base https://api.deepseek.com/v1 \
  --api-key-file ~/.swarm-key \      # 不要让 key 进 argv
  --model deepseek-chat \
  --test-cmd "cargo test -p <crate>"
```

`swarm board|status|log <dir>` 查看看板；`cargo run -p xtask` 找出"有测试但无生产调用者"的方法。

## Skill：说一句"用 swarm"就能跑

[`skills/swarm`](./skills/swarm/SKILL.md) 是给 Claude Code 用的运维 skill，固化了实测得来的操作知识（命令、坑、指标读法）。安装：

```powershell
.\skills\install.ps1        # 装到 ~/.claude/skills/
```

装完重启 Claude Code，说"用 swarm 做 X"即触发。详见 [`skills/README.md`](./skills/README.md)。

## 已实现

| crate | 内容 | 测试 |
|---|---|---|
| `swarm-core` | 线上协议类型（Item/Claim/Event/Submission/Digest）、状态常量、错误→HTTP 映射 | 4 |
| **`swarm-config`** | `runConfig` 全 28 个 flag + 默认值、Go `time.ParseDuration` 兼容、`-` 开头目录拒绝、**api-key 走文件/stdin（不进 argv）** | 11 |
| `swarm-store` | Go 兼容 schema + 迁移、资源键提取、claim 事务语义、submission 生命周期、意图标记、**资源级租约**、**run_log 事实层**、**item 去重折叠** | 68 |
| `swarm-prompts` | worker prompt 构建，**与 Go 逐字节一致**（4 个 golden fixture）、DoD 段落提取 | 18 |
| `swarm-harness` | AIMD 闸门、bash guard、4 工具 schema、**agent loop**（re-prime + 并行保序）、direct HTTP 客户端（带 last-status 旁路）、**闸门并发守恒 + 429 闭环测试** | 75 |
| `swarm-git` | worktree 管理、gated integrate、三路合并仲裁、scratch 产物拦截、char 边界安全截断 | 36 |
| `swarm-server` | axum 12 路由 + 错误→HTTP 映射 + --no-create 硬守卫 + 意图标记 + `/marks` 同时报告租约 | 31 |
| `swarm-cli` | 14 条熔断器状态机、autoscaler、integrator、reaper、runner、CLI、**runner 接线测试** | 80 |

## 借鉴 Super-agent 的提升（v1.1）

对齐 DeepSeek Harness 团队《Super-agent 设计探索》的四项改动，方案与依据见
[`docs/SUPER-AGENT-IMPROVEMENT-PLAN.md`](./docs/SUPER-AGENT-IMPROVEMENT-PLAN.md)：

| 改动 | 解决什么 |
|---|---|
| **资源级租约**（`resource_leases` 表） | item 级 claim 挡不住「两个 item 写同一个文件」——碰撞原本要到 merge 才暴露。现在 claim 在同一事务内按资源键上锁，冲突时拒绝并点名资源。上游消融实验：租约锁返工 97.6→0、吞吐 +9%；LLM 协商烧 121 万 token 仍留 21 次竞态。 |
| **AIMD 闸门接线** | `aimd.rs` 421 行实现 + 15 个测试一直没被构造过，第一次 429 零退避。现在整池共享一个闸门，429/503 退避、成功则缓慢抬升。 |
| **`observe()` 不再造假字段** | 前六个字段修过后，`reply_says_idle` / `submitted_this_turn` / `turn_ok` / `http_status` 四个仍是字面量，而真值就在 turn result 里。提交过的 worker 和什么都没做的 worker 看起来一模一样。 |
| **`evaluate_post_turn` 不再收假 `turn_ok`** | 同一个 bug 模式的第三次：调用点传硬编码 `true`，于是一次传输错误会被当成"端点忽略 tool_choice"的证据，把 worker park 掉、丢掉它已做的工作。 |
| **`note_turn_result` 去掉三个死参数** | 签名收 5 个参数却 `let _ = (...)` 丢掉 3 个——死参数比没有参数更糟，它让调用点看起来在喂更多信息。 |
| **runner 接线测试**（`tests/runner_wiring.rs`） | `SWARM-RUN-ISSUES.md` §4 记的最大教训——「263 测试全绿和功能是空的可同时成立」。熔断器 70 个测试全喂手工 `World`，没有一个问过 `observe()` 产出的世界对不对。 |
| **DoD 段落 + accepted 指标** | 报告原来只答「做了多少」，不答「验收了几个」。`detail` 里的 `DoD:` 段落现在会进 worker prompt，报告输出 accepted / 一次验收通过率。 |
| **`run_log` 事实层** | 表在 Go 侧早有，Rust 侧**一行代码都没写**——移植遗漏。现在每次 accept/reject 都落一行（含原因），accepted 率由这些行派生而非内存计数器，进程退出后仍可重算。 |
| **取消不再轮询 + store 不再阻塞 runtime** | `cancelled()` 从 50ms 轮询改为 `Notify`；`observe()` 的 5 连查和 `digest` 改走 `spawn_blocking`——`max_workers` 上百时，同步查询会占满本就不多的 tokio worker 线程。 |
| **UTF-8 截断 panic**（M4 基准实测发现） | `String::truncate` 按字节切，切在多字节序列中间直接 panic。HTTP 回包 / bash 输出 / gate 失败信息 / 冲突文件视图共 4 处，全部改为 char 边界截断 + 回归测试。echo harness 测不到这条——它不产生输出。 |
| **半成品交接**（304-worker 压测暴露） | guard 9 收回任务时只说"换个方式"，不说上次干到哪。现在释放租约（否则别的 worker 撞 409 最多等 300s）+ FAIL 事件带上具体改了哪些文件和 head sha。 |
| **guard 9 时间判据从未生效**（`advance_hold` 无生产调用者） | `advance_hold` 有 5 个单测却从未被 `runner.rs` 调用，`held_for` 恒为 0，`--max-item-seconds` 是静默空操作。已接线，真实验证：`item-hold=1`，交接说明带 `changed 1 file(s): examples/demo.rs (head ebd582df1b)`。 |
| **AIMD 闸门可观测 + 上限受池子约束 + 429 闭环验证**（96-worker 压测发现） | 运行时 limit/ceiling/退避次数原本完全不可见，无法区分"端点健康"和"闸门没触发"。已补 `GateState` 并在报告打印。同时发现两个缺陷：① 慢启动把 ceiling 翻倍到 128，**超过 96 个 worker 的总数**，闸门永远不可能成为瓶颈（已加 `ceiling_floor`）；② storm 窗口在计数器之前 return，两个 429 只记一个。429 闭环已用本地假端点完整验证。 |
| **失败触发拆分** | 反复 gate 失败的 item 原来只是换人重试同一方案直到 `max_item_fails` 判死。现在自动拆成 per-resource 子项，各自独立过门禁。 |
| **基线过期提示** | merge 只让 worktree 跟上 integration，不告诉模型"有东西变了"——模型照旧按读到的旧内容整文件覆写，制造 merge conflict。现在 brief 里明说有几个 commit 落下、要求覆写前重读。**（首版有 bug：在 merge 之后才读 HEAD，提示永不触发；由对比压测抓出，已修并补回归测试）** |
| **错误稳定码**（学自 cc-fleet） | `RepoError::code()` 返回 UPPER_SNAKE 稳定词表，`remedy()` 给出补救。原来 runner 用 `reason.contains("conflict")` 分派——改一下错误文案行为就静默改变。 |
| **submission 按 tree 判重**（学自 agent-fleet-manager） | commit sha 每次 merge 都变，tree 才是内容身份。原来 noop 静默走 accepted，重复提交会虚涨 accepted 率。 |
| **run_log 状态溯源** | 加 `field` / `old_value` / `new_value`：原来只记"被拒了"，不记"从 submitted 变成 pending"。 |
| **prompt 白名单 + 修 HARD RULES 重建 bug** | 新增 `submit-is-the-only-door` 规则（白名单优于黑名单）。**并抓出第 12 个缺陷**：`--no-create` 规则一直没生效过——规则块重建靠字符串精确匹配，而 body 快照的顺序与 `render_hard_rules` 的排序不同，匹配永远失败、替换静默跳过。 |
| **api-key 不进 argv** | 新增 `--api-key-file` / `--api-key-stdin`。`--api-key` 保留兼容。 |

**一处已知限制**（有意固定，见 `store_behaviour.rs`）：
`src/video.js` 与 `src/video.js#pollVideo` 是不同的资源键——这正是「同文件不同函数可并行」的基础，代价是整文件 item 不会阻塞其中的符号 item。修它要教租约理解前缀包含关系，会改变所有调用方的去重与冲突语义，属于资源模型 owner 的独立决策；当前由三路合并仲裁兜底。

## agent loop 的两个关键行为

| 行为 | 为什么 |
|---|---|
| **纯文本 → re-prime（最多 3 次）** | 有些端点忽略 `tool_choice:"required"` 直接回散文。当成完成会让 worker 什么都没做就退出。但**已 submit 后**的散文收尾是合法的，不再 re-prime。 |
| **并行工具调用，结果按原序写回** | 模型会批量下发独立操作（"写 3 个文件"），并发执行把 N 次往返压成 1 次；但 transcript 顺序必须确定，否则模型看到的结果错位。 |

## 六条不能违反的约束

1. **schema 必须与 Go 版字节兼容** —— Go 写的 `swarm.db` 要能在这里打开。见 `schema.rs`。
2. **`claim` 必须接受 `in_progress`** —— 过期租约在事务内回收并退回 `pending`；门口拒绝会让 item 永久卡死。见 `store.rs::claim`。
3. **资源正则逐字一致** —— 它是「`error.message` 不算资源」的唯一依据，且去重键与冲突扫描共用。见 `resources.rs`。
4. **prompt 必须字节级一致** —— 一个空格就改变模型行为。`tests/golden.rs` 对着 Go 生成的 fixture 比对。
5. **`submitted` 只在 POST /submit 返回 2xx 时置位** —— 它决定「纯文本收尾」是合法结束还是需要 re-prime。
6. **`needsSplit` 是「seed + 无父 + 非 terminal + 无子」** —— 与资源数量无关；资源阈值是 runner 的 `splitThreshold`，别混为一谈。

## 14 条熔断器：全部可单测

Go 把它们内联在 400 行的 `workerLoop` 里，每条都和 `*store.Store`、`*gitutil.Repo`、HTTP harness 缠在一起，几乎测不了。

Rust 侧抽成 `breaker.rs` 里的 `CircuitBreaker`：**纯状态机，无 I/O**。worker 只负责观察现实、调用 `note_*`、然后服从 `evaluate()` 返回的 `Verdict`。每条熔断器都有 `guard_NN_*` 测试。

```
guard_01 idle-suppression      guard_08 empty-output
guard_02 budget-gate           guard_09 item-hold
guard_03 hard-turns            guard_10 rate-limit-backoff
guard_04 no-claim-no-submit    guard_11 autoscaler-give-up
guard_05 worktree-conflict     guard_12 resource-guard
guard_06 idle-detection        guard_13 exit-when-done
guard_07 no-progress           guard_14 run-ceiling
```

## 已验证的端到端行为

`cargo test -p swarm-cli --test e2e` 用真实 git + 真实 store + echo harness 跑完整 runner：

- 提名的 seed item 被 presplit 成 per-file 子项（父项转 `split`）
- 为每个 worker 建 worktree，`integration` 分支存在
- 按 wall-clock 自行停止，输出三层报告 + breaker 统计

二进制冒烟（`target/release/swarm`，8.45 MB）：

```
swarm init <dir>
swarm seed <dir> --file seed.json
swarm board <dir>          # board: 2 items
swarm status <dir>         # seed work: 0/2 terminal
swarm run --help           # 拒绝：looks like a flag, not a directory
```

## 复现 golden fixture

```bash
cd ..                # 仓库根
go run gen/gen_golden.go
cd swarm-rs && cargo test -p swarm-prompts
```

生成器同时重写 `swarm-prompts/src/body_gen.rs`（共享 body 常量）。

## 移植过程中发现的 Go 侧问题

用测试和 `go run` 直接验证逼出来的，全部记入 `../docs/OPTIMIZATION-SWARM.md`：

| 问题 | 证据 |
|---|---|
| `runBashTool` 硬编码 `cmd /c` | Linux 上每次 bash 工具调用都 `exec: "cmd": not found`，worker 完全无法执行命令 |
| `bash tmp_patch.sh` 绕过 bash guard | `go run` 调 `CheckBashCommand` 返回 `blocked=false`；解释器列表缺 `bash`/`sh` |
| `src\openai\http.js` 只提取出 `http.js` | 正则路径段 `[A-Za-z0-9_.@-]` 不含 `\`，Go 的 `ReplaceAll(m,"\\","/")` 对路径是死代码 |
| `reap_expired` 在 Rust 里会自死锁 | Go 用「先提交事务再清 marks」；Rust 的 `std::sync::Mutex` 不可重入，必须显式 `drop` |

## 排查"有测试但没接线"的缺陷

同一个形态的缺陷在这代码库里出现了四次：`observe()` 的六个 0、`evaluate_post_turn` 的假 `turn_ok`、`advance_hold` 无调用者、`reap_backoff` 无调用者。共同点是**单元测试全绿而生产行为是空的**——因为每个测试都自己提供了被测对象。

`cargo build` 抓不到：`pub` 方法对编译器不是死代码，测试也算使用。

```bash
cargo run -p xtask            # 列出没有生产调用者的 pub 方法
```

它扫描 `crates/`，把每个文件在 `#[cfg(test)]` 处切开，只数生产代码里的调用点。当前输出 21 个候选，逐个看过都是访问器（`no_progress_streak`）或可选的对称操作（`note_progress` 是 `note_stalled_spawn` 的反操作，runner 走共享 `AtomicUsize`）。

**它当场抓出的两个缺陷：**

| 方法 | 症状 |
|---|---|
| `reap_backoff` | `reap_fail_count` 只增不读，reaper 每 10s 重试永不退避。已按 `10→20→40→…→320s` 接上 |
| `artifact_key` | 被 `pub use ... as dedup_key` 重新导出后**无人调用**。Go 的两级去重（`items.go` L358-395）整个缺失——注释却一直写着 "including the dedup-key check"。已实现 |

去重折叠的边界（实现后由 e2e 抓出）：任何 open item 与新建 item 的文件键相同就会折叠，**包括 presplit 的子项**。所以 `refactor a.js and b.js` 拆出的 `a.js` 子项，会被一个已存在的 `fix a.js` seed 吸收。这与 Go 的行为一致（同样的 peer 判据），不是回归——e2e 的 seed 改成命名 `c.js` 并注明原因。

> 这个工具第一次运行时报告"no test-only public methods found"。那是假的：根目录多退了一级，扫到 0 个文件。**一个永远通过检查比没有检查更糟**，所以它现在会在扫到 0 文件时明确报错而不是输出"干净"。

## 下一步

| 阶段 | 内容 |
|---|---|
| M4 | **接真模型已验证**（`deepseek-v4-flash`，20 item，[结果见 `docs/M4-BENCHMARK.md`](./docs/M4-BENCHMARK.md)）：10/10 模块真实实现，integration 分支 38 单测 + 19 doctest 全绿。过程中修掉一个真 bug（多字节 UTF-8 截断 panic，4 处）。**仍未做**：AIMD 退避路径验证（需压到 429）、与 Go 版的 wall/RSS/token 三角对比 |
| M5 | CLI 同名替换，生产灰度，Go 版保留回滚 |

仍未做（`docs/SUPER-AGENT-IMPROVEMENT-PLAN.md` §5 明确列为「不照搬」）：Router 分层/联邦、A2A 协议栈、端侧 daemon 零信任、记忆压实 Worker、五档自治旋钮——swarm-rs 是单机单信任域的离线编排器，这些解决的是它现在没有的问题。

完整方案见 [`../docs/RUST-TARGET-ARCHITECTURE.md`](../docs/RUST-TARGET-ARCHITECTURE.md)。

