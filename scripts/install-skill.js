#!/usr/bin/env node
// scripts/install-skill.js — 一键安装 swarm skill 到 Claude Code

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SWARM_ROOT = path.resolve(__dirname, '..');

const COLORS = {
  reset: '\x1b[0m',
  green: '\x1b[32m',
  yellow: '\x1b[33m',
  red: '\x1b[31m',
  cyan: '\x1b[36m',
};

function log(msg, color = 'reset') {
  console.log(`${COLORS[color]}${msg}${COLORS.reset}`);
}

function checkmark(ok) {
  return ok ? '✓' : '✗';
}

// 1. 检测 Claude Code skills 目录
function getClaudeSkillsDir() {
  const home = os.homedir();
  // Claude Code 在 Windows 上是 %USERPROFILE%\.config\claude-code\skills
  // 在 macOS/Linux 上是 ~/.config/claude-code/skills
  return path.join(home, '.config', 'claude-code', 'skills');
}

// 2. 检查 swarm 依赖
function checkDependencies() {
  const nm = path.join(SWARM_ROOT, 'node_modules', 'better-sqlite3');
  return fs.existsSync(nm);
}

// 3. 检查 API 配置
function checkApiConfig() {
  // 环境变量
  if (process.env.SWARM_API_BASE && process.env.SWARM_API_KEY) return { ok: true, source: 'env' };
  if (process.env.ANTHROPIC_BASE_URL && (process.env.ANTHROPIC_AUTH_TOKEN || process.env.ANTHROPIC_API_KEY)) {
    return { ok: true, source: 'env (ANTHROPIC_*)' };
  }
  // 配置文件
  const apiJson = path.join(os.homedir(), '.swarm', 'api.json');
  if (fs.existsSync(apiJson)) return { ok: true, source: apiJson };
  return { ok: false };
}

