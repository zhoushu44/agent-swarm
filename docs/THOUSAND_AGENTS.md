# 千 Agent 化：swarm 扩展建议（v5.8.0 基线 + 实战记录）

> 基于 **swarm-go origin/main v5.8.0** 已实现能力 + RegiForge 三轮 swarm 实测
> （54 items / 84M tokens / 43 min / gate 0.6；6-item 逆向实战 done=9/16
> gate 全绿，产出 30KB 研究报告）。
> 只提**尚未实现**或**实测补充**的，不重复上游 README 已记录的机制。

## 实战记录：swarm 真的能干活（2026-10-31）

任务：逆向 NVGS 前端 JS，定位 `user/login` token 生成算法。

```
seed 6 items → workers 6 → 自主拆分到 16 items → done=9/16 → gate 全绿
→ 5 份研究报告入库（30KB）
```

swarm **做了我没让它做的事**：
- 发现 42 个 JS 里 **零 org 概念**（搜 organization/orgId/onboard/tenant
  全零命中）→ 直接推翻了「逆向前端拿 org token」这个前提
- 给出 `userIdOf(e){return e.split(".")[1]}` 这种**反向证实**
- 产出 144 个 endpoint 完整分组表（我只要求 6 个小任务）
- **自主创造 7 个新 item**（BONUS WORK）：`re-token-storage-model`、
  `re-auth-storage-keys`、`re-login-flow-synthesis` 等
- autoscaler 扩到 w8-w11，三层报告正确区分 SEED / BONUS / STILL OPEN

**结论：v5.8.0 已可承接真实逆向任务。** 下面是让它更稳的建议。

## 先说已解决的（别重复造）

| 我原打算提的 | 上游状态 |
|---|---|
| 内存瓶颈（96.5MB/进程） | ✅ Go 重写 **22.6MB，4.3× 省**；`--max-workers` 默认 1000 |
| gate 失败自动回滚 | ✅ `gitutil` gated integrate + rollback + **三路合并仲裁** |
| token 成本分层 | ✅ AIMD 闸门 + 慢启动 + 429 容量治理 |
| 进度可观测 | ✅ `swarm log` 封闭词表 + item 级 token 账本 + 三层报告 |
| 防白烧 token | ✅ no-progress guard（v5.8）+ 空产出熔断 + lease 超时 |
| 长任务卡死 | ✅ `--max-item-seconds` + 静默超时 `--idle-stop` |
| 符号级 split | ✅ `4c4f430` 已实现（实测 swarm 能自己按函数边界拆 2916 行文件） |

**地基已铺好。** 我旧版建议（内存/回滚/账本/符号级）全部作废。

## 实测踩到的三个坑（附解法）

### 坑 1：`swarm run --help` 会创建 `./--help/.swarm/` 目录

`--help` 不是 flag，被当成位置参数 `dir`：

```
C:/Users/zs/Desktop/多agent/swarm/--help/.swarm/worktrees/w1
```

worktree 注册表被污染成两份 → `syncWorktree` 混乱 → worker 认领了 item
却**从不调 LLM**（症状：`pending` 下降、`inprog` 上升、`0 tok/s`、
`0 api calls`、`WORKER_RETIRE: no claim and no submission in 7 turns`）。

**建议**：`main.go` run 分支加 guard：

```go
if strings.HasPrefix(dir, "-") {
    fmt.Fprintf(os.Stderr, "run: %q looks like a flag, not a directory\n", dir)
    os.Exit(1)
}
```

清理（实测有效）：`git worktree prune` + 删 `--help/` + 删 `.swarm/worktrees`
+ 删 `.swarm/swarm.db` + 删 `swarm/swarm-w*` 分支。
`dev/swarm-clean.ps1` 不覆盖这种「错目录」情形，建议一并纳入。

### 坑 2：首轮 LLM 响应 5 分钟，`0 tok/s` 是假象

`deepseek-v4-flash` 带 `reasoning_content`，单次响应 5s+，一个 turn 多轮
工具调用 → 首轮 3-5 分钟。期间状态行一直 `0 tok/s | NO PROGRESS`，
**像卡死，实际在正常跑**（第一次我因此误判为 bug 并放弃了一轮）。

**建议**：状态行区分「等 LLM」与「真卡住」。worker 处于 `working` 且
`heldItem != ""` 时标 `WAITING-LLM`，而不是和 `STALLED` 混在一起。
`NO PROGRESS` 的判据 `+N done +M subm` 对长首轮误报。

### 坑 3：模型不会主动 submit（长 item 尤其明显）

第一轮 6 个 item，每个 detail 500+ 字 → worker 一直写 `tmp_*.py` 探针
（被 bash guard 拦 3 次），3 轮无提交 → park。**9 分钟纯烧 token，
`submtd=0`。**

