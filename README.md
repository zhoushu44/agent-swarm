# swarm — 去中心化多 agent 软件开发编排器

把"写码 + 审核 + 优化"的软件开发工作，拆给 N 个互相独立、无中央调度器的 agent 并行完成。
目标：在**墙钟时间最短**的前提下，用 200~1000 个 agent 替代单个主 agent 的慢速串行体验。

架构基座来自微软 Agensh 论文（arXiv:2609.26781v1）的自组织多 agent harness：
共享 workspace + 消息接口 + append-only 共享上下文数据库，配合"五步合作循环"。

---

## 给别人用（5 分钟上手）

### 前置要求

- **Node.js** ≥ 18（推荐 20+）
- **Git** ≥ 2.30
- **Claude Code** 或其他支持 skill 的 AI 编码工具（推荐）
- **Anthropic 兼容 API**（中转站或官方 API）

### 安装

```bash
# 1. 克隆仓库
git clone <repo-url> swarm
cd swarm

# 2. 安装依赖（只有 better-sqlite3 一个原生依赖）
npm install

# 3. 配置 API（三选一）
# 方式 1：环境变量（推荐）
export SWARM_API_BASE="https://your-relay.com/v1"
export SWARM_API_KEY="sk-..."
export SWARM_MODEL="cn:claude-sonnet-4"

# 方式 2：配置文件
mkdir ~/.swarm
cat > ~/.swarm/api.json <<EOF
{
  "baseUrl": "https://your-relay.com/v1",
  "apiKey": "sk-...",
  "model": "cn:claude-sonnet-4"
}
EOF

# 方式 3：命令行参数（每次 run 时传 --api-base / --api-key / --model）
```

### 使用方式：通过 skill（强烈推荐）

swarm **专门设计为通过 AI 编码工具的 skill 调用**，不要手动敲 CLI。

#### skill 功能说明

swarm skill 提供 **7 大核心能力**：

1. **任务拆解**：把一句话 mission（如"实现一个 TODO CLI"）自动拆成 N 个子任务（默认 6 个）
2. **并行执行**：拉起 N 个独立 worker（默认 8 个，最多 1000 个），每个 worker 自己认领任务、写代码、跑测试
3. **自动集成**：worker 提交代码后，集成器自动 merge 到主分支，跑测试门禁，失败自动回滚
4. **冲突处理**：
   - scratch 文件拦截（拒绝 `probe.*` / `tmp_*` / `*.log` 等调试文件）
   - 文本清单合并（`LICENSE` / `.gitignore` 等白名单文件的 add/add 冲突自动合并）
   - LLM 三路仲裁（源码冲突时让 LLM 裁决合并）
5. **崩溃续跑**：中途崩溃后重新 `run <repoDir>`（不传参数）自动从断点续跑
6. **实时监控**：跑到 `http://127.0.0.1:17771/dashboard` 看进度 / 树状分解 / token 用量 / 集成记录
7. **错误观测**：worker 崩溃按类型聚合（network / rate_limit / timeout 等），`status` 命令查看 `crashStats`

**触发条件**：用户在 Claude Code 里说以下任一句话，skill 自动激活：
- "并行开发" / "多 agent" / "拆分任务" / "swarm"
- "用 swarm 实现 XXX" / "让 swarm 做 XXX"
- "把 XXX 拆给多个 agent 并行做"
- "调用 swarm 帮我完成 XXX"

**首次使用前自动检查**：skill 文件里内置了安装检查逻辑，Claude Code 第一次调用时会自动验证：
- swarm 仓库是否存在
- 依赖是否安装（`node_modules/better-sqlite3`）
- API 配置是否就绪（环境变量 / `~/.swarm/api.json` / 命令行参数）

如果检查失败，Claude Code 会自动引导用户完成安装。

#### Claude Code skill（推荐）

**一键安装**：

```bash
# 在 swarm 仓库根目录执行
node scripts/install-skill.js
```