// 4. 生成 skill 文件内容
function skillTemplate(swarmPath) {
  // 统一用正斜杠（Claude Code 在 Windows 也接受）
  const normalized = swarmPath.replace(/\\/g, '/');
  return `---
name: swarm
description: 去中心化多 agent 并行开发。把大任务拆给 N 个 agent 并行完成，自动拆解、集成、测试、合并代码。
---

# swarm skill

当用户要求以下任一操作时，调用此 skill：
- "并行开发" / "多 agent" / "拆分任务" / "swarm"
- "用 swarm 实现 XXX" / "让 swarm 做 XXX"
- "把 XXX 拆给多个 agent 并行做"
- "调用 swarm 帮我完成 XXX"

## 功能说明

swarm 是一个**去中心化多 agent 软件开发编排器**，核心能力：

1. **任务拆解**：把一句话 mission（如"实现一个 TODO CLI"）自动拆成 N 个子任务（默认 6 个）
2. **并行执行**：拉起 N 个独立 worker（默认 8 个，最多 1000 个），每个 worker 自己认领任务、写代码、跑测试
3. **自动集成**：worker 提交代码后，集成器自动 merge 到主分支，跑测试门禁，失败自动回滚
4. **冲突处理**：
   - scratch 文件拦截（拒绝 \`probe.*\` / \`tmp_*\` / \`*.log\` 等调试文件）
   - 文本清单合并（\`LICENSE\` / \`.gitignore\` 等白名单文件的 add/add 冲突自动合并）
   - LLM 三路仲裁（源码冲突时让 LLM 裁决合并）
5. **崩溃续跑**：中途崩溃后重新 \`run <repoDir>\`（不传参数）自动从断点续跑
6. **实时监控**：跑到 \`http://127.0.0.1:17771/dashboard\` 看进度 / 树状分解 / token 用量 / 集成记录
7. **错误观测**：worker 崩溃按类型聚合（network / rate_limit / timeout 等），\`status\` 命令查看 \`crashStats\`

## 首次使用前检查（必须执行）

在调用任何 swarm 命令前，**必须先检查安装状态**：

\`\`\`bash
# 1. 检查 swarm 仓库是否存在
ls "${normalized}/cli.js"

# 2. 检查依赖是否安装
ls "${normalized}/node_modules/better-sqlite3"

# 3. 检查 API 配置（三选一）
# - 环境变量：SWARM_API_BASE + SWARM_API_KEY + SWARM_MODEL
# - 配置文件：~/.swarm/api.json
# - 命令行参数：--api-base + --api-key + --model
\`\`\`

**如果任何检查失败**：
- 依赖未装：\`cd "${normalized}" && npm install\`
- API 未配：引导用户按 README 配置（环境变量 / ~/.swarm/api.json / 命令行参数）
- 仓库不存在：提示用户先克隆 swarm 仓库

## 用法

用户提供：
- \`repoDir\`：目标项目目录（绝对路径，如 \`C:/Users/zs/projects/my-todo\`）
- \`mission\`：任务描述（一句话，如"实现一个 TODO CLI"）
- \`testCmd\`（可选）：集成门禁测试命令（如 \`npm test\` / \`node --test test/*.test.js\`）
- \`maxWorkers\`（可选）：worker 数上限（默认 8，范围 1-1000）
- \`count\`（可选）：任务拆解数量（默认 6，范围 3-20）

## 执行步骤

### 1. 检查安装（首次必须）

\`\`\`bash
ls "${normalized}/cli.js" && ls "${normalized}/node_modules"
\`\`\`

如果失败，先执行：
\`\`\`bash
cd "${normalized}" && npm install
\`\`\`

### 2. 初始化目标仓库

\`\`\`bash
node "${normalized}/cli.js" init <repoDir>
\`\`\`

这会：
- \`git init\`（如果还不是 git 仓库）
- 创建集成分支 \`swarm/integration\`
- 创建 \`.swarm/\` 目录（存放 SQLite 状态库）

### 3. 播种任务

\`\`\`bash
node "${normalized}/cli.js" seed <repoDir> --mission "<mission>" --count <count>
\`\`\`

这会让 LLM 把 mission 拆成 \`<count>\` 个顶层任务（默认 6 个）。

**手写 seed（可选）**：如果想精确控制任务拆解，创建 \`seed.json\`：
\`\`\`json
[
  { "title": "package.json", "description": "创建 package.json", "priority": 10 },
  { "title": "src/index.js", "description": "主入口", "priority": 9 },
  { "title": "src/cli.js", "description": "CLI 解析", "priority": 8 }
]
\`\`\`
然后：
\`\`\`bash
node "${normalized}/cli.js" seed <repoDir> --file seed.json
\`\`\`

### 4. 开跑

\`\`\`bash
node "${normalized}/cli.js" run <repoDir> --harness direct \\
  --mission "<mission>" \\
  --test-cmd "<testCmd>" \\
  --max-workers <maxWorkers> \\
  --exit-when-done
\`\`\`

**参数说明**：
- \`--harness direct\`：直连 API（成本最低，推荐）
- \`--exit-when-done\`：跑完自动退出
- \`--max-workers 8\`：worker 池上限（autoscaler 会按需伸缩）
- \`--test-cmd "npm test"\`：每次集成前跑测试，失败自动回滚

### 5. 监控

跑到 \`http://127.0.0.1:17771/dashboard\` 看实时面板：
- 进度（done / total）
- 树状分解（任务拆解层级）
- token 用量（每个 worker 消耗的 tokens）
- 集成记录（merge 历史 / 冲突处理）
- 崩溃统计（\`crashStats\` 按错误类型聚合）

### 6. 查看状态（可选）

\`\`\`bash
# 查看任务列表
node "${normalized}/cli.js" board <repoDir>

# 查看汇总状态（进度 / 活跃租约 / 待集成提交 / 崩溃统计）
node "${normalized}/cli.js" status <repoDir>
\`\`\`

## 完整示例

用户输入：
\`\`\`
/swarm 帮我实现一个 TODO CLI，项目目录是 C:/Users/zs/projects/my-todo
\`\`\`

Claude Code 执行：
\`\`\`bash
# 1. 检查安装
ls "${normalized}/cli.js" && ls "${normalized}/node_modules"

# 2. 初始化
node "${normalized}/cli.js" init "C:/Users/zs/projects/my-todo"

# 3. 播种（默认拆 6 个任务）
node "${normalized}/cli.js" seed "C:/Users/zs/projects/my-todo" --mission "实现一个 TODO CLI" --count 6

# 4. 开跑（默认 8 workers）
node "${normalized}/cli.js" run "C:/Users/zs/projects/my-todo" --harness direct \\
  --mission "实现一个 TODO CLI" \\
  --test-cmd "npm test" \\
  --max-workers 8 \\
  --exit-when-done

# 5. 提示用户打开 http://127.0.0.1:17771/dashboard 看实时监控
\`\`\`

## 运行时安全（必须遵守，违反会拖垮用户机器）

- **绝对不要给 swarm 加外部看门狗**（计划任务 / \`schtasks\` / 循环脚本重启）。
  swarm 自己管 worker 生命周期；进程被杀就是该停。外部保活会让崩溃循环反复重建 worker 池
  ——真实事故：某次用计划任务每 2 分钟重启 12-worker 池，把整台机器 CPU/内存耗干卡死。
- **必须带 \`--exit-when-done\`**，并把 \`run\` 当成需要监控的长任务；不要让它无人值守无限跑。
- **凭据不进命令行**：\`--api-key sk-xxx\` 会出现在进程列表、shell 历史和日志里。
  请用 \`~/.swarm/api.json\` 或 \`SWARM_API_BASE\` / \`SWARM_API_KEY\` 环境变量。
- **不要盲目调高并发**：\`--max-workers\` 缺省已按机器核数自适应（\`max(2, 核数/2)\`）；
  系统资源护栏默认开启（CPU>80% 停扩、>92% 缩容），不要用 \`--no-resource-guard\` 绕过。
- 用户抱怨"电脑卡死/进程反复复活"时的处置顺序：
  1. \`tasklist | findstr node\` 找到命令行含 \`swarm/cli.js run\` 的进程，连同它的 \`cmd.exe\` 父进程一起结束；
  2. 查 \`schtasks /query\` 与启动项，删除任何保活 swarm 的计划任务；
  3. 确认 \`.swarm/watchdog.ps1\` 之类的脚本不会被再次拉起。

## 注意事项

- **跑完后自动退出**（\`--exit-when-done\`），输出 \`[swarm] DONE {json}\`
- **崩溃续跑**：中途崩溃后重新 \`node "${normalized}/cli.js" run <repoDir>\`（不传参数）自动从断点续跑
- **首次使用前必须检查安装**（见上方"首次使用前检查"）
- **集成冲突自动处理**：
  - scratch 文件拦截：拒绝 \`probe.*\` / \`tmp_*\` / \`*.log\` / \`scratch/\` 等调试文件
  - 文本清单合并：\`LICENSE\` / \`.gitignore\` / \`.npmrc\` 等白名单文件的 add/add 冲突自动行级合并
  - LLM 三路仲裁：源码冲突时把 base/ours/theirs 发给 LLM 裁决，输出必须通过语法校验
- **任务发散护栏**：总任务数达到 200 后停止 auto-split（\`--max-items\` 覆盖）
- **错误观测**：\`status\` 命令的 \`crashStats\` 字段显示按类型聚合的崩溃统计（network / rate_limit / timeout 等）
- **成本估算**：实测 100 workers 跑中等 mission（39 任务、296 测试）约 600M tokens

## 常见问题

**Q: 跑一半崩溃了怎么办？**  
A: 直接 \`node "${normalized}/cli.js" run <repoDir>\`（不传任何参数），自动从上次断点续跑。

**Q: 怎么知道 worker 为什么挂了？**  
A: \`node "${normalized}/cli.js" status <repoDir>\` 看 \`crashStats\` 字段，按错误类型聚合（network / rate_limit / timeout 等）。

**Q: 集成冲突太多怎么办？**  
A: swarm 会自动处理三类冲突（scratch 拦截 / 文本合并 / LLM 仲裁），如果还是频繁失败，检查 mission 拆解粒度是否太细。

**Q: 成本多少？**  
A: 实测 100 workers 跑一个中等 mission（39 任务、296 测试）约 600M tokens，按中转站定价约 ¥500-2000。

**Q: 支持多少 workers？**  
A: 当前架构（单进程 Node）实测稳定支持 100-200 workers，500+ 需要 worker_threads 改造（见 docs/RESOURCE_ISOLATION.md）。
`;
}