改成「每 item 只一个交付物 + detail 硬性写明最多 3 个工具调用后必须
submit」→ 立刻 done=9/16。

**建议**：写进 `prompts.DefaultRules`（比 v5.8 的 no-progress guard
更前置——guard 是止损，这条是预防）：

```
Submit discipline: after at most 3 tool calls you MUST call submit with
what you have. "I found nothing" is a valid submission — record the
exclusion evidence. Never explore "just one more file".
```

## 上游 Still open —— 我来补

### 1. `protectedPaths`：分两级，不是一律拦

上游说「需运营方确认」。但 RegiForge 教训：**运营方不在场时也得拦住**。

实测案例：subagent 把**假结论**写进 skill 文档、PowerShell 引号转义毁掉
3 个 `contract.json`、含真密钥的 `config.json.bak*` 被提交进 git。

```yaml
protected:
  deny:   ["data/config.json", ".env", "**/*.key", "**/*.bak*"]
  review: ["tests/test_secret_hygiene.py", "**/SKILL.md"]
```

`review` 级：merge 前跑「文档断言检查」——skill 里出现「已实测跑通」
必须附可复现命令，否则 reject。把「结论先行」变成机器可检。

### 2. GATE reason 封闭词表 + 分级重试

54 items / gate 0.6。失败原因分类：

| 类别 | 占比 | 可自动重试 |
|---|---|---|
| 测试真失败 | ~40% | ❌ 记录即可 |
| 环境/编码（PowerShell 引号、GBK emoji） | ~30% | ✅ **必重试** |
| 凭证误提交 | ~15% | ✅ 拦下后重新生成 diff |
| merge 冲突 | ~15% | ✅ 已有三路仲裁 |

**建议**：`swarm log` 的 GATE 事件加 `reason`（`test_fail` / `env_error` /
`secret_leak` / `merge_conflict`），runner 只对 `env_error` 和 `secret_leak`
自动重试。当前 gate 只记通过/失败，重试决策靠人看日志——千 agent 下不可行。

### 3. 长时外部任务不要作为 swarm item

让 worker 跑真实注册（浏览器 + 等邮件 177s-37min）会占满
`--max-item-seconds` lease 被回收 → 重复劳动；或卡到 `--idle-stop`
判定「无进展」→ 整轮误停。

**建议**（README 补一句）：
> 长时外部任务（浏览器自动化、等邮件、跑批）不要作为 swarm item。
> 拆成「协议分析 / 实现 / 写测试」这类数分钟内可提交的 item；
> 真实验证由运营方用后台 job 跑，结果作为下一个 item 的输入。

### 4. Item 结果缓存（跨轮）

RegiForge 有多个 agent 重复读同一个 `_hybrid_engine.py`（每次 ~1.5M
input tokens）。`(文件 digest, 任务类型)` 相同就复用上次分析摘要，
可省 20-40% input tokens。上游已有「增量上下文加载」，把 digest
持久化到 SQLite 即可做跨轮缓存。

### 5. 报告加「token 效率」维度（本轮实测数据）

本轮 TOP TOKEN SPEND：

```
t3   6.06M tokens / 280 calls / 7 turns   ← 单个 item 烧掉 6M
t11  4.28M / 200 / 5
t9   4.11M / 200 / 5
```

单 item 6M tokens 太贵。建议报告加：

```
tokens per terminal item   （当前 ~1.5-6M/item，方差极大）
tokens per gate pass       （gate 0.6 → 2.5M/有效 item）
```

第二个数才是真成本。加进三层报告 STILL OPEN 段，运营方才能判断
「该不该加 worker」——否则只看到 tokens 涨，不知道多少是重做。

## 优先级

| 项 | 工作量 | 收益 |
|---|---|---|
| `run <dir>` 拒 `-` 开头参数 | 5 行 | 防 worktree 污染（实测最坑） |
| DefaultRules 加 submit discipline | 1 条 | 防无尽探索（实测省大量 token） |
| GATE reason + 分级重试 | 小 | gate 0.6 → 0.85+ |
| 状态行区分 WAITING-LLM | 小 | 减少误判卡死 |
| protectedPaths 两级 | 小 | 防密钥泄露 + 假结论进主干 |
| token 效率报告 | 小 | 扩容决策有依据 |
| digest 跨轮缓存 | 中 | 省 20-40% input tokens |

## 相关

- 上游：`https://github.com/zhoushu44/agent-swarm`（Go 版，v5.8.0）
- RegiForge 守卫范例：`tests/test_guards_selfcheck.py`（负向验证）
- 本轮实战产物：`RegiForge/docs/reverse-engineering/`
- 安全清理脚本：`dev/swarm-clean.ps1`（Windows worktree 锁自愈）