这个脚本会：
1. 检测 Claude Code 配置目录（`~/.config/claude-code/skills`）
2. 创建 `swarm.md` skill 文件（自动填入当前 swarm 仓库路径）
3. 验证 API 配置（`~/.swarm/api.json` 或环境变量）
4. 如果没装依赖，自动跑 `npm install`

**手动安装**（如果脚本不存在）：

```bash
# 1. 创建 skill 目录
mkdir -p ~/.config/claude-code/skills

# 2. 创建 skill 文件（注意替换 <SWARM_PATH> 为实际路径）
cat > ~/.config/claude-code/skills/swarm.md <<'EOF'
---
name: swarm
description: 去中心化多 agent 并行开发。把大任务拆给 N 个 agent 并行完成，自动集成、测试、合并。
---

# swarm skill

当用户要求"并行开发"、"多 agent"、"拆分任务"、"swarm"时，调用此 skill。

## 首次使用前检查

在调用任何 swarm 命令前，**必须先检查安装状态**：

```bash
# 1. 检查 swarm 仓库是否存在
ls <SWARM_PATH>/cli.js

# 2. 检查依赖是否安装
ls <SWARM_PATH>/node_modules/better-sqlite3

# 3. 检查 API 配置（三选一）
# - 环境变量：SWARM_API_BASE + SWARM_API_KEY
# - 配置文件：~/.swarm/api.json
# - 命令行参数：--api-base + --api-key

# 如果任何检查失败，先执行安装：
cd <SWARM_PATH> && npm install
# 然后配置 API（见 README）
```

## 用法

用户提供：
- `repoDir`：目标项目目录（绝对路径）
- `mission`：任务描述（一句话）
- `testCmd`（可选）：集成门禁测试命令（如 `npm test`）
- `maxWorkers`（可选）：worker 数上限（默认 8）

## 执行步骤

1. **检查安装**（首次必须）：
   ```bash
   ls <SWARM_PATH>/cli.js && ls <SWARM_PATH>/node_modules
   ```

2. 初始化：
   ```bash
   node <SWARM_PATH>/cli.js init <repoDir>
   ```

3. 播种：
   ```bash
   node <SWARM_PATH>/cli.js seed <repoDir> --mission "<mission>" --count 6
   ```

4. 开跑：
   ```bash
   node <SWARM_PATH>/cli.js run <repoDir> --harness direct \
     --mission "<mission>" \
     --test-cmd "<testCmd>" \
     --max-workers <maxWorkers> \
     --exit-when-done
   ```

5. 监控：打开 `http://127.0.0.1:17771/dashboard`

## 注意事项

- `<SWARM_PATH>` 是 swarm 仓库的绝对路径（安装时自动填入）
- 跑完后自动退出（`--exit-when-done`）
- 崩溃了直接重新 `run <repoDir>`（不传参数）自动续跑
- **首次使用前必须检查安装**（见上方"首次使用前检查"）
EOF

# 3. 替换 <SWARM_PATH> 为实际路径（比如）
sed -i "s|<SWARM_PATH>|$(pwd)|g" ~/.config/claude-code/skills/swarm.md  # macOS/Linux
# Windows PowerShell:
# (Get-Content ~/.config/claude-code/skills/swarm.md) -replace '<SWARM_PATH>', (Get-Location).Path | Set-Content ~/.config/claude-code/skills/swarm.md
```

**使用**：

在 Claude Code 里输入：
```
/swarm 帮我实现一个 TODO CLI，项目目录是 C:/Users/zs/projects/my-todo
```

Claude Code 会：
1. 读取 `~/.config/claude-code/skills/swarm.md`
2. 按 skill 里的"首次使用前检查"验证安装
3. 如果没装依赖，提示你先跑 `npm install`
4. 如果装好了，自动执行 init → seed → run → 打开 dashboard

#### 其他 AI 工具（Cursor / Windsurf / 自研）

只要支持"调用外部命令"的 AI 工具都能用。在 AI 工具的 prompt 里告诉它：

```
当用户要求并行开发时：

1. 首次使用前检查：
   - 检查 swarm 仓库是否存在：ls /path/to/swarm/cli.js
   - 检查依赖是否安装：ls /path/to/swarm/node_modules
   - 如果没装，先执行：cd /path/to/swarm && npm install

