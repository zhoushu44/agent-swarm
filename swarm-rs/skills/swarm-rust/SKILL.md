---
name: swarm-rust
description: swarm 并行执行器 + wb 跨会话工作台。用 swarm 跑多智能体并行开发（"用 swarm""跑 swarm""把一批工作交给多个 worker"）；用 wb 跨会话记任务与经验（"接着上次""还有什么没做完""记一下这个""以后别再犯"）。会话开始时用 wb brief 恢复上下文。
version: 4.1.0
status: stable
---

# swarm-rs + wb

两个工具，一套工作流：

| | 干什么 | 什么时候用 |
|---|---|---|
| **swarm** | 把一批独立任务并行跑完，确定性调度 + 门禁 + 合并 | 有 N 个可验证的小任务，想并行 |
| **wb** | 跨会话记住「还有什么没做完」和「踩过什么坑」 | 会话开始 / 结束，干活途中随手记 |

> **与 `swarm`（v1.0.0，Node 版）的区别**：本 skill 指向 Rust 重写版 `swarm-rs`，
> 有资源级租约锁、半成品交接、item 去重折叠、`--api-key-file`、跨 run 记忆等能力。
> Node 版已停止维护。

---

# 第一部分：wb —— 先读这条

## 会话开始：`wb brief`

```bash
wb brief
```

```
=== WORKBENCH BRIEF ===
db: ~/.dsh/workbench/wb.db
scope: project:E:/path/to/repo

## TASKS (2)
▶ #12   [doing  ] p5   补全接口清单
! #13   [blocked] p3   等 cookie 失效

## MEMORY (2)
  [FACT] 千川后台不用 a_bogus；frontierSign 走自定义字节码 VM

(14 task(s) done, 31 note(s) total)
```

**这是唯一必须在会话开头跑的命令。** 一次给出「这个项目还有什么没做完」和「在这个项目里踩过什么坑」。

`▶` 是进行中的任务，额外打印 detail 前四行——上一个会话的接续点就在那里。

## 干活途中

```bash
wb task add "补全 764 个接口" --priority 5
wb task start 12
wb task done 12 --note "只挑了 25 条，其余在 shards/"
wb task block 13 --note "cookie 过期，需要重登"

wb mem add "webmssdk 的 opcode 用 13x mod 241 加密"
wb mem add "Playwright 在 Windows 上需要 --no-sandbox" --scope global
```

`--note` 是**给下一个会话看的**：写「做到哪、卡在哪、下一步」，不要写「已完成」。

## scope 怎么选

| scope | 用途 | 例子 |
|---|---|---|
| 默认（当前目录） | 只在这个项目成立 | 「这个仓库的 gate 脚本在 scripts/verify.js」 |
| `--scope global` | 本机层面，所有项目成立 | 「cmd.exe 里 git commit -m 带空格会截断」 |
| `--scope tag:xxx` | 跨项目的同类经验 | 「tag:playwright」 |

**判据：换个项目还成立吗？** 成立 → `global`；不成立 → 默认。

任务永远按项目隔离；记忆默认按项目，`global` 的每个项目都能看到。

## 记忆该写什么

**写**：工具/网站的意外行为、花了代价才排除的死路、代码里读不出来的约定
**不写**：任务状态、读代码就知道的事、密钥（文件是 `$HOME` 下明文）

## 会话结束前

1. 进行中的任务改成准确状态，带 `--note`
2. 会改变下次做法的结论 → `wb mem add`
3. **不要把「今天做了什么」写进记忆** —— 那是日志

## wb 其余命令

```bash
wb mem ls | wb mem search <query> | wb forget <id>
wb mem compact --api-base URL --api-key-file PATH --model M [--scope S] [--dry-run]
wb task ls --all | wb task ls --scope global
wb path
```

**压缩记忆**：`wb brief` 只显示 12 条，被噪音塞满就看不到有用的。先 `--dry-run` 看计划再执行。
三条硬约束保证不丢数据：每个输入 id 恰好出现一次；`FAIL` 只能合并不能丢；多源合并若逐字
包含某个源会被拒绝。模型不稳定时自动重试三次。

