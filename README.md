# swarm — 去中心化多 agent 软件开发编排器

把"写码 + 审核 + 优化"的软件开发工作，拆给 N 个互相独立、无中央调度器的 agent 并行完成。
目标：在**墙钟时间最短**的前提下，用 200~1000 个 agent 替代单个主 agent 的慢速串行体验。

架构基座来自微软 Agensh 论文（arXiv:2609.26781v1）的自组织多 agent harness：
共享 workspace + 消息接口 + append-only 共享上下文数据库，配合"五步合作循环"。

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
| `--max-inflight N` | `64` | 全局在飞 API 请求上限（见下） |
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
进程级信号量把"在飞 API 请求数"钳在上限内（默认 64，`--max-inflight` / `SWARM_MAX_INFLIGHT` 覆盖，`0` = 不限流）。

### 崩溃恢复

- `_replay()`：把 append-only 事件流折叠成每个工作项的应然状态，与派生表比对；只做**单向、不丢工作**的修正
  （事件说 done 而表里没 done → 补 done），绝不把 done 退回 pending。
- `releaseOrphanClaims()`：`run` 启动时（尚无 worker 存活）把上次进程崩溃留下的活跃租约全部作废，
  否则对应项要干等租约自然过期才能被重认领。
- `recoverIntegrations()`：扫集成分支的 `Agensh-Integration:` trailer，补回"已 merge 但崩溃在落库之前"的集成记录。

> 恢复只在 `run` 启动时执行，刻意不放进 Store 构造函数——`status`/`board` 会打开同一个库，放构造函数会误杀正在跑的租约。

### 省 token（KV / 前缀缓存友好）

worker prompt 的正文段对所有 worker **逐字节相同**（身份/端口/token 用 `{PEER}`/`{API}` 占位符），
易变内容按易变度从低到高排在尾部：`共享正文 → IDENTITY(每 worker 固定) → TURN BRIEF(每回合变) → turn 号`。
于是跨 worker 共享整段正文前缀，同一 worker 跨回合共享到 IDENTITY 为止。实测 w1/w2 公共前缀 99.1%，w1 跨回合 99.8%。

### 空转抑制

worker 手头无租约、board 也无 claimable 项时退避轮询，不烧 LLM 回合；冷启动（`total === 0`）豁免。

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

- 状态库：`<repoDir>/.swarm/state.db`（SQLite）。表：`work_items` / `claims` / `submissions` / `integrations` / `context` / `messages` / `events`。
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