2. 执行命令：
   node /path/to/swarm/cli.js init <repoDir>
   node /path/to/swarm/cli.js seed <repoDir> --mission '<mission>'
   node /path/to/swarm/cli.js run <repoDir> --harness direct --mission '<mission>' --test-cmd 'npm test' --max-workers 8 --exit-when-done
```

### 使用方式：直接 CLI（仅调试）

如果不用 skill，也可以直接敲命令：

```bash
# 1. 初始化
node cli.js init ../my-project

# 2. 播种
node cli.js seed ../my-project --mission "实现一个 TODO CLI" --count 6

# 3. 开跑
node cli.js run ../my-project --harness direct \
  --mission "实现一个 TODO CLI" \
  --test-cmd "npm test" \
  --max-workers 8 \
  --exit-when-done

# 4. 查看状态
node cli.js board ../my-project
node cli.js status ../my-project
```

### 监控

跑到 `http://127.0.0.1:17771/dashboard` 看实时面板（进度 / 树状分解 / token 用量 / 集成记录）。

### 常见问题

**Q: 跑一半崩溃了怎么办？**  
A: 直接 `node cli.js run ../my-project`（不传任何参数），自动从上次断点续跑。

**Q: 怎么知道 worker 为什么挂了？**  
A: `node cli.js status ../my-project` 看 `crashStats` 字段，按错误类型聚合（network / rate_limit / timeout 等）。

**Q: 集成冲突太多怎么办？**  
A: swarm 会自动处理三类冲突（scratch 文件拦截 / 文本清单合并 / LLM 三路仲裁），如果还是频繁失败，检查 mission 拆解粒度是否太细。

**Q: 电脑被跑卡死了 / node 进程杀了又活怎么办？**  
A: 两个可能。① 并发开太高：删掉 `--no-resource-guard`，让资源护栏生效（默认就是开的），
并把 `--max-workers` 降到机器核数一半以内。② 有外部看门狗在保活：查 `schtasks /query`、
启动项和项目里的 `.swarm/watchdog.ps1`——swarm 自己不需要任何保活装置，杀进程时连它的
`cmd.exe` 父进程一起结束即可（详见「运行时安全」）。

**Q: 成本多少？**  
A: 实测 100 workers 跑一个中等 mission（39 任务、296 测试）约 600M tokens，按中转站定价约 ¥500-2000。

---

## 设计约束（硬性）

- **全并发**：没有全局 barrier，没有"等所有 agent 到齐再继续"的阶段。
- **主模型/lead 不在关键路径上**：没有任何中央编排器给 worker 派活。worker 自己拉 board、自己认领、自己提交。
- **薄编排层 + 复用执行层**：编排逻辑（claim/集成/伸缩）自写且极薄；真正干活的执行层直接复用本机已有能力
  （`direct` harness 直连 Anthropic 兼容中转站，或 `pi`/`claude`/`codex` CLI）。
- **状态外置**：每个 worker 每回合都是全新进程、无记忆；所有持久状态在 store（SQLite）里，因此崩溃可恢复、模型可随时替换。

## 快速开始

```bash
# 1. 初始化目标仓库（git init + 建集成分支 swarm/integration）
node cli.js init ../my-project

# 2. 播种工作项：让 LLM 把 mission 拆成首批顶层任务（也可 --file seed.json 手写）
node cli.js seed ../my-project --mission "实现一个支持 OAuth 登录的 REST API" --count 6

# 3. 开跑（direct harness = 直连 API，成本最低，200-1000 agent 首选）
node cli.js run ../my-project --harness direct \
  --mission "实现一个支持 OAuth 登录的 REST API" \
  --test-cmd "npm test" --max-workers 16 --exit-when-done

# 4. 查看状态
node cli.js board  ../my-project
node cli.js status ../my-project
```

跑到 `http://127.0.0.1:17771/dashboard` 可以看到实时面板（进度 / 树状分解 / token 用量 / 集成记录）。