---

# 第二部分：swarm —— 何时用、何时别用

## 何时调用

- 用户说：用 swarm / 跑 swarm / swarm 跑这个任务
- 要把一批独立工作交给多个 worker 并行处理
- 需要发起、监控或排障一次 swarm run

## 何时**不**调用（先看这条）

- 单文件小改动、一次性脚本：直接改更省
- **任务需要长时间深度推理**（逆向算法、读混淆代码、理解复杂 spec）：worker 是无记忆的
  一次性会话，每条 item 独立跑，跨片语义拼不回来。这类活单开一个长上下文 agent。
- **输入是海量同类小文件**（几百个 JS chunk）：先确定性预筛，只把有信息量的交给 swarm。
  实测 72 个分片里 46 个是无关的 Babel helper，纯浪费。
- 目标目录不是 git 仓库且用户不愿 `git init`
- 没有可用的 OpenAI 兼容 API
- 任务需要严格串行依赖

## 只用 Rust 版

**每次开工先验证**：

```bash
swarm run        # Rust 版应回答: run: missing value for dir
                 # Node 版会打印完整 usage —— 那就是装错了
```

装错了就替换：

```powershell
Copy-Item "<repo>\swarm-rs\target\release\swarm.exe" "<某个 PATH 目录>\swarm.exe" -Force
```

`<repo>` = 仓库根（含 `cmd/ internal/ gen/` 与 `swarm-rs/`）。**不要写死绝对路径。**
二进制不存在先 `cargo build --workspace --release`。

## 跑之前的体检（必做）

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <repo>\dev\swarm-clean.ps1 -WorkDir <目标仓库>
```

只杀空 CommandLine 的僵尸 git 进程，保留 board db。仍报锁就手动 `git worktree prune` + 删 `.swarm/worktrees`。

**如果分片/文件被重命名过**，还要确认 integration 分支跟得上 master：

```bash
git -C <dir> branch -f integration master
```

否则 worktree 检出旧文件名，worker 读不到文件，表现为「所有 item 都 in_progress 但零产出」。

## 发起一次 run

```bash
swarm init <dir>
swarm seed <dir> --file seed.json
swarm run <dir> \
  --workers 4 --max-workers 16 \
  --harness direct \
  --api-base http://<host>:<port>/v1 \
  --api-key-file ~/.swarm-key \
  --model deepseek-v4-flash \
  --test-cmd "exit 0" \
  --exit-when-done --max-wall-clock 10m --idle-stop 3m
```

seed.json（title 最好是文件路径，去重和资源锁都靠它）：

```json
[
  { "title": "src/one.rs",
    "detail": "创建 src/one.rs，含一个函数与单测。DoD:\n- cargo test 通过",
    "priority": 10 }
]
```

## 八条硬规矩

1. **API key 用 `--api-key-file` 或 `--api-key-stdin`，绝不用 `--api-key`** —— 后者进进程表和 history
2. **跑之前先验证 key 有效**。401 的表现极具误导性
3. **item 数必须远大于 worker 数**（至少 5:1）。否则你测到的是调度器不是并发
4. **`--test-cmd` 是唯一的验收关**，写真实命令。`exit 0` 只用于联调
5. **别中途加 worker**。扩容由 autoscaler 决定
6. **`--no-create` 是双闸**：prompt 白名单 + server 硬拦
7. **seed 的 detail 里不要写真实文件路径**（除非就是想让它锁那个文件）。资源提取器把任何
   形如 `xxx.js` 的 token 当资源锁，导致所有 item 争同一把锁 → 除第一个外全部 409。踩过三次：
   - `DoD: node scripts/verify.js` → 254 个 item 抢 `scripts/verify.js`
   - detail 里写源文件名 → 72 个分片抢同一文件
   - **分片名本身含源文件名** → 又锁一次
   解法：detail 用标签（「webmssdk 去混淆版」）代替文件名，分片重编号成 `shard_NNN`
8. **`--max-item-seconds` 默认 300s 对 2MB+ 输入不够**。实测连续 5 次、每次 800s、零进展。
   大输入要么提到 1200s，要么（更好）预先切片

## 大文件怎么处理

`bash` 输出硬截断 **16KB**，`http` 截断 8KB。`cat` 一个 2MB bundle 只能看到 **0.8%**，
而模型不知道自己没看全。

| 情况 | 做法 |
|---|---|
| < 512 KB | 直接给 swarm |
| ≥ 512 KB 多行 | 按行块切（150 行/片） |
| ≥ 512 KB 单行 minified | **先正则打分预筛**，只把高分片切片 |
| 只需要几个字段 | `grep -o` / `jq` / `node -e`，**不进 swarm** |

swarm v0.3.0+ 内置单文件字节切片（`path#byte:START-END`，512KB 阈值，160KB/片，上限 24 片，
保证覆盖到 EOF），但**内置切片是兜底不是首选**——能预筛就别切。

