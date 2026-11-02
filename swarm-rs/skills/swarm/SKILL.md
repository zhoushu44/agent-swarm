---
name: swarm
description: 用 agent-swarm 的 Rust 版（swarm-rs）跑多智能体协作。当用户说"用 swarm"、"跑 swarm"、"swarm 跑这个任务"、要把一批工作交给多个 worker 并行处理、或需要发起/监控/排障一次 swarm run 时使用。只用 Rust 版，不用 Go 版。
allowed-tools: Bash, Read, Write, Edit, Glob, Grep
---

你是 swarm 运维 skill。目标：**用户只说一句"用 swarm 做 X"，你就能把整件事跑起来**。

# 只用 Rust 版

项目有两个实现，**必须用 Rust 版**：

| 版本 | 位置 | 用不用 |
|---|---|---|
| **Rust（本 skill 用这个）** | `<swarm-rs>/target/release/swarm.exe`，装到任意 PATH 目录 | ✅ |
| Go v5.8.0 | 仓库根的 `swarm.exe` | ❌ 只作回滚备份 |

Rust 版有的、Go 版没有的能力：资源级租约锁、半成品交接、item 去重折叠、`/marks` 报租约、`llm gate` 可观测、`--api-key-file`。

**每次开工先验证走的是 Rust 版**：

```bash
swarm run        # Rust 版应回答: run: missing value for dir
                 # Go 版会打印完整 usage —— 那就是装错了
```

如果 PATH 上是 Go 版，把 Rust 版装过去（任选一个在 PATH 上的目录）：

```powershell
Copy-Item "<swarm-rs>\target\release\swarm.exe" "<某个 PATH 目录>\swarm.exe" -Force
```

# 定位路径，不要假设

```bash
# 仓库根（含 cmd/ internal/ gen/ 与 swarm-rs/ 的那一层）
ls ./swarm-rs 2>/dev/null || find . -maxdepth 3 -name swarm-rs -type d 2>/dev/null
```

用 `<repo>` 表示仓库根，`<swarm-rs>` 表示 `<repo>/swarm-rs`。下面的命令都基于这两个变量，不要写死绝对路径。

二进制不存在就先构建（在 `<swarm-rs>/` 下）：

```bash
cargo build --workspace --release
```

# 跑之前的体检（必做）

worktree 残留会让 run 起来就失败。先清：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <repo>\dev\swarm-clean.ps1 -WorkDir <目标仓库>
```

只杀**空 CommandLine 的僵尸 git 进程**，不动你真正启动的 git；保留 board db。

# 发起一次 run

`swarm` 现在就是 Rust 版（已装到 PATH），直接用：

```bash
swarm init <dir>                      # 已初始化就跳过
swarm seed <dir> --file seed.json     # 或让用户给任务清单
swarm run <dir> \
  --workers 4 --max-workers 16 \
  --harness direct \
  --api-base http://8.163.52.51:7863/v1 \
  --api-key-file ~/.swarm-key \
  --model deepseek-v4-flash \
  --test-cmd "exit 0" \
  --exit-when-done --max-wall-clock 10m --idle-stop 3m
```

## 六条硬规矩

1. **API key 用 `--api-key-file` 或 `--api-key-stdin`，绝不用 `--api-key`** —— 后者会进进程表和 shell history。
2. **跑之前先验证 key 有效**。401 的表现极具误导性：报告显示 `0 api calls`（失败的调用不计入 `steps`），看起来像调度器没起来，实际是每次调用都被拒：

   ```bash
   curl -s -X POST http://<host>:<port>/v1/chat/completions \
     -H "Authorization: Bearer $(cat ~/.swarm-key)" \
     -H "Content-Type: application/json" \
     -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'
   ```

3. **item 数必须远大于 worker 数**（至少 5:1）。121 worker 抢 120 item 时，没抢到活儿的 worker 每秒 sleep 一次，你测到的是调度器不是并发。这是实测踩过的坑。
4. **`--test-cmd` 是唯一的验收关**，写真实测试命令。`exit 0` 只用于联调。
5. **别中途加 worker**。扩容由 autoscaler 决定，手改 `--max-workers` 要重启 run。
6. **`--no-create` 是双闸**：prompt 白名单 + server 硬拦。要严格限定范围就加上。

# 监控与读报告

```bash
swarm status <dir>    # 每 3s 采样，比 board 信息密度高
swarm board  <dir>    # 看板
swarm log    <dir>    # 事件流，排障主入口
```

报告里四行是关键：

```
accepted: 7/11 items passed the gate (2 rejected)   # 交付质量
llm gate: limit=64 ceiling=64 slow_start=true rate_limits=0 backoffs=0   # 端点限流情况
breakers: item-hold=1 no-claim-no-submit=4          # 哪条熔断器在动作
seed work: 12/46 terminal                           # 用户视角的进度
```

`rate_limits > 0` 说明端点在被限流，闸门在退避；`accepted` 率低于 50% 先看 `breakers` 和 `swarm log` 里的 FAIL 事件。

# 排障速查

**第一步永远是 `swarm log <dir>`** —— harness 的真实错误只写在那里，报告上看不到。

| 症状 | 原因 | 处理 |
|---|---|---|
| `tokens: 0`、`0 api calls`、`breakers: give-up` | **401 invalid_api_key**。`api_calls` 只累计成功的 step，失败的调用不计入，所以"0 次调用"可能意味着"调了但全被拒" | `swarm log` 确认是不是 401；核对 `--api-key-file` 里的 key 是否真实有效 |
| `http=None: transport error` | 网络不可达或 `--api-base` 写错 | 先 `curl` 探活端点 |
| `pending` 不降、`in_progress` 满 | worker 抢到活但模型不驱动工具 | 换模型或降 `--max-workers`；guard 11 会兜底停扩 |
| accepted 率低、大量 merge conflict | 多个 worker 覆写同一文件 | brief 里已提示"重读再 append"；仍不行就把 seed 拆细 |
| run 起不来 / worktree 报锁 | 上次残留 | 重跑 `swarm-clean.ps1`；必要时手动 `git worktree prune` + 删 `.swarm/worktrees` |
| token 猛涨、done 不动 | 模型在原地空转 | guard 7/8 会 park；必要时降 `--max-workers` |
| `rate_limits` 很高 | 端点限流 | 闸门自动退避；持续就降 worker 数 |

**一个反直觉的点**：`0 api calls` 不等于"没调 API"。`steps == 0` 时 guard 10 会记 rate-limit 并退避，然后 autoscaler 放弃扩容——整条链看起来像"调度器没跑起来"，实际是"每次调用都被拒"。

# 收尾

run 结束后报告三件事：**完成了多少**（`accepted` / `seed work`）、**花了多少**（tokens / wall）、**卡在哪**（`breakers` + FAIL 事件摘要）。不要只贴原始日志。

产物在 `<dir>/.swarm/integration`（已合并的分支），git 历史完整可查。

# 不要做

- **不要用 Go 版**（`swarm-go-v5.8.0.exe`）—— 它没有资源租约、半成品交接、去重折叠，会在同一批文件上撞车
- 不要手动 `git merge` 到 integration —— 会破坏 gate 账本
- 不要改 `<dir>/.swarm/swarm.db` —— 状态机只在 store 层改
- 不要 `--api-key` 传 key
- 不要在 item 数 < worker 数时开跑