## 命令

| 命令 | 说明 |
| --- | --- |
| `swarm init <repoDir>` | 初始化：`git init` + 创建集成分支 `swarm/integration` |
| `swarm seed <repoDir> --file seed.json` | 从 JSON 播种工作项与共享上下文 |
| `swarm seed <repoDir> --mission "..."` | 由 LLM 自动把 mission 拆成首批顶层工作项（`--count N` 控制数量） |
| `swarm run <repoDir> --mission "..."` | 启动编排：拉起 worker 池 + 集成器 + autoscaler |
| `swarm board <repoDir>` | 列出全部工作项 |
| `swarm status <repoDir>` | 汇总状态：进度 / 活跃租约 / 待集成提交 / 最近集成 |

### `run` 常用 flag

| flag | 默认 | 说明 |
| --- | --- | --- |
| `--harness` | `pi` | 执行底座：`direct` / `pi` / `claude` / `codex` / `echo`（离线测试） |
| `--model NAME` | — | 覆盖模型（中转站模型名需带 `cn:` 前缀） |
| `--max-workers N` | `8` | worker 池上限 |
| `--min-workers N` | `1` | worker 池下限（autoscaler 的地板） |
| `--max-turns N` | `10` | 每个 worker 最多跑几个回合 |
| `--test-cmd "..."` | — | 集成门禁测试命令；通过才保留合并，失败自动回滚 |
| `--max-inflight N` | 自适应 | 全局在飞 API 请求上限（默认 `clamp(workers×4, 8, 64)`，见下） |
| `--max-items N` | `200` | 任务发散护栏：总 item 数达到上限后停止 auto-split |
| `--plan` | off | board 为空时先自动规划首批工作项再开跑 |
| `--port N` | `17771` | 协调 API 与 dashboard 端口 |
| `--conventions FILE` | — | 项目规范文本，注入每个 worker 的 prompt |
| `--seed FILE` | — | 开跑前先播种的 seed JSON |
| `--exit-when-done` | off | board 清空 + 集成队列空 + 无活跃 worker 时优雅退出，打印 `[swarm] DONE {json}` |

## 核心机制

### 五步合作循环（写在 prompt 里，不在运行时硬编码）

每个 worker 每回合按序执行：**收集上下文 → 认领或创建工作 → 执行 → 验证 → 提交**。
prompt 里不塞任何全局状态（board 内容、别人干了什么），agent 每回合自己拉——因此 prompt 大小与 agent 总数无关，规模可扩展。

### claim / fence 租约协议

- 认领是**原子**的：`POST /claim` 竞争失败返回 409，worker 换一个项即可（race 是常态，不是错误）。
- 租约默认 300s，过期后 `submit` 会被 409 拒绝；长回合可 `POST /renew` 续期。
- **claim 不改变 item 的 status**——认领只是拿租约，完成由"提交 + 门禁"决定。

### 容器不变式 + 拆解幂等

- 已有子项的 item 不可再被认领（它是容器，不是叶子）；子项全 done 时父项自动级联完成。
- 拆解按 `dedupKey(title)` 幂等：从标题里抽取确定性交付物名（带扩展名的文件路径 token），
  同一父项下同一交付物只建一个子项。实测防止了 10 个 worker 把一个大任务重拆成 91 个重复子项导致父项永远等不齐。

### 集成器 + 门禁（gate）

worker 提交后不直接改主干。集成器单独串行循环：

1. 按序把 worker 分支 `merge --no-ff` 进集成分支；
2. 在集成 worktree 跑 `--test-cmd`；
3. 通过 → 保留合并、item 标记 done；失败 → `reset --hard` 回滚到合并前 sha、item 退回 pending 并指数退避。

合并串行是 git 语义要求，但跑门禁测试**不阻塞** worker 继续干活。

### autoscaling

autoscaler 是独立异步循环，只看 board 计数（不看内容）：

- 广度信号 `openCount`：所有未完成工作项数；
- 深度信号 `recentSplitCount`：最近拆解出的子项数——单一大任务时 pending 只有 1~2 个，光看 pending 会永远停在 minWorkers。