## 监控与读报告

```bash
swarm status <dir>    # 每 3s 采样，信息密度最高
swarm board  <dir>    # 看板
swarm log    <dir>    # 默认只返回 50 条，看全量加 --limit 5000
```

报告里四行关键：

```
accepted: 7/11 items passed the gate (2 rejected)   # 交付质量
llm gate: limit=64 ceiling=64 slow_start=true rate_limits=0 backoffs=0   # 端点限流
breakers: item-hold=1 no-claim-no-submit=4          # 哪条熔断器在动作
seed work: 12/46 terminal                           # 用户视角的进度
```

**注意 `seed work: a/b` 的 b 可能大于 seed 数量**（worker 会自己拆子项）。`0 failed` + 有 pending
通常不是「任务失败」，是「autoscaler 停了」（v0.3.0 之前的常见现象）。

## 排障速查

**第一步永远是 `swarm log <dir> --limit 5000`** —— harness 的真实错误只写在那里。

| 症状 | 原因 | 处理 |
|---|---|---|
| `tokens: 0`、`0 api calls`、`breakers: give-up` | **401 invalid_api_key**。`api_calls` 只累计成功的 step | `swarm log` 确认；核对 key 文件 |
| `http=None: transport error` | 网络不可达或 `--api-base` 写错 | 先 curl 探活端点 |
| claim 全部 **500** | v0.3.0 前：`resource_leases.token` 单列主键，多资源必撞 UNIQUE | 升级 0.3.0+ |
| claim 全部 **409 needs resources** | seed detail 里写了真实文件路径，全板抢一把锁 | 见硬规矩 7 |
| 跑到 60-70% 停，`0 failed`，还有 pending | v0.3.0 前：GiveUp 是终态 + 健康退出 worker 被误判 stalled | 升级 0.3.0+ |
| 所有 item in_progress 但零 done、events 空 | worktree 检出旧基线（分片重命名后没更新 integration） | `git branch -f integration master` 后重跑 |
| 大文件反复释放重试，每次 ~800s | 超过 `--max-item-seconds` | 提高该值或预先切片 |
| `pending` 不降、`in_progress` 满 | worker 抢到活但模型不驱动工具 | 换模型或降 `--max-workers` |
| accepted 率低、大量 merge conflict | 多 worker 覆写同一文件 | brief 已提示重读；仍不行把 seed 拆细 |
| run 起不来 / worktree 报锁 | 上次残留 | 重跑 clean 脚本；仍不行手动 prune |
| token 猛涨、done 不动 | 模型原地空转 | guard 7/8 会 park；降 worker 数 |
| `rate_limits` 很高 | 端点限流 | 闸门自动退避；持续就降 worker 数 |
| 进程被杀 / 机器重启 | 状态都在 db 里 | **直接重跑同一条 `swarm run`**；不要重新 init/seed |

**反直觉的点**：`0 api calls` 不等于"没调 API"。`steps == 0` 时 guard 10 记 rate-limit 并退避，
autoscaler 放弃扩容——看起来像"调度器没跑起来"，实际是"每次调用都被拒"。

## 跨 run 记忆（v0.4.0+）