// 主流程
async function main() {
  log('\n=== swarm skill 安装 ===\n', 'cyan');

  // 1. 检查 swarm 仓库
  log(`swarm 仓库路径: ${SWARM_ROOT}`);
  const cliExists = fs.existsSync(path.join(SWARM_ROOT, 'cli.js'));
  log(`  ${checkmark(cliExists)} cli.js`, cliExists ? 'green' : 'red');
  if (!cliExists) {
    log('\n错误：当前目录不是 swarm 仓库根目录', 'red');
    process.exit(1);
  }

  // 2. 检查依赖
  const depsOk = checkDependencies();
  log(`  ${checkmark(depsOk)} node_modules/better-sqlite3`, depsOk ? 'green' : 'yellow');
  if (!depsOk) {
    log('\n依赖未安装，正在执行 npm install...', 'yellow');
    try {
      execSync('npm install', { cwd: SWARM_ROOT, stdio: 'inherit' });
      log('✓ 依赖安装完成', 'green');
    } catch (e) {
      log(`✗ npm install 失败: ${e.message}`, 'red');
      process.exit(1);
    }
  }

  // 3. 检查 API 配置
  const apiCfg = checkApiConfig();
  log(`  ${checkmark(apiCfg.ok)} API 配置${apiCfg.ok ? ` (来源: ${apiCfg.source})` : ''}`, apiCfg.ok ? 'green' : 'yellow');
  if (!apiCfg.ok) {
    log('\n警告：未检测到 API 配置。首次跑 mission 前请先配置：', 'yellow');
    log('  方式 1: 设置环境变量 SWARM_API_BASE / SWARM_API_KEY / SWARM_MODEL');
    log(`  方式 2: 创建 ${path.join(os.homedir(), '.swarm', 'api.json')}，内容 {"baseUrl":"...","apiKey":"...","model":"..."}`);
    log('  方式 3: 每次 run 时传 --api-base / --api-key / --model\n');
  }

  // 4. 写 skill 文件
  const skillsDir = getClaudeSkillsDir();
  fs.mkdirSync(skillsDir, { recursive: true });
  const skillPath = path.join(skillsDir, 'swarm.md');
  fs.writeFileSync(skillPath, skillTemplate(SWARM_ROOT), 'utf8');
  log(`\n✓ skill 已写入: ${skillPath}`, 'green');

  // 5. 完成提示
  log('\n=== 安装完成 ===\n', 'cyan');
  log('在 Claude Code 中使用：', 'cyan');
  log('  /swarm 帮我实现一个 TODO CLI，项目目录是 /path/to/project\n');
  log('首次调用时 Claude Code 会自动执行"首次使用前检查"。\n');
}

main().catch((e) => {
  log(`\n安装失败: ${e.message}`, 'red');
  process.exit(1);
});