`want = clamp(demand, minWorkers, maxWorkers)`。扩容按批并发 `git worktree add`（默认每批 16、每轮上限 64、轮询 1s），
缩容只把 idle worker 标记 retired 让其自然退出（不 kill 强杀）。

### 全局在飞 API 闸门

所有 worker 共享同一个 Node 进程，也就共享同一个上游配额。没有闸门时 1000 个 worker 会同时打请求，
把中转站压死（大面积 503 "system cpu overloaded"），反而拖长总墙钟时间。
进程级信号量把"在飞 API 请求数"钳在上限内（默认 `clamp(workers×4, 8, 64)`，
`--max-inflight` / `SWARM_MAX_INFLIGHT` 覆盖，`0` = 不限流）。

> 2026-10 修正：旧默认是 `max(64, workers)`——在只有 2~6 个 worker 的小机器上闸门等于形同虚设，
> 一次 run 能同时挂几十条连接，把本机网络栈和中转站一起拖慢。现在跟随 worker 数自适应，
> 下限 8 防止并发过低拖长墙钟，上限 64 防止撞 429 反而更慢。

### 崩溃恢复

- `_replay()`：把 append-only 事件流折叠成每个工作项的应然状态，与派生表比对；只做**单向、不丢工作**的修正
  （事件说 done 而表里没 done → 补 done），绝不把 done 退回 pending。
- `releaseOrphanClaims()`：`run` 启动时（尚无 worker 存活）把上次进程崩溃留下的活跃租约全部作废，
  否则对应项要干等租约自然过期才能被重认领。
- `recoverIntegrations()`：扫集成分支的 `Agensh-Integration:` trailer，补回"已 merge 但崩溃在落库之前"的集成记录。
- **持久化续跑**（2026-10 新增）：`run` 启动时把 mission/testCmd/harness/model/maxWorkers 等元数据写进 `kv` 表；
  崩溃后再次 `swarm run <dir>`（不传任何参数）自动从 kv 读回，接力未完成的 mission。

> 恢复只在 `run` 启动时执行，刻意不放进 Store 构造函数——`status`/`board` 会打开同一个库，放构造函数会误杀正在跑的租约。

### 省 token（KV / 前缀缓存友好）

worker prompt 的正文段对所有 worker **逐字节相同**（身份/端口/token 用 `{PEER}`/`{API}` 占位符），
易变内容按易变度从低到高排在尾部：`共享正文 → IDENTITY(每 worker 固定) → TURN BRIEF(每回合变) → turn 号`。
于是跨 worker 共享整段正文前缀，同一 worker 跨回合共享到 IDENTITY 为止。实测 w1/w2 公共前缀 99.1%，w1 跨回合 99.8%。

### 空转抑制

worker 手头无租约、board 也无 claimable 项时退避轮询，不烧 LLM 回合；冷启动（`total === 0`）豁免。

## 质量护栏（2026-10 新增）

### 集成门禁增强

- **scratch 文件拦截**：集成时按文件名模式拦截调试/临时文件（`probe.*` / `tmp_*` / `*.log` / `scratch/` 等），拒绝整个 merge；worker 在 CONFLICT 简报里看到文件名，下回合清理后重新提交。
- **无测试文件跳过**：`node --test test/*.test.js` 这类门禁在 test 目录为空时直接放行（输出 `(skipped: no test files yet)`），等真实测试落地后再门禁——避免 src 模块先于 test 提交时的时序性空跑。
- **文本清单合并**：`LICENSE` / `.gitignore` / `.npmrc` 等白名单文件的 add/add 冲突做行级去重并集合并（不再回滚让 worker 重试）。
- **LLM 三路仲裁**：白名单外的源码冲突（两个 worker 同时改 `utils.js`）先跑 `git merge-file` 三路合并；有冲突块时把 base/ours/theirs 发给 LLM 裁决，输出必须通过 `node --check` 语法校验。单次最多仲裁 3 个文件（防无限烧 token）。

### 系统资源护栏（2026-10 新增）