`~/.swarm/memory.db`，跑前自动注入、跑后自动沉淀（只收 `FACT`/`FAIL`，短于 40 字符丢弃）。
`--no-memory` 关掉，run 完全可由仓库复现。

```bash
swarm memory                                    # 列出
swarm memory add <scope> <text>
swarm memory search <query>
swarm memory compact --api-base URL --api-key-file PATH --model M [--scope S] [--dry-run]
swarm memory prune --days 30
```

**swarm memory vs wb**：做一般性工作记录用 `wb`（跨工具、跨项目）；只想让 swarm run 之间共享
经验用 `swarm memory`。两套文件独立，各有一份 compact 实现（故意重复——共用代码会让两个独立
版本的工具产生路径依赖，失败模式是「那个本该在别的东西坏掉时仍能工作的工具编译不过」）。

## 收尾

报告三件事：**完成了多少**（`accepted` / `seed work`）、**花了多少**（tokens / wall）、
**卡在哪**（`breakers` + FAIL 摘要）。不要只贴原始日志。

产物在 `<dir>/.swarm/integration`，git 历史完整可查。

**手工补产出**：run 卡在最后几个 item 时，与其反复重启，不如从 integration worktree 取出
已完成产物、用确定性脚本补齐剩下的——但要如实说明哪些是 swarm 产出的、哪些是脚本补的。

## 安全限制

- API key 只走 `--api-key-file` / `--api-key-stdin` / 环境变量，不写进 skill、seed、日志或回复
- 不在生产仓库直接跑：先在独立目录或新分支验证
- `--test-cmd` 只允许项目自身的只读测试命令
- **大并发前确认成本**：
  - 96 workers / 600 个中等任务 ≈ 1.9M tokens / 8 分钟
  - 一轮 254 item 的 JS 逆向分析 ≈ 1.2 亿 tokens / 约 1 小时
  - 并发稳定性上限实测 100–200 workers

## 断点续跑

**直接重跑同一条 `swarm run` 命令**：board 状态在 `.swarm/swarm.db` 里，未完成 item 仍是
`pending`，已合并的分支不受影响。**不要重新 `init` / `seed`** —— 会覆盖已有状态。

如果是 autoscaler 停了（`0 failed` 且有 pending）：先 `swarm-clean.ps1`（不加 `-ResetDB`）
清 worktree，再重跑同一条命令。

## 不要做

- 不要用 Node 版（`cli.js`）—— 没有资源租约、半成品交接、去重折叠
- 不要手动 `git merge` 到 integration —— 破坏 gate 账本
- 不要改 `<dir>/.swarm/swarm.db` —— 状态机只在 store 层改
- 不要 `--api-key` 传 key
- 不要在 item 数 < worker 数时开跑
- **不要把深度推理型任务塞给 swarm** —— 它擅长「N 个独立、可验证、有明确完成标准的小任务」，
  不是「想清楚一个问题」

---

# 一套完整的工作流

```bash
# 会话开始
wb brief

# 判断这批活适不适合 swarm
#   独立、可验证、有明确完成标准、item 数 >> worker 数  → 适合
#   深度推理、海量同类小文件、强串行                    → 别用

# 适合的话
wb task add "..." --priority 5      # 记进工作台
swarm init / seed / run              # 跑
swarm status / log                   # 盯
# 卡住就按排障表处理，别反复重启

# 干完
wb task done <id> --note "做到哪、卡在哪、下一步"
wb mem add "这轮学到的、会改变下次做法的东西"

# 会话结束
wb brief    # 确认状态干净
```

# 来源

- swarm Rust 重写版：`zhoushu44/agent-swarm` 的 `swarm-rs/` 子项目。
  运维知识来自 7 轮真模型压测（累计 ~700 个 item）与 20+ 项缺陷修复。
  v0.3.0 修的长跑停摆、大文件空转、资源锁污染三条，都是在 254/175/72 item 的
  真实任务上跑到 60-70% 卡死才定位到的。
- wb：本机自建，`C:\Users\Administrator\Desktop\workbench`。