事故背景：一次 `--max-workers 12 --max-inflight 64` 的 run 跑在普通 PC 上（外加外部看门狗保活），
把机器 CPU/内存耗干直接卡死。根因不是 Node 性能，而是项目此前**零资源感知**——机器多大都敢开 N 个 worker。

护栏分两层：

1. **默认值自适应**：不传 `--max-workers` 时，上限取 `max(2, ceil(CPU核数 / 2))`，不再无脑默认 8。
2. **运行时动态钳制**（`src/sysload.js`）：autoscaler 每轮扩容前采一次系统快照，
   按压力档位钳制 `want`：

| 档位 | 触发条件 | 行为 |
|---|---|---|
| `ok` | CPU < 80% 且可用内存 > 15% | 不设上限，用户 `--max-workers` 说了算 |
| `high` | CPU > 80% 或可用内存 < 15% | `ceiling = active`，只许停不许扩 |
| `critical` | CPU > 92% 或可用内存 < 8% | `ceiling = active - 1`，主动缩一个 |

要点：

- **跨平台**：Windows 上 `os.loadavg()` 恒为 `[0,0,0]`，禁用；CPU 占用率用 `os.cpus()` 的
  `times` 两次采样差分计算（首样本返回 `null`，不误判）。
- **只拦增长，不替用户做决定**：机器健康时不会因为核数少就压掉你显式设置的 `--max-workers`。
- **可观测**：`GET /status` 增加 `resourceGuard: { enabled, pressure, ceiling, sample }`；
  压力档位变化时发 `autoscaler.guard_pressure` 事件（只在变化时发，不刷屏）。
- **逃生开关**：`--no-resource-guard` 完全关闭；启动行会打印 `resource-guard=on|OFF`。
- 测试：`npm run test:guard`（纯函数 + 采样器，离线零依赖）。

> 注意：护栏防的是"本机被自己跑死"，不防"外部看门狗每 2 分钟重启你"。后者请用
> `--exit-when-done` + 计划任务自身的运行时限，别用无上限的保活脚本。

### 任务发散护栏

- **auto-split 上限**：总 item 数达到 `maxItems`（默认 200，`--max-items` 覆盖）后停止拆解，仅熔断释放。
- **拆解树深度上限**：4 层，防止无限拆解。
- **事件**：`worker.item_split_capped` 记录被拦截的拆解尝试。

### prompt 治理

- **RULES 注册表**：HARD RULES 从硬编码字符串改成结构化注册表（每条规则带 ID/priority/category/rationale）。
- **冲突检测**：`validateRules()` 在 `run` 启动时检测两类冲突（同 category 同 priority 过多、ALWAYS vs NEVER 关键词互斥），warnings 打印、errors 退出。
- **barrel 延后**：planner prompt 新增规则 "AGGREGATOR/BARREL files LAST"——index.js/README.md 这类只负责 re-export 的汇总文件必须最后做，避免依赖未就绪时反复重写。

### 错误观测

- **错误分类**：`_classifyError(e)` 把无结构的 Error 消息聚类成 10 个桶（network / rate_limit / auth / api_5xx / timeout / disk_full / permission / git / config / parse / unknown）。
- **事件**：`worker.crashed` 带 `category` 字段。
- **聚合统计**：`status()` 输出 `crashStats`（按 category 聚合），100 workers 挂了 5 个直接看到 `{"network": 3, "rate_limit": 2}` 而不是 5 条无结构文本。

## 资源隔离演进路线

单进程 Node 现在（100-200 workers）够用，500+ workers 会撞墙（事件循环瓶颈、内存泄漏传染、SQLite 锁竞争、子进程爆炸）。

**分阶段演进**（详见 [docs/RESOURCE_ISOLATION.md](docs/RESOURCE_ISOLATION.md)）：

| 阶段 | worker 数 | 架构 | 工作量 | 现状 |
|---|---|---|---|---|
| 1️⃣ | ≤ 200 | 单进程 Node | 已完成 | ✅ 稳定运行 |
| 2️⃣ | 200-800 | worker_threads | 1-2 天 | 🔲 需要时再做 |
| 3️⃣ | 1000+ | Go 重写 | 1-2 周 | 🔲 远期目标 |

**不要现在换 Go**——等实测撞到墙了再升级。

## harness 一览

| harness | 说明 |
| --- | --- |
| `direct` | **推荐底座**。绕过 CLI 直连 Anthropic 兼容 API，只发 worker prompt（省掉每回合 ~4400 tokens 的 CLI 系统提示词），无每回合进程启动开销。工具：`bash` / `write` / `http` / `finish`。 |
| `pi` | pi CLI（`pi -p` 无会话回合），工具 read/bash/edit/write（Windows 另有 powershell）。 |
| `claude` | `claude -p`（每回合起 CLI 进程，prompt 开销大）。 |
| `codex` | `codex exec`（需登录）。 |
| `echo` | 离线脚本，不调 API，跑测试用。 |

## 配置

中转站配置读取优先级（`direct`/`pi`/`planMission` 及全部测试脚本共用 `src/harness.js` 的 `loadDirectConfig()`）：

1. 显式入参（`--api-base` / `--api-key`）；
2. 环境变量：`SWARM_API_BASE` / `SWARM_API_KEY`（推荐，swarm 专用），回退 `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`；模型名 `SWARM_MODEL`；
3. `~/.swarm/api.json`：`{ "baseUrl": "...", "apiKey": "...", "model": "..." }`；
4. `~/.claude/settings.json` 的 `env` 段。

> 中转站模型名需带 `cn:` 前缀；鉴权同时发 `x-api-key` 与 `Authorization: Bearer`（兼容两种鉴权方式）。

**凭据安全**：key/地址只从上述来源读取，**绝不写进代码或测试脚本**。推荐把凭据放在 `~/.swarm/api.json` 或环境变量里；一旦怀疑泄露，立刻在中转站轮换 key，然后更新 `~/.swarm/api.json` 即可，无需改任何代码。`.gitignore` 已排除 `.swarm/` 与凭据类文件。

## 状态与文件布局

- 状态库：`<repoDir>/.swarm/state.db`（SQLite）。表：`work_items` / `claims` / `submissions` / `integrations` / `context` / `messages` / `events` / `kv`（持久化续跑元数据）。
- 集成分支：`swarm/integration`；每个 worker 一个 git worktree + 分支，退场后由 reaper 回收。

## HTTP API

协调服务默认绑 `127.0.0.1:17771`，仅本机访问（Host/Origin 校验，可选 `X-Swarm-Token`）。

- `GET /health` `/dashboard` `/status` `/board` `/item` `/context/search` `/digest` `/messages` `/events` `/tree` `/stats`
- `POST /claim` `/renew` `/release` `/context` `/message` `/item` `/submit`

## 测试

```bash
node test/run_tests.js          # 全部：offline_swarm + protocol + api
npm run test:offline            # 离线端到端（echo harness，不调 API）
npm run test:protocol           # claim/租约/门禁/autoscaler/reaper/拆解幂等
npm run test:live               # 真实 API 联调（需配置中转站）
```

## 源码导览

| 文件 | 职责 |
| --- | --- |
| [cli.js](cli.js) | 入口：init / seed / run / board / status |
| [src/store.js](src/store.js) | 全部状态核心：board / 租约 / 提交 / 集成 / 上下文 / 事件 + 崩溃回放 |
| [src/runner.js](src/runner.js) | worker 生命周期 + 集成器 + autoscaler + reaper |
| [src/harness.js](src/harness.js) | 执行底座适配层（direct/pi/claude/codex/echo）+ API 闸门 + mission 规划 |
| [src/prompts.js](src/prompts.js) | worker prompt 协议（KV 缓存友好布局） |
| [src/gitutil.js](src/gitutil.js) | git worktree / 合并 / 门禁 / 崩溃恢复扫描 |
| [src/server.js](src/server.js) | 协调 HTTP API |
| [src/dashboard.js](src/dashboard.js) | 实时面板 |
